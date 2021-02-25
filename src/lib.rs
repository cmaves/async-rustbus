#![feature(unix_socket_ancillary_data)]

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_io::Async;
use async_std::channel::{unbounded, Receiver as CReceiver, Sender as CSender};
use async_std::path::Path;
use async_std::sync::{Mutex, MutexGuard};
use futures::future::{select, Either};
use futures::pin_mut;
use futures::prelude::*;
use futures::task::{Context, Poll, Waker};

mod rustbus_core;

use rustbus_core::message_builder::{MarshalledMessage, MessageType};
use rustbus_core::sync_conn;
use rustbus_core::sync_conn::rpc_conn::MessageFilter;

pub mod conn;

use conn::{Conn, Receiver, Sender};

mod utils;

use conn::{get_session_bus_path, get_system_bus_path};

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
    async fn send(&self, msg: MarshalledMessage) {
        self.sender.send(msg).await.unwrap()
    }
}
pub struct RpcConn {
    //conn: Async<MutexConn>,
    sender: Mutex<Async<Sender>>,
    receiver: Mutex<Async<Receiver>>,
    sig_queue: MsgQueue,
    reply_map: Mutex<HashMap<NonZeroU32, WakerOrMsg>>,
    serial: AtomicU32,
}
enum MsgOrRecv<'a> {
    Msg(MarshalledMessage),
    Recv(MutexGuard<'a, Async<Receiver>>),
}
impl RpcConn {
    pub fn new(/*what goes here*/) -> Self {
        unimplemented!()
    }
    pub async fn session_conn(with_fd: bool) -> Result<Self, sync_conn::Error> {
        let path = get_session_bus_path().await?;
        Self::connect_to_path(path, with_fd).await
    }
    pub async fn system_conn(with_fd: bool) -> Result<Self, sync_conn::Error> {
        let path = get_system_bus_path().await?;
        Self::connect_to_path(path, with_fd).await
    }
    pub async fn connect_to_path<P: AsRef<Path>>(
        path: P,
        with_fd: bool,
    ) -> Result<Self, sync_conn::Error> {
        let conn = Conn::connect_to_path(path, with_fd).await?;
        let (sender, receiver) = conn.split();
        Ok(Self {
            sender: Mutex::new(Async::new(sender)?),
            receiver: Mutex::new(Async::new(receiver)?),
            sig_queue: MsgQueue::new(),
            reply_map: Mutex::new(HashMap::new()),
            serial: AtomicU32::new(1),
        })
    }
    pub fn set_filter(&mut self, filter: MessageFilter) {
        unimplemented!()
    }
    pub async fn call_method<'a>(
        &'a self,
        msg: &MarshalledMessage,
    ) -> std::io::Result<impl Future<Output = std::io::Result<Option<MarshalledMessage>>> + 'a>
    {
        match msg.typ {
            MessageType::Call => {}
            _ => panic!("Didn't send message!"),
        }
        let mut msg_res = None;
        if msg.flags & NO_REPLY_EXPECTED != 0 {
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
        let mut async_sender = self.sender.lock().await;
        let mut started = false;
        async_sender
            .write_with_mut(move |sender| {
                if started {
                    sender.finish_sending_next()
                } else {
                    let res = sender.write_next_message(msg)?;
                    let sent = res.0;
                    debug_assert_eq!(res.1, None);
                    started = true;
                    if sent {
                        Ok(())
                    } else {
                        Err(ErrorKind::WouldBlock.into())
                    }
                }
            })
            .await?;
        Ok(self.wait_for_response(msg_res))
    }

    async fn wait_for_response(
        &self,
        res_idx: Option<NonZeroU32>,
    ) -> std::io::Result<Option<MarshalledMessage>> {
        match res_idx {
            Some(idx) => {
                let mut reply_fut = self.reply_map.lock().boxed();
                let mut recv_fut = self.receiver.lock().boxed();

                let res: MsgOrRecv = futures::future::poll_fn(move |cx| {
                    // Check to see if there is already a message present
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
                    // There wasn't or the lock was pending so try to receiver lock
                    match recv_fut.poll_unpin(cx) {
                        Poll::Ready(recv) => Poll::Ready(MsgOrRecv::Recv(recv)),
                        Poll::Pending => Poll::Pending,
                    }
                })
                .await;

                // We got a message or receiver. Get the message from it
                self.get_from_msg_or_recv(idx, res).await
            }
            None => Ok(None),
        }
    }
    async fn get_from_msg_or_recv(
        &self,
        idx: NonZeroU32,
        msg_or_recv: MsgOrRecv<'_>,
    ) -> std::io::Result<Option<MarshalledMessage>> {
        match msg_or_recv {
            MsgOrRecv::Msg(msg) => Ok(Some(msg)),
            MsgOrRecv::Recv(mut async_receiver) => {
                let mut reply_map = self.reply_map.lock().await;
                let ent = reply_map
                    .remove(&idx)
                    .expect("Only this future should remove its idx from the reply map!");
                match ent {
                    WakerOrMsg::Msg(msg) => Ok(Some(msg)),
                    WakerOrMsg::Waker(_) => {
                        drop(reply_map);
                        loop {
                            let msg = async_receiver
                                .read_with_mut(|receiver| receiver.get_next_message())
                                .await?;
                            match &msg.typ {
                                MessageType::Signal => self.sig_queue.send(msg).await,
                                MessageType::Reply | MessageType::Error => {
                                    let res_idx = match msg.dynheader.response_serial {
                                        Some(res_idx) => NonZeroU32::new(res_idx)
                                            .expect("serial should never be zero"),
                                        None => unreachable!(
                                            "Should never reply/err without res serial."
                                        ),
                                    };
                                    if res_idx == idx {
                                        break Ok(Some(msg));
                                    }
                                    let mut reply_map = self.reply_map.lock().await;
                                    if let Some(waker_or_msg) = reply_map.get_mut(&res_idx) {
                                        waker_or_msg.replace_and_wake(msg);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
    pub async fn get_signal(&self) -> std::io::Result<MarshalledMessage> {
        let msg = self.sig_queue.recv();
        let async_receiver = self.receiver.lock();
        pin_mut!(msg);
        pin_mut!(async_receiver);
        match select(msg, async_receiver).await {
            Either::Left((msg, _)) => Ok(msg),
            Either::Right((mut async_receiver, _)) => {
                async_receiver
                    .read_with_mut(|receiver| {
                        let msg = receiver.get_next_message()?;
                        match msg.typ {
                            MessageType::Signal => Ok(msg),
                            _ => unimplemented!(),
                        }
                    })
                    .await
            }
        }
    }
}

pub struct ResponseFuture<'a, T> 
    where
        T: Future<Output=std::io::Result<Option<MarshalledMessage>>> + Unpin
{
    rpc_conn: &'a RpcConn,
    idx: Option<NonZeroU32>,
    fut: T

}
impl<'a, T> ResponseFuture<'a, T>
    where
        T: Future<Output=std::io::Result<Option<MarshalledMessage>>> + Unpin
{
    fn new() -> Self {
        unimplemented!()
    }
}
impl<T> Future for ResponseFuture<'_, T> 
    where
        T: Future<Output=std::io::Result<Option<MarshalledMessage>>> + Unpin
{
    type Output = std::io::Result<Option<MarshalledMessage>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.fut.poll_unpin(cx) 
    }

}

#[cfg(test)]
mod tests {
    use libc;
    #[test]
    fn it_works() {
        assert_eq!(2 + 2, 4);
    }
    #[test]
    fn cmg_space() {
        let space = unsafe { libc::CMSG_SPACE(32 * 4) };
        assert_eq!(0, space);
    }
}
