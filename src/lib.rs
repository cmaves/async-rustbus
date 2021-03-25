use std::collections::HashMap;
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_io::Async;
use async_std::channel::{unbounded, Receiver as CReceiver, Sender as CSender};
use async_std::net::ToSocketAddrs;
use async_std::path::Path;
use async_std::sync::Mutex;
use futures::future::{select, Either};
use futures::pin_mut;
use futures::prelude::*;
use futures::task::{Context, Poll};

pub mod rustbus_core;

use rustbus_core::message_builder::{MarshalledMessage, MessageType};
use rustbus_core::standard_messages::hello;

pub mod conn;

use conn::{Conn, DBusAddr, GenStream, RecvState, SendState};

mod utils;
use utils::{one_time_channel, OneReceiver, OneSender};

mod routing;
pub use routing::CallAction;
use routing::CallHierarchy;

pub use conn::{get_session_bus_addr, get_system_bus_path};
/*
mod dispatcher;
pub use dispatcher::DispatcherConn;*/

const NO_REPLY_EXPECTED: u8 = 0x01;

pub static READ_COUNT: AtomicUsize = AtomicUsize::new(0);

struct MsgQueue {
    sender: CSender<MarshalledMessage>,
    recv: CReceiver<MarshalledMessage>,
}
impl MsgQueue {
    fn new() -> Self {
        let (sender, recv) = unbounded::<MarshalledMessage>();
        Self { sender, recv }
    }
    /*
    async fn recv(&self) -> MarshalledMessage {
        self.recv.recv().await.unwrap()
    }*/
    fn get_receiver(&self) -> CReceiver<MarshalledMessage> {
        self.recv.clone()
    }
    fn send(&self, msg: MarshalledMessage) {
        self.sender.try_send(msg).unwrap()
    }
}
struct RecvData {
    state: RecvState,
    reply_map: HashMap<NonZeroU32, OneSender<MarshalledMessage>>,
    hierarchy: CallHierarchy,
}
pub struct RpcConn {
    conn: Async<GenStream>,
    sig_queue: MsgQueue,
    //call_queue: MsgQueue,
    //recv_cond: Condvar,
    recv_data: Arc<Mutex<RecvData>>,
    send_data: Mutex<(SendState, Option<NonZeroU32>)>,
    serial: AtomicU32,
    sig_filter: Box<dyn Send + Sync + Fn(&MarshalledMessage) -> bool>,
    auto_name: String,
}
impl RpcConn {
    async fn new(conn: Conn) -> std::io::Result<Self> {
        let recv_data = RecvData {
            state: conn.recv_state,
            reply_map: HashMap::new(),
            hierarchy: CallHierarchy::new(),
        };
        let mut ret = Self {
            conn: Async::new(conn.stream)?,
            sig_queue: MsgQueue::new(),
            //call_queue: MsgQueue::new(),
            send_data: Mutex::new((conn.send_state, None)),
            recv_data: Arc::new(Mutex::new(recv_data)),
            //recv_cond: Condvar::new(),
            serial: AtomicU32::new(1),
            sig_filter: Box::new(|_| false),
            auto_name: String::new(),
        };
        let hello_res = ret.send_message(&hello()).await?.unwrap().await?;
        match hello_res.typ {
            MessageType::Reply => {
                ret.auto_name = hello_res.body.parser().get().map_err(|_| {
                    std::io::Error::new(ErrorKind::ConnectionRefused, "Unable to parser name")
                })?;
                Ok(ret)
            }
            MessageType::Error => {
                let (err, details): (&str, &str) = hello_res
                    .body
                    .parser()
                    .get()
                    .unwrap_or(("Unable to parse message", ""));
                Err(std::io::Error::new(
                    ErrorKind::ConnectionRefused,
                    format!("Hello message failed with: {}: {}", err, details),
                ))
            }
            _ => Err(std::io::Error::new(
                ErrorKind::ConnectionAborted,
                "Unexpected reply to hello message!",
            )),
        }
    }
    pub fn get_name(&self) -> &str {
        &self.auto_name
    }
    /// Connect to the system bus.
    pub async fn session_conn(with_fd: bool) -> std::io::Result<Self> {
        let addr = get_session_bus_addr().await?;
        Self::connect_to_addr(&addr, with_fd).await
    }
    pub async fn system_conn(with_fd: bool) -> std::io::Result<Self> {
        let path = get_system_bus_path().await?;
        Self::connect_to_path(path, with_fd).await
    }
    pub async fn connect_to_addr<P: AsRef<Path>, S: ToSocketAddrs>(
        addr: &DBusAddr<P, S>,
        with_fd: bool,
    ) -> std::io::Result<Self> {
        let conn = Conn::connect_to_addr(addr, with_fd).await?;
        Ok(Self::new(conn).await?)
    }
    pub async fn connect_to_path<P: AsRef<Path>>(path: P, with_fd: bool) -> std::io::Result<Self> {
        let conn = Conn::connect_to_path(path, with_fd).await?;
        Ok(Self::new(conn).await?)
    }
    pub fn set_sig_filter(
        &mut self,
        filter: Box<dyn Send + Sync + Fn(&MarshalledMessage) -> bool>,
    ) {
        self.sig_filter = filter;
    }
    fn allocate_idx(&self) -> NonZeroU32 {
        let mut idx = 0;
        while idx == 0 {
            idx = self.serial.fetch_add(1, Ordering::Relaxed);
        }
        NonZeroU32::new(idx).unwrap()
    }
    /// Make a DBus call to a remote service or a signal.
    ///
    /// This function returns a future nested inside a future.
    /// Awaiting the outer future sends the message out the DBus stream to the remote service.
    /// The inner future, returned by the outer, waits for the response from the remote service.
    /// # Notes
    /// * If the message sent was a signal or has the NO_REPLY_EXPECTED flag set then the inner future will
    ///   return immediatly when awaited.
    /// * If two futures are simultanously being awaited (like via `futures::future::join`) then
    ///   outgoing order of messages is not guaranteed.
    ///
    pub async fn send_message<'a>(
        &'a self,
        msg: &MarshalledMessage,
    ) -> std::io::Result<Option<impl Future<Output = std::io::Result<MarshalledMessage>> + 'a>>
    {
        let idx = self.allocate_idx();
        let msg_res = if expects_reply(msg) {
            let recv = self.get_recv_and_insert_sender(idx).await;
            Some(recv)
        } else {
            None
        };
        self.send_msg_loop(msg, idx).await?;
        Ok(match msg_res {
            Some(recv) => Some(ResponseFuture {
                idx,
                rpc_conn: self,
                fut: self.wait_for_response(idx, recv).boxed(),
            }),
            None => None,
        })
        //Ok(self.wait_for_response(msg_res))
    }
    async fn send_msg_loop(&self, msg: &MarshalledMessage, idx: NonZeroU32) -> std::io::Result<()> {
        let mut started = false;
        loop {
            let mut send_lock = self.send_data.lock().await;
            let stream = self.conn.get_ref();
            if started {
                match send_lock.1 {
                    Some(i) if i == idx => match send_lock.0.finish_sending_next(stream) {
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e),
                        _ => return Ok(()),
                    },
                    _ => return Ok(()),
                }
            } else {
                match send_lock.0.write_next_message(stream, msg, idx) {
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                    Ok(sent) => {
                        started = true;
                        if sent {
                            send_lock.1 = None;
                            return Ok(());
                        } else {
                            send_lock.1 = Some(idx);
                        }
                    }
                }
            }
            drop(send_lock);
            self.conn.writable().await?;
        }
    }
    pub async fn send_msg_no_reply(&self, msg: &MarshalledMessage) -> std::io::Result<()> {
        assert!(!expects_reply(msg));
        let idx = self.allocate_idx();
        self.send_msg_loop(msg, idx).await
    }
    pub async fn send_msg_with_reply<'a>(
        &'a self,
        msg: &MarshalledMessage,
    ) -> std::io::Result<impl Future<Output = std::io::Result<MarshalledMessage>> + 'a> {
        assert!(expects_reply(msg));
        let idx = self.allocate_idx();
        let recv = self.get_recv_and_insert_sender(idx).await;
        self.send_msg_loop(msg, idx).await?;
        Ok(ResponseFuture {
            idx,
            rpc_conn: self,
            fut: self.wait_for_response(idx, recv).boxed(),
        })
    }
    async fn get_recv_and_insert_sender(&self, idx: NonZeroU32) -> OneReceiver<MarshalledMessage> {
        let (sender, recv) = one_time_channel();
        let mut recv_lock = self.recv_data.lock().await;
        recv_lock.reply_map.insert(idx, sender);
        recv
    }
    async fn wait_for_response(
        &self,
        idx: NonZeroU32,
        recv: OneReceiver<MarshalledMessage>,
    ) -> std::io::Result<MarshalledMessage> {
        let res_pred = |msg: &MarshalledMessage, _: &mut RecvData| match &msg.typ {
            MessageType::Reply | MessageType::Error => {
                let res_idx = match msg.dynheader.response_serial {
                    Some(res_idx) => NonZeroU32::new(res_idx).expect("serial should never be zero"),
                    None => {
                        unreachable!("Should never reply/err without res serial.")
                    }
                };
                res_idx == idx
            }
            _ => false,
        };
        let msg_fut = recv.recv();
        pin_mut!(msg_fut);
        loop {
            let recv_fut = self.recv_data.lock();
            pin_mut!(recv_fut);
            match select(msg_fut, recv_fut).await {
                Either::Left((msg, _)) => {
                    let msg = msg.unwrap();
                    return Ok(msg);
                }
                Either::Right((mut recv_lock, msg_f)) => {
                    match self.queue_msg(&mut recv_lock, res_pred) {
                        Ok((msg, bad)) => {
                            if bad {
                                let res = msg
                                    .dynheader
                                    .make_error_response("UnknownObject".to_string(), None);
                                self.send_msg_no_reply(&res).await?;
                            } else {
                                return Ok(msg);
                            }
                            /*
                            msg_fut = msg_f;
                            */
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e),
                    }
                    msg_fut = msg_f;
                }
            }
            let read_fut = self.conn.readable();
            pin_mut!(read_fut);
            match select(msg_fut, read_fut).await {
                Either::Left((msg, _)) => {
                    let msg = msg.unwrap();
                    return Ok(msg);
                }
                Either::Right((_, msg_f)) => {
                    msg_fut = msg_f;
                }
            }
        }
    }
    fn queue_msg<F>(
        &self,
        recv_data: &mut RecvData,
        pred: F,
    ) -> std::io::Result<(MarshalledMessage, bool)>
    where
        F: Fn(&MarshalledMessage, &mut RecvData) -> bool,
    {
        let stream = self.conn.get_ref();
        loop {
            let msg = recv_data.state.get_next_message(stream)?;
            if pred(&msg, recv_data) {
                return Ok((msg, false));
            } else {
                match &msg.typ {
                    MessageType::Signal => self.sig_queue.send(msg),
                    MessageType::Reply | MessageType::Error => {
                        let idx = msg
                            .dynheader
                            .response_serial
                            .expect("Reply should always have a response serial!");
                        let idx =
                            NonZeroU32::new(idx).expect("Reply should always have non zero u32!");
                        if let Some(sender) = recv_data.reply_map.remove(&idx) {
                            sender.send(msg).ok();
                        }
                    }
                    MessageType::Call => {
                        if let Err(msg) = recv_data.hierarchy.send(msg) {
                            return Ok((msg, true));
                        }
                    }
                    MessageType::Invalid => unreachable!(),
                }
            }
        }
    }

    async fn get_msg<Q, F>(&self, queue: Q, pred: F) -> std::io::Result<MarshalledMessage>
    where
        Q: FnOnce(&mut RecvData) -> Option<CReceiver<MarshalledMessage>>,
        F: Fn(&MarshalledMessage, &mut RecvData) -> bool,
    {
        let mut recv_data = self.recv_data.lock().await;
        let queue = queue(&mut recv_data).ok_or_else(|| {
            std::io::Error::new(ErrorKind::InvalidInput, "Invalid message path given!")
        })?;
        let msg_fut = queue.recv();
        pin_mut!(msg_fut);
        let mut recv_fut = futures::future::ready(recv_data).boxed();
        loop {
            match select(msg_fut, recv_fut).await {
                Either::Left((msg, _)) => {
                    let msg = msg.map_err(|_| {
                        std::io::Error::new(
                            ErrorKind::Interrupted,
                            "Message Queue was deleted, while waiting!",
                        )
                    })?;
                    return Ok(msg);
                }
                Either::Right((mut recv_lock, msg_f)) => {
                    match self.queue_msg(&mut recv_lock, &pred) {
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            drop(recv_lock);
                            msg_fut = msg_f;
                            self.conn.readable().await?;
                            recv_fut = self.recv_data.lock().boxed();
                        }
                        Err(e) => return Err(e),
                        Ok((msg, bad)) => {
                            if bad {
                                drop(recv_lock);
                                self.send_msg_no_reply(&msg).await?;
                                recv_fut = self.recv_data.lock().boxed();
                                msg_fut = msg_f;
                            } else {
                                return Ok(msg);
                            }
                        }
                    }
                }
            }
        }
    }
    /// Gets the next signal not filtered by the message filter.
    ///
    /// *Warning:* The default signal filter ignores all message.
    /// You need to set a new message filter.
    pub async fn get_signal(&self) -> std::io::Result<MarshalledMessage> {
        let sig_queue = |_: &mut RecvData| Some(self.sig_queue.get_receiver());
        let sig_pred = |msg: &MarshalledMessage, _: &mut RecvData| match &msg.typ {
            MessageType::Signal => true,
            _ => false,
        };
        self.get_msg(sig_queue, sig_pred).await
    }
    /// Gets the next call not filtered by the message filter.
    ///
    /// *Warning:* The default message filter ignores all signals.
    /// You need to set a new message filter.
    pub async fn get_call<S: AsRef<str>>(&self, path: S) -> std::io::Result<MarshalledMessage> {
        let path = path.as_ref();
        let call_queue =
            |recv_data: &mut RecvData| Some(recv_data.hierarchy.get_queue(path)?.get_receiver());
        let call_pred = |msg: &MarshalledMessage, recv_data: &mut RecvData| match &msg.typ {
            MessageType::Call => {
                let msg_path = msg.dynheader.object.as_ref().unwrap();
                recv_data.hierarchy.is_match(path, msg_path)
            }
            _ => false,
        };
        self.get_msg(call_queue, call_pred).await
    }
    pub async fn insert_call_path<S: AsRef<str>>(&self, path: S, action: CallAction) {
        let path = path.as_ref();
        let mut recv_data = self.recv_data.lock().await;
        recv_data.hierarchy.insert_path(path, action);
    }
    pub async fn get_call_path_action(&self, path: &str) -> Option<CallAction> {
        let recv_data = self.recv_data.lock().await;
        recv_data.hierarchy.get_action(path)
    }
    pub async fn get_call_recv<S: AsRef<str>>(&self, path: S) -> Option<CReceiver<MarshalledMessage>> {
        let path = path.as_ref();
        let recv_data = self.recv_data.lock().await;
        Some(recv_data.hierarchy.get_queue(path)?.get_receiver())
    }
}

struct ResponseFuture<'a, T>
where
    T: Future<Output = std::io::Result<MarshalledMessage>> + Unpin,
{
    rpc_conn: &'a RpcConn,
    idx: NonZeroU32,
    fut: T,
}

impl<T> Future for ResponseFuture<'_, T>
where
    T: Future<Output = std::io::Result<MarshalledMessage>> + Unpin,
{
    type Output = T::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.fut.poll_unpin(cx)
    }
}

impl<T> Drop for ResponseFuture<'_, T>
where
    T: Future<Output = std::io::Result<MarshalledMessage>> + Unpin,
{
    fn drop(&mut self) {
        if let Some(mut recv_lock) = self.rpc_conn.recv_data.try_lock() {
            recv_lock.reply_map.remove(&self.idx);
            return;
        }
        let reply_arc = Arc::clone(&self.rpc_conn.recv_data);

        //TODO: Is there a better solution to this?
        let idx = self.idx;
        async_std::task::spawn(async move {
            let mut recv_lock = reply_arc.lock().await;
            recv_lock.reply_map.remove(&idx);
        });
    }
}

fn expects_reply(msg: &MarshalledMessage) -> bool {
    msg.typ == MessageType::Call && (msg.flags & NO_REPLY_EXPECTED) == 0
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
