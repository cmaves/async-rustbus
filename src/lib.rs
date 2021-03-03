use std::collections::HashMap;
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_io::Async;
use async_std::channel::{unbounded, Receiver as CReceiver, Sender as CSender};
use async_std::net::ToSocketAddrs;
use async_std::path::Path;
use async_std::sync::{Mutex, MutexGuard};
use async_std::task::spawn;
use futures::future::{select, Either};
use futures::pin_mut;
use futures::prelude::*;
use futures::task::{Context, Poll, Waker};

pub mod rustbus_core;

use rustbus_core::message_builder::{MarshalledMessage, MessageType};
use rustbus_core::standard_messages::hello;

pub mod conn;

use conn::{AsyncConn, Conn, DBusAddr, RecvState};

mod utils;
//use utils::CallOnDrop;

use conn::{get_session_bus_addr, get_system_bus_path};

const NO_REPLY_EXPECTED: u8 = 0x01;

enum WakerOrMsg {
    Waker(Waker),
    Msg(MarshalledMessage),
}
impl WakerOrMsg {
    fn replace_and_wake(&mut self, msg: MarshalledMessage) {
        let msg = WakerOrMsg::Msg(msg);
        if let WakerOrMsg::Waker(waker) = std::mem::replace(self, msg) {
            waker.wake()
        }
    }
}
struct MsgQueue {
    sender: CSender<MarshalledMessage>,
    recv: CReceiver<MarshalledMessage>,
}
impl MsgQueue {
    fn new() -> Self {
        let (sender, recv) = unbounded::<MarshalledMessage>();
        Self { sender, recv }
    }
    async fn recv(&self) -> MarshalledMessage {
        self.recv.recv().await.unwrap()
    }
    fn send(&self, msg: MarshalledMessage) {
        self.sender.try_send(msg).unwrap()
    }
}
pub struct RpcConn {
    conn: Async<AsyncConn>,
    msg_queue: MsgQueue,
    sig_queue: MsgQueue,
    call_queue: MsgQueue,
    reply_map: Arc<Mutex<HashMap<NonZeroU32, WakerOrMsg>>>,
    serial: AtomicU32,
    sig_filter: Box<dyn Send + Sync + Fn(&MarshalledMessage) -> bool>,
    auto_name: String,
}
enum MsgOrRecv<'a> {
    Msg(MarshalledMessage),
    Recv(MutexGuard<'a, RecvState>),
}
impl RpcConn {
    pub async fn new(conn: Conn) -> std::io::Result<Self> {
        let mut ret = Self {
            conn: Async::new(conn.into())?,
            sig_queue: MsgQueue::new(),
            msg_queue: MsgQueue::new(),
            call_queue: MsgQueue::new(),
            reply_map: Arc::new(Mutex::new(HashMap::new())),
            serial: AtomicU32::new(1),
            sig_filter: Box::new(|_| false),
            auto_name: String::new(),
        };
        let hello_res = ret.send_message(&hello()).await?.await?.unwrap();
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
    ) -> std::io::Result<impl Future<Output = std::io::Result<Option<MarshalledMessage>>> + 'a>
    {
        let mut msg_res = None;
        if let MessageType::Call = msg.typ {
            if msg.flags & NO_REPLY_EXPECTED == 0 {
                // We expect a reply so we need to reserver
                // the serial for the reply and insert it into the reply map.
                let mut idx = self.serial.fetch_add(1, Ordering::Relaxed);
                if idx == 0 {
                    idx = self.serial.fetch_add(1, Ordering::Relaxed);
                }
                msg_res = NonZeroU32::new(idx);
                let idx = msg_res.unwrap();
                let mut reply_map = self.reply_map.lock().await;
                futures::future::poll_fn(|cx| {
                    reply_map.insert(idx, WakerOrMsg::Waker(cx.waker().clone()));
                    Poll::Ready(())
                })
                .await;
            }
        }
        let mut ss_option = Some(self.conn.get_ref().send_state.lock().await);
        let mut started = false;
        self.conn
            .write_with(move |sender| {
                let send_state = ss_option.as_mut()
                    .expect("send_state MutexGuard should only be taken on last iteration of this function.");
                if started {
                    match send_state.finish_sending_next(&sender.stream) {
                        Err(e) if e.kind() == ErrorKind::WouldBlock => Err(e),
                        els => {
                            // We need to ensure the mutex is freed 
                            // so there is not deadlock in other futures.
                            drop(ss_option.take());
                            els
                        }
                    }
                } else {
                    let res = send_state.write_next_message(&sender.stream, msg)?;
                    let sent = res.0;
                    //debug_assert_eq!(res.1, None);
                    started = true;
                    if sent {
                        // We need to ensure the mutex is freed 
                        // so there is not deadlock in other futures.
                        drop(ss_option.take());
                        Ok(())
                    } else {
                        Err(ErrorKind::WouldBlock.into())
                    }
                }
            })
            .await?;
        Ok(ResponseFuture {
            rpc_conn: self,
            fut: self.wait_for_response(msg_res).boxed(),
            idx: msg_res,
        })
        //Ok(self.wait_for_response(msg_res))
    }

    async fn wait_for_response(
        &self,
        res_idx: Option<NonZeroU32>,
    ) -> std::io::Result<Option<MarshalledMessage>> {
        match res_idx {
            Some(idx) => {
                let mut reply_fut = self.reply_map.lock().boxed();
                let mut msg_queue_fut = self.msg_queue.recv().boxed();
                let mut recv_fut = self.conn.get_ref().recv_state.lock().boxed();
                // let mut recv_fut = self.conn.lock().boxed();
                let res: MsgOrRecv = futures::future::poll_fn(move |cx| {
                    // Check to see if there is already a message in the map
                    match reply_fut.poll_unpin(cx) {
                        Poll::Ready(mut reply_map) => {
                            let ent = reply_map.remove(&idx).expect(
                                "Only this future should remove its idx from the reply map!",
                            );
                            match ent {
                                WakerOrMsg::Msg(msg) => return Poll::Ready(MsgOrRecv::Msg(msg)),
                                WakerOrMsg::Waker(_) => {
                                    reply_map.insert(idx, WakerOrMsg::Waker(cx.waker().clone()));
                                    // because this is a brand new future we wont wake until next call
                                    reply_fut = self.reply_map.lock().boxed();
                                }
                            }
                        }
                        Poll::Pending => {}
                    }
                    match msg_queue_fut.poll_unpin(cx) {
                        Poll::Ready(msg) => {
                            let other_idx = msg.dynheader.response_serial.unwrap();
                            let other_idx = NonZeroU32::new(other_idx).unwrap();
                            if other_idx == idx.into() {
                                return Poll::Ready(MsgOrRecv::Msg(msg));
                            } else {
                                match self.reply_map.try_lock() {
                                    Some(mut reply_map) => {
                                        if let Some(ent) = reply_map.get_mut(&other_idx) {
                                            ent.replace_and_wake(msg);
                                        }
                                    }
                                    None => {
                                        let reply_arc = self.reply_map.clone();
                                        spawn(async move {
                                            let mut reply_map = reply_arc.lock().await;
                                            if let Some(ent) = reply_map.get_mut(&other_idx) {
                                                ent.replace_and_wake(msg);
                                            }
                                        });
                                    }
                                }
                                msg_queue_fut = self.msg_queue.recv().boxed();
                            }
                        }
                        Poll::Pending => {}
                    }
                    // There wasn't or the lock was pending so try to receiver lock
                    match recv_fut.poll_unpin(cx) {
                        Poll::Ready(recv) => Poll::Ready(MsgOrRecv::Recv(recv)),
                        Poll::Pending => Poll::Pending,
                    }
                })
                .await;

                // We got a message or receiver. Get the message from it
                Ok(Some(self.get_from_msg_or_recv(idx, res).await?))
            }
            None => Ok(None),
        }
    }
    fn queue_msgs<F>(
        &self,
        rs_option: &mut Option<MutexGuard<'_, RecvState>>,
        pred: F,
    ) -> std::io::Result<MarshalledMessage>
    where
        F: Fn(&MarshalledMessage) -> bool,
    {
        loop {
            let recv_state = rs_option
                .as_mut()
                .expect("The recv_state MutexGuard should only be taken by this function");
            let msg = match recv_state.get_next_message(&self.conn.get_ref().stream) {
                Err(e) if e.kind() != ErrorKind::WouldBlock => {
                    drop(rs_option.take());
                    Err(e)
                }
                els => els,
            }?;
            if pred(&msg) {
                drop(rs_option.take());
                return Ok(msg);
            } else {
                match &msg.typ {
                    MessageType::Signal => self.sig_queue.send(msg),
                    MessageType::Reply | MessageType::Error => self.msg_queue.send(msg),
                    MessageType::Call => self.call_queue.send(msg),
                    MessageType::Invalid => unreachable!(),
                }
            }
        }
    }
    async fn get_from_msg_or_recv(
        &self,
        idx: NonZeroU32,
        msg_or_recv: MsgOrRecv<'_>,
    ) -> std::io::Result<MarshalledMessage> {
        match msg_or_recv {
            MsgOrRecv::Msg(msg) => Ok(msg),
            MsgOrRecv::Recv(recv_state) => {
                let mut reply_map = self.reply_map.lock().await;
                let ent = reply_map
                    .remove(&idx)
                    .expect("Only this future should remove its idx from the reply map!");
                match ent {
                    WakerOrMsg::Msg(msg) => Ok(msg),
                    WakerOrMsg::Waker(_) => {
                        drop(reply_map);
                        let mut rs_option = Some(recv_state);
                        let res_pred = |msg: &MarshalledMessage| match &msg.typ {
                            MessageType::Reply | MessageType::Error => {
                                let res_idx = match msg.dynheader.response_serial {
                                    Some(res_idx) => NonZeroU32::new(res_idx)
                                        .expect("serial should never be zero"),
                                    None => {
                                        unreachable!("Should never reply/err without res serial.")
                                    }
                                };
                                res_idx == idx
                            }
                            _ => false,
                        };
                        self.conn
                            .read_with(|_| self.queue_msgs(&mut rs_option, res_pred))
                            .await
                    }
                }
            }
        }
    }

    async fn get_msg<'a, M, F>(&self, msg_fut: M, msg_pred: F) -> std::io::Result<MarshalledMessage>
    where
        M: Future<Output = MarshalledMessage>,
        F: Fn(&MarshalledMessage) -> bool,
    {
        let async_fut = self.conn.get_ref().recv_state.lock();
        pin_mut!(msg_fut);
        pin_mut!(async_fut);
        match select(msg_fut, async_fut).await {
            Either::Left((msg, _)) => Ok(msg),
            Either::Right((rs, _)) => {
                let mut rs_option = Some(rs);
                self.conn
                    .read_with(|_| self.queue_msgs(&mut rs_option, &msg_pred))
                    .await
            }
        }
    }
    /// Gets the next signal not filtered by the message filter.
    ///
    /// *Warning:* The default signal filter ignores all message.
    /// You need to set a new message filter.
    pub async fn get_signal(&self) -> std::io::Result<MarshalledMessage> {
        let msg = self.sig_queue.recv();
        let sig_pred = |msg: &MarshalledMessage| match &msg.typ {
            MessageType::Signal => true,
            _ => false,
        };
        self.get_msg(msg, sig_pred).await
    }
    /// Gets the next call not filtered by the message filter.
    ///
    /// *Warning:* The default message filter ignores all signals.
    /// You need to set a new message filter.
    pub async fn get_call(&self) -> std::io::Result<MarshalledMessage> {
        let msg = self.call_queue.recv();
        let call_pred = |msg: &MarshalledMessage| match &msg.typ {
            MessageType::Call => true,
            _ => false,
        };
        self.get_msg(msg, call_pred).await
    }
}

struct ResponseFuture<'a, T>
where
    T: Future<Output = std::io::Result<Option<MarshalledMessage>>> + Unpin,
{
    rpc_conn: &'a RpcConn,
    idx: Option<NonZeroU32>,
    fut: T,
}

impl<T> Future for ResponseFuture<'_, T>
where
    T: Future<Output = std::io::Result<Option<MarshalledMessage>>> + Unpin,
{
    type Output = std::io::Result<Option<MarshalledMessage>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.fut.poll_unpin(cx)
    }
}

impl<T> Drop for ResponseFuture<'_, T>
where
    T: Future<Output = std::io::Result<Option<MarshalledMessage>>> + Unpin,
{
    fn drop(&mut self) {
        let idx = match self.idx {
            Some(idx) => idx,
            None => return,
        };
        if let Some(mut reply_map) = self.rpc_conn.reply_map.try_lock() {
            reply_map.remove(&idx);
            return;
        }
        let reply_arc = Arc::clone(&self.rpc_conn.reply_map);

        //TODO: Is there a better solution to this?
        async_std::task::spawn(async move {
            let mut reply_map = reply_arc.lock().await;
            reply_map.remove(&idx);
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
}
