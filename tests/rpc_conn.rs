use async_rustbus::conn::DBusAddr;
use async_rustbus::rustbus_core::message_builder::{
    MarshalledMessage, MessageBuilder, MessageType,
};
use async_rustbus::RpcConn;
use futures::future::{select, Either, try_join_all, ready};
use futures::prelude::*;
use futures::future::Future;
use futures::task::{Context, Poll};
use async_std::future::{timeout, TimeoutError};
use std::pin::Pin;
use std::process::id;
use std::time::Duration;

const DBUS_NAME: &'static str = "io.maves.dbus";

const DEFAULT_TO: Duration = Duration::from_secs(1);

#[derive(Debug)]
enum TestingError {
    Io(std::io::Error),
    Bad(MarshalledMessage),
    Timeout(TimeoutError),
}
impl From<std::io::Error> for TestingError {
    fn from(err: std::io::Error) -> Self {
        TestingError::Io(err)
    }
}
impl From<TimeoutError> for TestingError {
    fn from(err: TimeoutError) -> Self {
        TestingError::Timeout(err)
    }
}

#[async_std::test]
async fn session_wo_fd() -> std::io::Result<()> {
    RpcConn::session_conn(false).await?;
    Ok(())
}

#[async_std::test]
async fn session_w_fd() -> std::io::Result<()> {
    RpcConn::session_conn(true).await?;
    Ok(())
}

#[async_std::test]
async fn system_wo_fd() -> std::io::Result<()> {
    RpcConn::system_conn(false).await?;
    Ok(())
}

#[async_std::test]
async fn system_w_fd() -> std::io::Result<()> {
    RpcConn::system_conn(true).await?;
    Ok(())
}

#[async_std::test]
#[ignore]
async fn tcp_wo_fd() -> std::io::Result<()> {
    let addr: DBusAddr<&str, &str> = DBusAddr::Tcp("localhost:29011");
    RpcConn::connect_to_addr(&addr, false).await?;
    Ok(())
}

#[async_std::test]
async fn tcp_w_fd() -> std::io::Result<()> {
    let addr: DBusAddr<&str, &str> = DBusAddr::Tcp("localhost:29011");
    assert!(RpcConn::connect_to_addr(&addr, true).await.is_err());
    Ok(())
}

#[async_std::test]
async fn get_name() -> Result<(), TestingError> {
    use rustbus::standard_messages::request_name;
    let conn = RpcConn::session_conn(false).await?;
    let msg_fut = timeout(
        DEFAULT_TO,
        conn.send_message(&request_name(DBUS_NAME.to_string(), 0)),
    )
    .await??;
    println!("Name Request sent");
    let msg = timeout(DEFAULT_TO, msg_fut).await??.unwrap();
    match &msg.typ {
        MessageType::Reply => {}
        _ => return Err(TestingError::Bad(msg)),
    }
    let mut call = MessageBuilder::new()
        .call(String::from("GetConnectionUnixProcessID"))
        .with_interface(String::from("org.freedesktop.DBus"))
        .on(String::from("/org/freedesktop/DBus"))
        .at(String::from("org.freedesktop.DBus"))
        .build();

    call.body.push_param(DBUS_NAME).unwrap();
    let res_fut = timeout(DEFAULT_TO, conn.send_message(&call)).await??;
    let res = timeout(DEFAULT_TO, res_fut).await??.unwrap();
    match &res.typ {
        MessageType::Reply => {
            let i: u32 = res.body.parser().get().unwrap();
            assert_eq!(i, id());
        }
        _ => return Err(TestingError::Bad(msg)),
    }
    call.dynheader.member = Some(String::from("GetNameOwner"));
    let res_fut = timeout(DEFAULT_TO, conn.send_message(&call)).await??;
    let res = timeout(DEFAULT_TO, res_fut).await??.unwrap();
    match &res.typ {
        MessageType::Reply => {
            let name: &str = res.body.parser().get().unwrap();
            assert_eq!(name, conn.get_name());
            Ok(())
        }
        _ => Err(TestingError::Bad(msg)),
    }
}

#[async_std::test]
async fn get_mach_id() -> Result<(), TestingError> {
    let conn = RpcConn::session_conn(false).await?;
    let call = MessageBuilder::new()
        .call(String::from("GetMachineId"))
        .with_interface(String::from("org.freedesktop.DBus.Peer"))
        .on(String::from("/org/freedesktop/DBus"))
        .at(String::from("org.freedesktop.DBus"))
        .build();

    let msg = conn.send_message(&call).await?.await?.unwrap();
    match &msg.typ {
        MessageType::Reply => {
            let _s: &str = msg.body.parser().get().unwrap();
            Ok(())
        }
        _ => Err(TestingError::Bad(msg)),
    }
}

#[async_std::test]
#[ignore]
async fn no_recv_deadlock() -> Result<(), TestingError> {
    let conn = RpcConn::session_conn(false).await?;
    let mut call = MessageBuilder::new()
        .call(String::from("Echo"))
        .with_interface(String::from("org.freedesktop.DBus.Testing"))
        .on(String::from("/"))
        .at(String::from("io.maves.LongWait"))
        .build();
    println!("no_recv_deadlock() stage1: send messages");
    let long_fut = conn.send_message(&call).await?;
    call.dynheader.destination = Some(String::from("io.maves.ShortWait"));
    let short_fut= conn.send_message(&call).await?;
    println!("no_recv_deadlock() stage1: wait on first messages.");
    let long_fut = match select(long_fut, short_fut).await {
        Either::Right((short_res, long_fut)) => {
            let short_res = short_res?.unwrap();
            match short_res.typ {
                MessageType::Reply => long_fut,
                _ => return Err(TestingError::Bad(short_res))
            }
        },
        Either::Left((long_res, _)) => {
            panic!("The long_res returned first: {:?}", long_res);
        }
    };
    println!("no_recv_deadlock() stage1: send second set messages.");
    let mut calls = Vec::with_capacity(25);
    for _ in 0u8..24 {
        calls.push(conn.send_message(&call).await?);
    }
    eprintln!("no_recv_deadlock() stage1: wait for responses.");
    let mut responses = try_join_all(calls).await?;
    eprintln!("no_recv_deadlock() stage1: wait for final responses.");
    responses.push(long_fut.await?);
    for response in responses {
        let res = response.unwrap();
        match res.typ {
            MessageType::Reply => {}
            _ => {},
        }
    }
    Ok(())
}

/// Some tests rely on the left part of a select()-call always being
/// polled, first. While most implementations do this, we need to ensure,
/// this behavior remains.
#[async_std::test]
async fn select_left_priority() {
struct PollCounter {
   cnt: usize
}
impl PollCounter {
    fn new() -> Self { Self { cnt: 0 } }
    fn get_count(&self) -> usize { self.cnt }
}
impl Future for PollCounter {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.cnt += 1;
        Poll::Pending
    }
}
    let mut counter = PollCounter::new();
    for i in 1..=32 {
        counter = match select(counter, ready(())).await {
        Either::Right((_, c)) => c,
        _ => unreachable!()
    };
    assert_eq!(counter.get_count(), i);
    }
}


