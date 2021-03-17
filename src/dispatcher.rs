
use futures::prelude::*;

use async_std::net::ToSocketAddrs;
use async_std::path::Path;

use crate::rustbus_core;
use rustbus_core::message_builder::{MarshalledMessage, MessageType};

use crate::RpcConn;
use crate::conn;
use conn::{DBusAddr, get_session_bus_addr, get_system_bus_path};


pub struct DispatcherConn {
    conn: RpcConn
}

/*
impl DispatcherConn {
    pub async fn session_conn(with_fd: bool) -> std::io::Result<Self> {
        let addr = get_session_bus_addr().await?;
        Self::connect_to_addr(&addr, with_fd).await
    }
    pub async fn system_conn(with_fd: bool) -> std::io::Result<Self> {
        let path = get_system_bus_path().await?;
        Self::connect_to_path(&path, with_fd).await
    }
    pub async fn connect_to_addr<P: AsRef<Path>, S: ToSocketAddrs>(
        addr: &DBusAddr<P, S>,
        with_fd: bool,
    ) -> std::io::Result<Self> {
        let conn = RpcConn::connect_to_addr(addr, with_fd).await?;
        Ok(Self { conn } )
    }
    pub async fn connect_to_path<P: AsRef<Path>>(path: P, with_fd: bool) -> std::io::Result<Self> {
        let conn = RpcConn::connect_to_path(path, with_fd).await?;
        Ok(Self { conn } )
    }
    pub async fn send_message<'a>(
        &'a self,
        msg: &MarshalledMessage,
    ) -> std::io::Result<impl Future<Output = std::io::Result<Option<MarshalledMessage>>> + 'a> {
        self.conn.send_message(msg).await
    }
    // pub async fn insert_handler(path: &ObjectPath) {}
}
*/
