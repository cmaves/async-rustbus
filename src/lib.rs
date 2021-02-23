
use futures::Future;
use async_std::path::Path;

// BEGIN: rustbus core modules canidates
use rustbus::message_builder;
use rustbus::connection as sync_conn;
// END
use rustbus::connection::rpc_conn::MessageFilter;
use message_builder::MarshalledMessage;

pub mod conn;

use conn::{get_session_bus_path, get_system_bus_path};

pub struct RpcConn {
}

impl RpcConn {
    pub fn new(/*what goes here*/) -> Self {
        unimplemented!()
    }
    pub async fn session_conn() -> Result<Self, sync_conn::Error> {
        let path = get_session_bus_path().await?;
        Self::connect_to_path(path).await
    }
    pub async fn system_conn() -> Result<Self, sync_conn::Error> {
        let path = get_system_bus_path().await?;
        Self::connect_to_path(path).await
    }
    pub async fn connect_to_path<P: AsRef<Path>>(path: P) -> Result<Self, sync_conn::Error> {
        unimplemented!()
    }
    pub fn set_filter(&mut self, filter: MessageFilter) {
        unimplemented!()
    }
    pub fn send_message(&mut self) -> impl Future<Output=Result<impl Future<Output=MarshalledMessage>,sync_conn::Error>> {
        unimplemented!()
    }
    pub fn get_any_message(&mut self) -> impl Future<Output=Result<MarshalledMessage, sync_conn::Error>> {
        unimplemented!()
    }
    pub fn get_call(&mut self) -> impl Future<Output=Result<MarshalledMessage, sync_conn::Error>> {
        unimplemented!()
    }
    pub fn get_signal(&mut self) -> impl Future<Output=Result<MarshalledMessage, sync_conn::Error>> {
        unimplemented!()
    }

}


pub struct SendMessageFuture {

}
#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
