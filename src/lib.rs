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
use async_std::sync::{Condvar, Mutex, MutexGuard};
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
//use utils::CallOnDrop;

use conn::{get_session_bus_addr, get_system_bus_path};

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
    async fn recv(&self) -> MarshalledMessage {
        self.recv.recv().await.unwrap()
    }
    fn send(&self, msg: MarshalledMessage) {
        self.sender.try_send(msg).unwrap()
    }
}
pub struct RpcConn {
    conn: Async<GenStream>,
    sig_queue: MsgQueue,
    call_queue: MsgQueue,
    recv_cond: Condvar,
    recv_data: Arc<Mutex<(RecvState, HashMap<NonZeroU32, Option<MarshalledMessage>>)>>,
    send_data: Mutex<(SendState, Option<NonZeroU32>)>,
    serial: AtomicU32,
    sig_filter: Box<dyn Send + Sync + Fn(&MarshalledMessage) -> bool>,
    auto_name: String,
}
impl RpcConn {
    pub async fn new(conn: Conn) -> std::io::Result<Self> {
        let mut ret = Self {
            conn: Async::new(conn.stream)?,
            sig_queue: MsgQueue::new(),
            call_queue: MsgQueue::new(),
            send_data: Mutex::new((conn.send_state, None)),
            recv_data: Arc::new(Mutex::new((conn.recv_state, HashMap::new()))),
            recv_cond: Condvar::new(),
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
        let mut idx = 0;
        while idx == 0 {
            idx = self.serial.fetch_add(1, Ordering::Relaxed);
        }
        let idx = NonZeroU32::new(idx).unwrap();
        let msg_res = if msg.typ == MessageType::Call && (msg.flags & NO_REPLY_EXPECTED) == 0 {
            let mut recv_lock = self.recv_data.lock().await;
            recv_lock.1.insert(idx, None);
            Some(idx)
        } else {
            None
        };
        let mut started = false;
        loop {
            let mut send_lock = self.send_data.lock().await;
            let stream = self.conn.get_ref();
            if started {
                match send_lock.1 {
                    Some(i) if i == idx => match send_lock.0.finish_sending_next(stream) {
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                        Err(e) => return Err(e),
                        _ => break,
                    },
                    _ => break,
                }
            } else {
                match send_lock.0.write_next_message(stream, msg) {
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                    Ok((sent, _)) => {
                        started = true;
                        if sent {
                            send_lock.1 = None;
                            break;
                        } else {
                            send_lock.1 = Some(idx);
                        }
                    }
                }
            }
            drop(send_lock);
            self.conn.writable().await?;
        }
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
                let res_pred = |msg: &MarshalledMessage| match &msg.typ {
                    MessageType::Reply | MessageType::Error => {
                        let res_idx = match msg.dynheader.response_serial {
                            Some(res_idx) => {
                                NonZeroU32::new(res_idx).expect("serial should never be zero")
                            }
                            None => {
                                unreachable!("Should never reply/err without res serial.")
                            }
                        };
                        res_idx == idx
                    }
                    _ => false,
                };
                let mut cond_wakeup = false;
                let mut recv_lock = self.recv_data.lock().await;
                loop {
                    if recv_lock.1.get(&idx).unwrap().is_some() {
                        return Ok(Some(recv_lock.1.remove(&idx).unwrap().unwrap()));
                    }
                    if !cond_wakeup {
                        match self.queue_msg(&mut recv_lock, res_pred) {
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                            Err(e) => return Err(e),
                            Ok(msg) => return Ok(Some(msg)),
                        }
                    };

                    // We either wake up when readable or another
                    // future has read data and updated the reply_map (recv_lock.1)
                    let recv_fut = self.recv_cond.wait(recv_lock);
                    let read_fut = self.conn.readable();
                    pin_mut!(recv_fut);
                    pin_mut!(read_fut);
                    match select(recv_fut, read_fut).await {
                        Either::Left((lock, _)) => {
                            recv_lock = lock;
                            cond_wakeup = true;
                        }
                        Either::Right((r, _)) => {
                            r?;
                            recv_lock = self.recv_data.lock().await;
                            cond_wakeup = false;
                        }
                    }
                }
            }
            None => Ok(None),
        }
    }
    fn queue_msg<F>(
        &self,
        recv_lock: &mut MutexGuard<'_, (RecvState, HashMap<NonZeroU32, Option<MarshalledMessage>>)>,
        pred: F,
    ) -> std::io::Result<MarshalledMessage>
    where
        F: Fn(&MarshalledMessage) -> bool,
    {
        let stream = self.conn.get_ref();
        loop {
            let msg = recv_lock.0.get_next_message(stream)?;
            if pred(&msg) {
                return Ok(msg);
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
                        if let Some(waiting) = recv_lock.1.get_mut(&idx) {
                            waiting.replace(msg);
                            self.recv_cond.notify_all();
                        }
                    }
                    MessageType::Call => self.call_queue.send(msg),
                    MessageType::Invalid => unreachable!(),
                }
            }
        }
    }

    pub async fn get_msg<Q, M, F>(&self, queue: Q, pred: F) -> std::io::Result<MarshalledMessage>
    where
        Q: Fn() -> M,
        M: Future<Output = MarshalledMessage>,
        F: Fn(&MarshalledMessage) -> bool,
    {
        let msg_fut = queue();
        pin_mut!(msg_fut);
        let mut recv_fut = self.recv_data.lock().boxed();
        loop {
            match select(msg_fut, recv_fut).await {
                Either::Left((msg, _)) => return Ok(msg),
                Either::Right((mut recv_lock, msg_f)) => {
                    match self.queue_msg(&mut recv_lock, &pred) {
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            msg_fut = msg_f;
                            self.conn.readable().await?;
                            recv_fut = self.recv_data.lock().boxed();
                        }
                        els => return els,
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
        let sig_queue = || self.sig_queue.recv();
        let sig_pred = |msg: &MarshalledMessage| match &msg.typ {
            MessageType::Signal => true,
            _ => false,
        };
        self.get_msg(sig_queue, sig_pred).await
    }
    /// Gets the next call not filtered by the message filter.
    ///
    /// *Warning:* The default message filter ignores all signals.
    /// You need to set a new message filter.
    pub async fn get_call(&self) -> std::io::Result<MarshalledMessage> {
        let call_queue = || self.call_queue.recv();
        let call_pred = |msg: &MarshalledMessage| match &msg.typ {
            MessageType::Call => true,
            _ => false,
        };
        self.get_msg(call_queue, call_pred).await
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
        if let Some(mut reply_map) = self.rpc_conn.recv_data.try_lock() {
            reply_map.1.remove(&idx);
            return;
        }
        let reply_arc = Arc::clone(&self.rpc_conn.recv_data);

        //TODO: Is there a better solution to this?
        async_std::task::spawn(async move {
            let mut reply_map = reply_arc.lock().await;
            reply_map.1.remove(&idx);
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
