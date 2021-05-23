use std::io::ErrorKind;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use std::pin::Pin;
use std::process::id;
use std::time::{Duration, Instant};

use async_std::future::{timeout, TimeoutError};
use async_std::os::unix::net::UnixStream;
use futures::future::Future;
use futures::future::{ready, select, try_join, try_join_all, Either};
use futures::pin_mut;
use futures::prelude::*;
use futures::task::{Context, Poll};

use async_rustbus::conn::DBusAddr;
use async_rustbus::rustbus_core;
use async_rustbus::rustbus_core::wire::unixfd::UnixFd;
use async_rustbus::CallAction;
use async_rustbus::{MatchRule, RpcConn, EMPTY_MATCH};
use rustbus_core::message_builder::{MarshalledMessage, MessageBuilder, MessageType};

use rustbus_core::wire::unmarshal::traits::Unmarshal;

const DBUS_NAME: &str = "io.test.dbus";

const DEFAULT_TO: Duration = Duration::from_secs(1);

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
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
    let addr = DBusAddr::tcp_addr("localhost:29011");
    RpcConn::connect_to_addr(&addr, false).await?;
    Ok(())
}

#[async_std::test]
#[ignore]
async fn tcp_w_fd() -> std::io::Result<()> {
    let addr = DBusAddr::tcp_addr("localhost:29011");
    assert!(RpcConn::connect_to_addr(&addr, true).await.is_err());
    Ok(())
}

#[async_std::test]
async fn get_name() -> Result<(), TestingError> {
    use rustbus_core::standard_messages::request_name;
    let conn = RpcConn::session_conn(false).await?;
    let msg_fut = timeout(DEFAULT_TO, conn.send_msg(&request_name(DBUS_NAME, 0)))
        .await??
        .unwrap();
    println!("Name Request sent");
    let msg = timeout(DEFAULT_TO, msg_fut).await??;
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
    let res_fut = timeout(DEFAULT_TO, conn.send_msg(&call)).await??.unwrap();
    let res = timeout(DEFAULT_TO, res_fut).await??;
    match &res.typ {
        MessageType::Reply => {
            let i: u32 = res.body.parser().get().unwrap();
            assert_eq!(i, id());
        }
        _ => return Err(TestingError::Bad(msg)),
    }
    call.dynheader.member = Some(String::from("GetNameOwner"));
    let res_fut = timeout(DEFAULT_TO, conn.send_msg(&call)).await??.unwrap();
    let res = timeout(DEFAULT_TO, res_fut).await??;
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

    let msg = conn.send_msg(&call).await?.unwrap().await?;
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
async fn no_recv_deadlock_overcut() -> Result<(), TestingError> {
    let conn = RpcConn::session_conn(false).await?;
    let mut call = MessageBuilder::new()
        .call(String::from("Echo"))
        .with_interface(String::from("org.freedesktop.DBus.Testing"))
        .on(String::from("/"))
        .build();
    for i in 0..5 {
        println!("no_recv_deadlock() iteration {}", i);
        call.dynheader.destination = Some(String::from("io.test.LongWait"));
        let long_fut = conn.send_msg(&call).await?.unwrap();
        call.dynheader.destination = Some(String::from("io.test.ShortWait"));
        let short_fut = conn.send_msg(&call).await?.unwrap();
        println!(
            "no_recv_deadlock() iteration {}: awaiting first short res",
            i
        );
        let long_fut = match select(long_fut, short_fut).await {
            Either::Right((short_res, long_fut)) => {
                let short_res = short_res?;
                match short_res.typ {
                    MessageType::Reply => long_fut,
                    _ => return Err(TestingError::Bad(short_res)),
                }
            }
            Either::Left((long_res, _)) => {
                panic!("The long_res returned first: {:?}", long_res);
            }
        };
        println!(
            "no_recv_deadlock() iteration {}: awaiting first short res",
            i
        );
        let mut calls = Vec::with_capacity(501);
        for _ in 0u16..16 {
            calls.push(conn.send_msg(&call).await?.unwrap());
        }
        eprintln!("no_recv_deadlock() stage1: wait for responses.");
        let mut responses = try_join_all(calls).await?;
        eprintln!("no_recv_deadlock() stage1: wait for final responses.");
        responses.push(long_fut.await?);
        for res in responses {
            is_msg_reply(res)?;
        }
    }
    Ok(())
}

#[async_std::test]
#[ignore]
async fn no_recv_deadlock_undercut() -> Result<(), TestingError> {
    let conn = RpcConn::session_conn(false).await?;
    let mut call = MessageBuilder::new()
        .call(String::from("Echo"))
        .with_interface(String::from("org.freedesktop.DBus.Testing"))
        .on(String::from("/"))
        .build();
    for i in 0u8..5 {
        println!(
            "no_recv_deadlock_under(): iteration {}, sending messages",
            i
        );
        call.dynheader.destination = Some(String::from("io.test.LongWait"));
        let sent_inst = Instant::now();
        let long_fut = conn.send_msg(&call).await?.unwrap();
        call.dynheader.destination = Some(String::from("io.test.NoWait"));
        let short_fut = conn.send_msg(&call).await?.unwrap();
        pin_mut!(long_fut);
        pin_mut!(short_fut);
        println!(
            "no_recv_deadlock_under(): iteration {}, awaiting responses",
            i
        );
        let to = Duration::from_secs(1)
            .checked_sub(sent_inst.elapsed())
            .unwrap_or_default();
        match timeout(to, select(long_fut, short_fut)).await? {
            Either::Right((short_res, long_fut)) => {
                println!("no_recv_deadlock_under(): iteration {}: first recvd", i);
                is_msg_reply(short_res?)?;
                let long_res = timeout(Duration::from_millis(2000), long_fut).await??;
                is_msg_reply(long_res)?;
            }
            Either::Left(_) => panic!("long_fut finished before the short_fut"),
        }
    }
    Ok(())
}
#[async_std::test]
async fn fd_send_recv() -> Result<(), TestingError> {
    let (conn, recv_conn) =
        try_join(RpcConn::session_conn(true), RpcConn::session_conn(true)).await?;
    recv_conn
        .insert_call_path("/", CallAction::Queue)
        .await
        .unwrap();

    let mut call = MessageBuilder::new()
        .call(String::from("Echo"))
        .with_interface(String::from("org.freedesktop.DBus.Testing"))
        .on(String::from("/"))
        .at(String::from(recv_conn.get_name()))
        .build();

    let (mut ours, theirs) = UnixStream::pair()?;
    call.body
        .push_param(UnixFd::new(theirs.into_raw_fd()))
        .unwrap();
    println!("fd_send_recv(): sending first msg");
    let res_fut = conn.send_msg(&call).await?.unwrap();

    println!("fd_send_recv(): get first msg");
    let mut call_msg = recv_conn.get_call("/").await?;
    call_msg.typ = MessageType::Reply;
    call_msg.dynheader.response_serial = call_msg.dynheader.serial;
    call_msg.dynheader.serial = None;
    call_msg.dynheader.destination = Some(String::from(conn.get_name()));
    println!("fd_send_recv(): sending first response");
    assert!(recv_conn.send_msg(&call_msg).await?.is_none());

    println!("fd_send_recv(): getting first response");
    let res = res_fut.await?;
    println!("Response sig: {:?}", res.dynheader.signature);

    let unix_fd: UnixFd = res.body.parser().get().unwrap();
    let mut theirs = unsafe { UnixStream::from_raw_fd(unix_fd.take_raw_fd().unwrap()) };
    theirs.write_all(b"Hello World!").await?;
    let mut buf = [0; 12];
    timeout(Duration::from_millis(50), ours.read_exact(&mut buf[..])).await??;
    Ok(())
}
#[async_std::test]
async fn send_fd_wo_fd_conn() -> Result<(), TestingError> {
    let conn = RpcConn::session_conn(false).await?;
    let mut call = MessageBuilder::new()
        .call(String::from("Echo"))
        .with_interface(String::from("org.freedesktop.DBus.Testing"))
        .on(String::from("/"))
        .at(String::from("io.test.NoWait"))
        .build();
    let (ours, theirs) = UnixStream::pair()?;
    call.body
        .push_param(UnixFd::new(theirs.into_raw_fd()))
        .unwrap();
    match conn.send_msg(&call).await {
        Err(e) if e.kind() == ErrorKind::InvalidInput => { /*this is what we want*/ }
        Err(e) => panic!("Received wrong error type: {:?}", e),
        Ok(_) => panic!("Message was send successfully when it should fail."),
    }
    drop(ours);
    Ok(())
}
#[async_std::test]
async fn no_send_deadlock_long() -> Result<(), TestingError> {
    let (conn, recv_conn) =
        try_join(RpcConn::session_conn(false), RpcConn::session_conn(false)).await?;
    recv_conn
        .insert_call_path("/", CallAction::Queue)
        .await
        .unwrap();

    let mut call = MessageBuilder::new()
        .call(String::from("Echo"))
        .with_interface(String::from("org.freedesktop.DBus.Testing"))
        .on(String::from("/"))
        .at(recv_conn.get_name().to_string())
        .build();
    call.body.push_param(vec![1u8; 16 * 1024 * 1024]).unwrap();
    let conn_name = conn.get_name().to_string();
    let handle = async_std::task::spawn(async move {
        for i in 0..8 {
            println!("no_send_deadlock_long(): other thread waiting for {}", i);
            let mut call_msg = recv_conn.get_call("/").await.unwrap();
            println!("no_send_deadlock_long(): other thread recvd {}", i);
            call_msg.typ = MessageType::Reply;
            call_msg.dynheader.response_serial = call_msg.dynheader.serial;
            call_msg.dynheader.serial = None;
            call_msg.dynheader.destination = Some(conn_name.clone());
            assert!(recv_conn.send_msg(&call_msg).await.unwrap().is_none());
        }
    });
    let mut res_futs = Vec::new();
    for i in 0..8 {
        println!("no_send_deadlock_long(): send iteration {}", i);
        let res_fut = timeout(Duration::from_millis(500), conn.send_msg(&call))
            .await??
            .unwrap();
        res_futs.push(res_fut);
    }
    handle.await;
    for response in try_join_all(res_futs).await? {
        is_msg_reply(response)?;
    }
    Ok(())
}

fn is_msg_reply(msg: MarshalledMessage) -> Result<(), TestingError> {
    match msg.typ {
        MessageType::Reply => Ok(()),
        _ => Err(TestingError::Bad(msg)),
    }
}
#[allow(dead_code)]
fn is_msg_bad(msg: MarshalledMessage) -> Result<(), TestingError> {
    match msg.typ {
        MessageType::Error => Ok(()),
        _ => Err(TestingError::Bad(msg)),
    }
}
fn is_msg_good<T>(msg: MarshalledMessage) -> Result<T, TestingError>
where
    for<'r> T: Unmarshal<'r, 'r>,
{
    let res: Result<T, _> = match msg.typ {
        MessageType::Reply => msg.body.parser().get(),
        _ => return Err(TestingError::Bad(msg)),
    };
    match res {
        Ok(v) => Ok(v),
        Err(_) => Err(TestingError::Bad(msg)),
    }
}

#[async_std::test]
async fn introspect() -> Result<(), TestingError> {
    let (conn, recv_conn) =
        try_join(RpcConn::session_conn(false), RpcConn::session_conn(false)).await?;
    recv_conn
        .insert_call_path("/", CallAction::Intro)
        .await
        .unwrap();
    recv_conn
        .insert_call_path("/usr/local/lib/libdbus", CallAction::Intro)
        .await
        .unwrap();
    recv_conn
        .insert_call_path("/usr/local/lib/libssl", CallAction::Intro)
        .await
        .unwrap();
    recv_conn
        .insert_call_path("/usr/local/bin/ls", CallAction::Intro)
        .await
        .unwrap();
    recv_conn
        .insert_call_path("/tmp", CallAction::Drop)
        .await
        .unwrap();
    recv_conn
        .insert_call_path("/tmp/log", CallAction::Queue)
        .await
        .unwrap();
    let mut intro = MessageBuilder::new()
        .call("Introspect".to_string())
        .at(recv_conn.get_name().to_string())
        .on("/".to_string())
        .with_interface("org.freedesktop.DBus.Introspectable".to_string())
        .build();
    let other = async_std::task::spawn(async move {
        println!("introspect(): spawned: Being await");
        let res = recv_conn.get_call("/tmp/log").await;
        unreachable!(
            "introspect(): spawned: unfinishable task finished: {:?}",
            res
        );
    });
    //async_std::task::sleep(Duration::from_secs(60)).await;
    let intro_str: String = is_msg_good(conn.send_msg_w_rsp(&intro).await?.await?)?;
    assert!(intro_str.contains("<node name=\"usr\"/>"));
    assert!(!intro_str.contains("<node name=\"tmp\"/>"));

    intro.dynheader.object = Some("/usr".to_string());
    let intro_str: String = is_msg_good(conn.send_msg_w_rsp(&intro).await?.await?)?;
    assert!(intro_str.contains("<node name=\"local\"/>"));

    intro.dynheader.object = Some("/usr/local".to_string());
    let intro_str: String = is_msg_good(conn.send_msg_w_rsp(&intro).await?.await?)?;
    assert!(intro_str.contains("<node name=\"lib\"/>"));
    assert!(intro_str.contains("<node name=\"bin\"/>"));

    intro.dynheader.object = Some("/usr/local/lib".to_string());
    let intro_str: String = is_msg_good(conn.send_msg_w_rsp(&intro).await?.await?)?;
    assert!(intro_str.contains("<node name=\"libdbus\"/>"));
    assert!(intro_str.contains("<node name=\"libssl\"/>"));

    intro.dynheader.object = Some("/usr/local/bin".to_string());
    let intro_str: String = is_msg_good(conn.send_msg_w_rsp(&intro).await?.await?)?;
    assert!(intro_str.contains("<node name=\"ls\"/>"));
    other.cancel().await;
    Ok(())
}
#[async_std::test]
async fn detect_hangup() -> Result<(), TestingError> {
    let conn = RpcConn::session_conn(false).await?;
    conn.insert_call_path("/", CallAction::Exact).await.unwrap();
    let fd = conn.as_raw_fd();

    println!("Writing bad buffer");
    let bad_buf = [0xFFu8; 32];
    let bad_ptr = bad_buf.as_ptr() as *const libc::c_void;
    let res = unsafe { libc::write(fd, bad_ptr, bad_buf.len()) };
    assert_eq!(res, 32);

    println!("Checking if hung up");
    let res = timeout(Duration::from_secs(1), conn.get_call("/")).await?;
    assert!(matches!(res, Err(_)));
    Ok(())
}

#[async_std::test]
async fn signal_send_and_receive() -> Result<(), TestingError> {
    let (conn, recv_conn) =
        try_join(RpcConn::session_conn(false), RpcConn::session_conn(false)).await?;

    let recv_dest = recv_conn.get_name();

    println!("Inserting signal matches");
    recv_conn.insert_sig_match(EMPTY_MATCH).await?;
    let mut m1 = MatchRule::new();
    m1.path("/io/test/specific");
    recv_conn.insert_sig_match(&m1).await?;

    let mut m2 = MatchRule::new();
    m2.path_namespace("/io/test");
    recv_conn.insert_sig_match(&m2).await?;

    let mut m3 = MatchRule::new();
    m3.interface("io.test.Test1");
    recv_conn.insert_sig_match(&m3).await?;

    let mut m4 = m3.clone();
    m4.member("TestSignal");
    recv_conn.insert_sig_match(&m4).await?;

    let mut m5 = m4.clone();
    m5.interface = None;
    recv_conn.insert_sig_match(&m5).await?;

    /*recv_conn
    .set_sig_filter(Box::new(|io| {
        io.dynheader
            .interface
            .as_ref()
            .unwrap()
            .starts_with("io.test")
    }))
    .await;*/

    let s_default = MessageBuilder::new()
        .signal("io.test.Test3", "TestSignal2", "/")
        .to(recv_dest)
        .build();
    println!("Sending signals");
    conn.send_msg_wo_rsp(&s_default).await?;

    let mut s1 = MessageBuilder::new()
        .signal("io.test.Test2", "TestSignal", "/io/test/specific")
        .build(); // build rs1
    conn.send_msg_wo_rsp(&s1).await?;
    s1.dynheader.object = Some("/io/test/other".into()); // rs2
    conn.send_msg_wo_rsp(&s1).await?;
    s1.dynheader.object = Some("/io/test".into()); // rs3
    conn.send_msg_wo_rsp(&s1).await?;
    s1.dynheader.object = Some("/io".into()); // rs4
    conn.send_msg_wo_rsp(&s1).await?;
    s1.dynheader.interface = Some("io.test.Test1".into()); // rs5
    conn.send_msg_wo_rsp(&s1).await?;
    s1.dynheader.member = Some("TestSignal2".into()); // rs6
    conn.send_msg_wo_rsp(&s1).await?;

    println!("Receiving signals");
    let rs1 = recv_conn.get_signal(&m1).await?.dynheader;
    let rs2 = recv_conn.get_signal(&m2).await?.dynheader;
    let rs3 = recv_conn.get_signal(&m2).await?.dynheader;
    let rs4 = recv_conn.get_signal(&m5).await?.dynheader;
    let rs5 = recv_conn.get_signal(&m4).await?.dynheader;
    let rs6 = recv_conn.get_signal(&m3).await?.dynheader;

    let mut found = false;
    while let Some(res) = recv_conn.get_signal(EMPTY_MATCH).now_or_never() {
        let rs_default = res?.dynheader;
        if rs_default.interface.as_deref() == Some("io.test.Test3")
            && rs_default.member.as_deref() == Some("TestSignal2")
            && rs_default.object.as_deref() == Some("/")
        {
            found = true;
            break;
        }
    }
    assert!(found);

    assert_eq!(rs1.interface.as_deref(), Some("io.test.Test2"));
    assert_eq!(rs1.member.as_deref(), Some("TestSignal"));
    assert_eq!(rs1.object.as_deref(), Some("/io/test/specific"));

    assert_eq!(rs2.interface.as_deref(), Some("io.test.Test2"));
    assert_eq!(rs2.member.as_deref(), Some("TestSignal"));
    assert_eq!(rs2.object.as_deref(), Some("/io/test/other"));

    assert_eq!(rs3.interface.as_deref(), Some("io.test.Test2"));
    assert_eq!(rs3.member.as_deref(), Some("TestSignal"));
    assert_eq!(rs3.object.as_deref(), Some("/io/test"));

    assert_eq!(rs4.interface.as_deref(), Some("io.test.Test2"));
    assert_eq!(rs4.member.as_deref(), Some("TestSignal"));
    assert_eq!(rs4.object.as_deref(), Some("/io"));

    assert_eq!(rs5.interface.as_deref(), Some("io.test.Test1"));
    assert_eq!(rs5.member.as_deref(), Some("TestSignal"));
    assert_eq!(rs5.object.as_deref(), Some("/io"));

    assert_eq!(rs6.interface.as_deref(), Some("io.test.Test1"));
    assert_eq!(rs6.member.as_deref(), Some("TestSignal2"));
    assert_eq!(rs6.object.as_deref(), Some("/io"));
    Ok(())
}

#[async_std::test]
#[should_panic]
async fn panic_on_bad_match() {
    let conn = RpcConn::session_conn(false).await.unwrap();
    let mut m = MatchRule::new();
    m.path = Some("/io/test/specific".into());
    m.path_namespace = Some("/io".into());
    conn.insert_sig_match(&m).await.unwrap();
}

/// Some tests rely on the left part of a select()-call always being
/// polled first, to be meaningful. While most implementations do this,
/// we need to ensure this behavior remains.
#[async_std::test]
async fn select_left_priority() {
    struct PollCounter {
        cnt: usize,
    }
    impl PollCounter {
        fn new() -> Self {
            Self { cnt: 0 }
        }
        fn get_count(&self) -> usize {
            self.cnt
        }
    }
    impl Future for PollCounter {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.cnt += 1;
            Poll::Pending
        }
    }
    let mut counter = PollCounter::new();
    for i in 1..=16 {
        counter = match select(counter, ready(())).await {
            Either::Right((_, c)) => c,
            _ => unreachable!(),
        };
        assert_eq!(counter.get_count(), i);
    }
}
