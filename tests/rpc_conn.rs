use async_rustbus::conn::DBusAddr;
use async_rustbus::rustbus_core::message_builder::{
    MarshalledMessage, MessageBuilder, MessageType,
};
use async_rustbus::RpcConn;
use async_std::future::{timeout, TimeoutError};
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
