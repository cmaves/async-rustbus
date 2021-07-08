//! Async-rustbus is an async-rustbus library built on top of [`rustbus`].
//! It is a multi-threaded client allowing for asynchronous calls to services,
//! and the creation of local services.
//!
//! # Missing Features
//! * Eavesdropping using match rules is not currently supported.
//! * Monitor mode is not currently supported. There are plans to implement it.
//! * There is no support for DBUS_COOKIE_SHA1 authentication. This makes DBus over TCP not
//!   as useful with only ANOYNMOUS mode supported when using TCP (EXTERNAL is not available for TCP).
//! # API Stability
//! As with most crates,
//! breaking changes will only be added after an increment of the most sigificant version number (SemVer).
//! The most likely elements to change in the future is the incoming signal handling
//! and the relation of this crate and [`rustbus`].
//!
//!
//! # Examples
//! An example client that queues info about the current DBus session server connections:
//! ```
//! # async_std::task::block_on(async {
//! use std::collections::HashMap;
//! use futures::future::try_join_all;
//! use async_rustbus::{RpcConn, MatchRule};
//! use async_rustbus::rustbus_core::message_builder;
//! use async_rustbus::rustbus_core::dbus_variant_var;
//! use message_builder::{MessageBuilder, MarshalledMessage, MessageType};
//!
//! // Create the DBus connection to the session DBus.
//! let conn = RpcConn::session_conn(false).await.unwrap();
//!
//! // Fetch the ID of the DBus
//! let mut msg = MessageBuilder::new().call("GetId")
//!     .with_interface("org.freedesktop.DBus")
//!     .on("/org/freedesktop/DBus")
//!     .at("org.freedesktop.DBus")
//!     .build();
//! let res = conn.send_msg_w_rsp(&msg).await.unwrap().await.unwrap();
//! assert!(matches!(res.typ, MessageType::Reply));
//! let id: &str = res.body.parser().get().unwrap();
//! println!("Info for Dbus {}:", id);
//!
//! // Get call of the names of all connections.
//! msg.dynheader.member = Some("ListNames".into());
//! let res = conn.send_msg_w_rsp(&msg).await.unwrap().await.unwrap();
//! assert!(matches!(res.typ, MessageType::Reply));
//! let mut names: Vec<&str> = res.body.parser().get().unwrap();
//! // Ignore unique names
//! names.retain(|s| !s.starts_with(":"));
//!
//! // Get stats for each individual message
//! let mut dbg_msg = MessageBuilder::new().call("GetConnectionStats")
//!     .with_interface("org.freedesktop.DBus.Debug.Stats")
//!     .on("/org/freedesktop/DBus")
//!     .at("org.freedesktop.DBus")
//!     .build();
//! let mut res_futs = Vec::with_capacity(names.len());
//! for name in names.iter() {
//!     dbg_msg.body.reset();
//!     dbg_msg.body.push_param(name).unwrap();
//!     let res_fut = conn.send_msg_w_rsp(&dbg_msg).await.unwrap();
//!     res_futs.push(res_fut);
//! }
//! let stats = try_join_all(res_futs).await.unwrap();
//!
//! // Parse responses and print out some info
//! dbus_variant_var!(StatVariant, U32 => u32; Str => &'buf str);
//! for (name, stat_msg) in names.into_iter().zip(stats) {
//!     if !matches!(stat_msg.typ, MessageType::Reply) {
//!         continue;
//!     }
//!     let mut stat_map: HashMap<&str, StatVariant> = stat_msg.body.parser().get().unwrap();
//!     let unique = match stat_map["UniqueName"] {
//!         StatVariant::Str(s) => s,
//!         _ => continue,
//!     };
//!     let peak_out = match stat_map["PeakOutgoingBytes"] {
//!         StatVariant::U32(s) => s,
//!         _ => continue,
//!     };
//!     let peak_in = match stat_map["PeakIncomingBytes"] {
//!         StatVariant::U32(s) => s,
//!         _ => continue,
//!     };
//!     println!("\t{} ({}):", name, unique);
//!     println!("\t\t PeakIncomingBytes: {}, PeakOutgoingBytes: {}\n", peak_in, peak_out);
//! }
//! # });
//! ```
//! A simple example server that gives out the time in millis since Epoch and a reference time:
//! ```no_run
//! # async_std::task::block_on(async {
//! use async_rustbus::{RpcConn, MatchRule, CallAction};
//! use async_rustbus::rustbus_core;
//! use rustbus_core::message_builder::{MessageBuilder, MessageType};
//! use std::time::{Instant, SystemTime, UNIX_EPOCH};
//! let conn = RpcConn::session_conn(false).await.unwrap();
//! conn.insert_call_path("/example/TimeServer", CallAction::Exact).await.unwrap();
//! conn.insert_call_path("/", CallAction::Intro).await.unwrap();
//! conn.request_name("example.TimeServer").await.unwrap();
//! let start = Instant::now();
//! loop {
//!     let call = match conn.get_call("/example/TimeServer").await {
//!         Ok(c) => c,
//!         Err(e) => {
//!             eprintln!("Error occurred waiting for calls: {:?}", e);
//!               break;
//!         }
//!        };
//!     assert!(matches!(call.typ, MessageType::Call));
//!     let res = match (call.dynheader.interface.as_deref().unwrap(), call.dynheader.member.as_deref().unwrap()) {
//!         ("example.TimeServer", "GetUnixTime") => {
//!             let mut res = call.dynheader.make_response();
//!             let cur_time = UNIX_EPOCH.elapsed().unwrap().as_millis() as u64;
//!             res.body.push_param(cur_time).unwrap();
//!             res
//!         }
//!         ("example.TimeServer", "GetRefTime") => {
//!             let mut res = call.dynheader.make_response();
//!             let elapsed = start.elapsed().as_millis() as u64;
//!             res.body.push_param(elapsed).unwrap();
//!             res
//!         }
//!         ("org.freedesktop.DBus.Introspectable", "Introspect") => {
//!             todo!("We need to put a introspect impl so that other connection can discover this object.");
//!         }
//!         _ => {
//!             call.dynheader.make_error_response("UnknownInterface", None)
//!         }
//!     };
//!     conn.send_msg_wo_rsp(&res).await.unwrap();
//! }
//! # });
//! ```
//!
//! [`rustbus`]: https://crates.io/crates/rustbus
use std::collections::HashMap;
use std::convert::TryInto;
use std::io::ErrorKind;
use std::num::NonZeroU32;
use std::os::unix::io::{AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_io::Async;
use async_std::channel::{unbounded, Receiver as CReceiver, Sender as CSender};
use async_std::future::ready;
use async_std::net::ToSocketAddrs;
use async_std::path::Path;
use async_std::sync::{Mutex, Condvar};
use futures::future::{select, Either};
use futures::pin_mut;
use futures::prelude::*;
use futures::task::{Context, Poll};


pub mod rustbus_core;

use rustbus_core::message_builder::{MarshalledMessage, MessageType};
use rustbus_core::path::ObjectPath;
use rustbus_core::standard_messages::{hello, release_name, request_name};
use rustbus_core::standard_messages::{
    DBUS_NAME_FLAG_DO_NOT_QUEUE, DBUS_REQUEST_NAME_REPLY_ALREADY_OWNER,
    DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER,
};

pub mod conn;

use conn::{Conn, GenStream, RecvState, SendState};

mod utils;
use utils::{one_time_channel, OneReceiver, OneSender, prime_future};

mod routing;
use routing::{queue_sig, CallHierarchy};
pub use routing::{CallAction, MatchRule, EMPTY_MATCH};

pub use conn::{get_session_bus_addr, get_system_bus_addr, DBusAddr};
/*
mod dispatcher;
pub use dispatcher::DispatcherConn;*/

const NO_REPLY_EXPECTED: u8 = 0x01;

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
    sig_matches: Vec<MatchRule>,
    //sig_filter: Box<dyn Send + Sync + FnMut(&MarshalledMessage) -> bool>,
}
/// RpcConn is used to create and interact with a DBus connection.
/// It can be used to easily connect to either session (user) or system DBus daemons.
/// `RpcConn` is thread-safe and can be used from within an [`Arc`] if desired.
///
/// [`Arc`]: https://doc.rust-lang.org/std/sync/struct.Arc.html
pub struct RpcConn {
    conn: Async<GenStream>,
    //call_queue: MsgQueue,
    recv_cond: Condvar,
    recv_data: Arc<Mutex<RecvData>>,
    send_data: Mutex<(SendState, Option<NonZeroU32>)>,
    serial: AtomicU32,
    auto_name: String,
}
impl RpcConn {
    async fn new(conn: Conn) -> std::io::Result<Self> {
        let recv_data = RecvData {
            state: conn.recv_state,
            reply_map: HashMap::new(),
            hierarchy: CallHierarchy::new(),
            sig_matches: Vec::new(),
        };
        let mut ret = Self {
            conn: Async::new(conn.stream)?,
            send_data: Mutex::new((conn.send_state, None)),
            recv_data: Arc::new(Mutex::new(recv_data)),
            recv_cond: Condvar::new(),
            serial: AtomicU32::new(1),
            auto_name: String::new(),
        };
        let hello_res = ret.send_msg(&hello()).await?.unwrap().await?;
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
    /// Returns the name assigned by the DBus daemon.
    /// This name was retreived using the `org.freedesktop.DBus.Hello` call when the connection was started.
    pub fn get_name(&self) -> &str {
        &self.auto_name
    }
    /// Connect to the system bus.
    ///
    /// If `with_fd` is true then sending and receiving file descriptors is enabled for this connection.
    /// # Notes
    /// * Like all the `RpcConn` constructors, this method handles sending the initial `org.freedesktop.DBus.Hello` message and handles the response.
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use async_rustbus::RpcConn;
    /// use async_rustbus::rustbus_core::message_builder::MessageBuilder;
    /// let conn = RpcConn::system_conn(false).await.unwrap();
    /// let mut msg = MessageBuilder::new().call("GetConnectionUnixProcessID")
    ///     .at("org.freedesktop.DBus")
    ///        .on("/org/freedesktop/DBus")
    ///     .with_interface("org.freedesktop.DBus")
    ///     .build();
    /// msg.body.push_param(conn.get_name()).unwrap();
    /// let res = conn.send_msg_w_rsp(&msg).await.unwrap().await.unwrap();
    /// let pid: u32 = res.body.parser().get().unwrap();
    /// assert_eq!(pid, std::process::id());
    /// # });
    /// ```
    pub async fn session_conn(with_fd: bool) -> std::io::Result<Self> {
        let addr = get_session_bus_addr().await?;
        Self::connect_to_addr(&addr, with_fd).await
    }
    /// Connect to the session (user) bus.
    ///
    /// If `with_fd` is true then sending and receiving file descriptors is enabled for this connection.
    /// # Notes
    /// * Like all the `RpcConn` constructors, this method handles sending the initial `org.freedesktop.DBus.Hello` message and handles the response.
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use async_rustbus::RpcConn;
    /// use async_rustbus::rustbus_core::message_builder::MessageBuilder;
    /// let conn = RpcConn::system_conn(false).await.unwrap();
    /// let mut msg = MessageBuilder::new().call("GetConnectionUnixProcessID")
    ///     .at("org.freedesktop.DBus")
    ///        .on("/org/freedesktop/DBus")
    ///     .with_interface("org.freedesktop.DBus")
    ///     .build();
    /// msg.body.push_param(conn.get_name()).unwrap();
    /// let res = conn.send_msg_w_rsp(&msg).await.unwrap().await.unwrap();
    /// let pid: u32 = res.body.parser().get().unwrap();
    /// assert_eq!(pid, std::process::id());
    /// # });
    /// ```
    pub async fn system_conn(with_fd: bool) -> std::io::Result<Self> {
        let addr = get_system_bus_addr().await?;
        //let path = get_system_bus_path().await?;
        Self::connect_to_addr(&addr, with_fd).await
    }
    /// Connect to the given address.
    ///
    /// This can be used to connect to a non-standard DBus daemon.
    /// If `with_fd` is true then sending and receiving file descriptors is enabled for this connection.
    /// In most instances users should use [`session_conn`] or [`system_conn`] to connecto to their local DBus instance.
    /// # Notes
    /// * Like all the `RpcConn` constructors, this method handles sending the initial `org.freedesktop.DBus.Hello` message and handles the response.
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use async_rustbus::{RpcConn, DBusAddr};
    /// use async_rustbus::rustbus_core::message_builder::MessageBuilder;
    /// let system_addr = DBusAddr::unix_path("/run/dbus/system_bus_socket");
    /// let conn = RpcConn::connect_to_addr(&system_addr, false).await.unwrap();
    /// let mut msg = MessageBuilder::new().call("GetConnectionUnixProcessID")
    ///     .at("org.freedesktop.DBus")
    ///        .on("/org/freedesktop/DBus")
    ///     .with_interface("org.freedesktop.DBus")
    ///     .build();
    /// msg.body.push_param(conn.get_name()).unwrap();
    /// let res = conn.send_msg_w_rsp(&msg).await.unwrap().await.unwrap();
    /// let pid: u32 = res.body.parser().get().unwrap();
    /// assert_eq!(pid, std::process::id());
    /// # });
    /// ```
    ///
    /// [`session_conn`]: ./struct.RpcConn.html#method.session_conn
    /// [`system_conn`]: ./struct.RpcConn.html#method.system_conn
    pub async fn connect_to_addr<P: AsRef<Path>, S: ToSocketAddrs, B: AsRef<[u8]>>(
        addr: &DBusAddr<P, S, B>,
        with_fd: bool,
    ) -> std::io::Result<Self> {
        let conn = Conn::connect_to_addr(addr, with_fd).await?;
        Ok(Self::new(conn).await?)
    }
    /// Connect to the given Unix sockect path.
    ///
    /// This can be used to connect to a non-standard DBus daemon.
    /// If `with_fd` is true then sending and receiving file descriptors is enabled for this connection.
    /// In most instances users should use [`session_conn`] or [`system_conn`] to connecto to their local DBus instance.
    /// # Notes
    /// * Like all the `RpcConn` constructors, this method handles sending the initial `org.freedesktop.DBus.Hello` message and handles the response.
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use async_rustbus::RpcConn;
    /// use async_rustbus::rustbus_core::message_builder::MessageBuilder;
    /// let conn = RpcConn::connect_to_path("/run/dbus/system_bus_socket", false).await.unwrap();
    /// let mut msg = MessageBuilder::new().call("GetConnectionUnixProcessID")
    ///     .at("org.freedesktop.DBus")
    ///        .on("/org/freedesktop/DBus")
    ///     .with_interface("org.freedesktop.DBus")
    ///     .build();
    /// msg.body.push_param(conn.get_name()).unwrap();
    /// let res = conn.send_msg_w_rsp(&msg).await.unwrap().await.unwrap();
    /// let pid: u32 = res.body.parser().get().unwrap();
    /// assert_eq!(pid, std::process::id());
    /// # });
    /// ```
    ///
    /// [`session_conn`]: ./struct.RpcConn.html#method.session_conn
    /// [`system_conn`]: ./struct.RpcConn.html#method.system_conn
    pub async fn connect_to_path<P: AsRef<Path>>(path: P, with_fd: bool) -> std::io::Result<Self> {
        let conn = Conn::connect_to_path(path, with_fd).await?;
        Ok(Self::new(conn).await?)
    }
    /*
    /// Set a filter that determines whether a signal should be dropped for received.
    ///
    /// If the filter returns `true` then the message is allowed to be received, otherwise it is dropped.
    /// The default signal filter when `RpcConn` is constructed is to drop all incoming signals.
    /// This default was chosen primarly to prevent leaking resources for unexpected signals sent specifically to this connection (by setting the destination header field).
    pub async fn set_sig_filter(
        &self,
        filter: Box<dyn Send + Sync + FnMut(&MarshalledMessage) -> bool>,
    ) {
        let mut recv_data = self.recv_data.lock().await;
        recv_data.sig_filter = filter;
    }
    */
    fn allocate_idx(&self) -> NonZeroU32 {
        let mut idx = 0;
        while idx == 0 {
            idx = self.serial.fetch_add(1, Ordering::Relaxed);
        }
        NonZeroU32::new(idx).unwrap()
    }
    /// Make a DBus call to a remote service or send a signal.
    ///
    /// This function returns a future nested inside a future.
    /// Awaiting the outer future sends the message out the DBus stream to the remote service.
    /// The inner future, returned by the outer, waits for the response from the remote service.
    /// # Notes
    /// * If the message sent was a signal or has the NO_REPLY_EXPECTED flag set then the inner future will
    ///   return immediatly when awaited.
    /// * If two futures are simultanously being awaited (like via `futures::future::join` or across tasks) then outgoing order of messages is not guaranteed.
    /// * If other incoming message are received before a response is received, then they will be processed by this future while awaiting.
    /// This processing may include placing messages in their correct queue or sending simple responses out the connection.
    ///
    pub async fn send_msg(
        &self,
        msg: &MarshalledMessage,
    ) -> std::io::Result<Option<impl Future<Output = std::io::Result<MarshalledMessage>> + '_>>
    {
        let idx = self.allocate_idx();
        let msg_res = if expects_reply(msg) {
            let recv = self.get_recv_and_insert_sender(idx).await;
            Some(recv)
        } else {
            None
        };
        self.send_msg_loop(msg, idx).await?;
        Ok(msg_res.map(|r| ResponseFuture {
            idx,
            rpc_conn: self,
            fut: self.wait_for_response(idx, r).boxed(),
        }))
    }
    async fn send_msg_loop(&self, msg: &MarshalledMessage, idx: NonZeroU32) -> std::io::Result<()> {
        let mut send_idx = None;
        loop {
            let mut send_lock = self.send_data.lock().await;
            let stream = self.conn.get_ref();
            match send_idx {
                Some(send_idx) => {
                    if send_lock.0.current_idx() > send_idx {
                        return Ok(());
                    }
                    let new_idx = send_lock.0.finish_sending_next(stream)?;
                    if new_idx > send_idx {
                        return Ok(());
                    }
                }
                None => {
                    send_idx = match send_lock.0.write_next_message(stream, msg, idx) {
                        Ok(si) => si,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                        Err(e) => return Err(e),
                    };
                    if send_idx.is_none() {
                        return Ok(());
                    }
                }
            }
            drop(send_lock);
            self.conn.writable().await?;
        }
    }
    /// Sends a signal or make a call to a remote service with the NO_REPLY_EXPECTED flag set.
    ///
    /// # Notes
    /// * If multiple send futures are simultanously being awaited (like via `futures::future::join` or across tasks) then outgoing order of messages is not guaranteed.
    /// # Panics
    /// * Panics if the message given expects a reply.
    pub async fn send_msg_wo_rsp(&self, msg: &MarshalledMessage) -> std::io::Result<()> {
        assert!(!expects_reply(msg));
        let idx = self.allocate_idx();
        self.send_msg_loop(msg, idx).await
    }
    /// Make a call to a remote service.
    ///
    /// `msg` must be a message the expects a call otherwise this method will panic.
    ///
    /// # Notes
    /// * If multiple send futures are simultanously being awaited (like via `futures::future::join` or across tasks) then outgoing order of messages is not guaranteed.
    /// * If other incoming message are received before a response is received, then they will be processed by this future while awaiting.
    /// This processing may include placing messages in their correct queue or sending simple responses out the connection.
    /// # Panics
    /// * Panics if the message does not expect a reply, such as signals or calls with the NO_REPLY_EXPECTED set.
    pub async fn send_msg_w_rsp(
        &self,
        msg: &MarshalledMessage,
    ) -> std::io::Result<impl Future<Output = std::io::Result<MarshalledMessage>> + '_> {
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
                    Some(res_idx) => res_idx,
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
        let mut recv_fut = self.recv_data.lock().boxed();
        loop {
            match select(msg_fut, recv_fut).await {
                Either::Left((msg, _)) => {
                    let msg = msg.unwrap();
                    return Ok(msg);
                }
                Either::Right((mut recv_lock, msg_f)) => {
                    msg_fut = msg_f;
                    match self.queue_msg(&mut recv_lock, res_pred) {
                        Ok((msg, bad)) => {
                            if bad {
                                drop(recv_lock);
                                let res = msg.dynheader.make_error_response("UnknownObject", None);
                                self.send_msg_wo_rsp(&res).await?;
                            } else {
                                self.recv_cond.notify_all();
                                return Ok(msg);
                            }
                            recv_fut = self.recv_data.lock().boxed();
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            let read_fut = self.conn.readable();
                            let listener = self.recv_cond.wait(recv_lock);
                            pin_mut!(listener);
                            recv_fut = match select(read_fut, listener).await {
                                Either::Left((_, l)) => {
                                    drop(l);
                                    self.recv_data.lock().boxed()
                                },
                                Either::Right((recv_lock, _)) => ready(recv_lock).boxed()
                            };
                        }
                        Err(e) => {
                            self.recv_cond.notify_all();
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
    /// Add a match to retreive signals.
    ///
    /// A `org.freedesktop.DBus.AddMatch` call is made to tell the DBus daemon to route matching signals to this connection.
    /// These signals are stored by the `RpcConn` and can be retreived by using the [`get_signal`] method.
    /// If a message is received that matches multiple `sig_match`es, then the message is associated with the most specific specific [`MatchRule`].
    /// See [`MatchRule`] for more details.
    ///
    /// # Panics
    /// * Panics if both the path and path_namespace matching parameters are used in the `MatchRule`.
    ///
    /// # Examples
    /// ```
    /// # async_std::task::block_on(async {
    /// use async_rustbus::{RpcConn, MatchRule};
    /// let conn = RpcConn::session_conn(false).await.unwrap();
    /// let rule = MatchRule::new()
    ///     .sender("org.freedesktop.DBus")
    ///     .interface("org.freedesktop.DBus")
    ///     .member("NameOwnerChanged").clone();
    /// conn.insert_sig_match(&rule).await.unwrap();
    /// let owned = conn.request_name("example.name").await.unwrap();
    /// if owned {
    ///     let msg = conn.get_signal(&rule).await.unwrap();
    ///     let (_, _, new_owner): (&str, &str, &str)  = msg.body.parser().get3().unwrap();
    ///     assert_eq!(new_owner, conn.get_name());
    /// }
    /// conn.remove_sig_match(&rule).await.unwrap();
    /// # });
    /// ```
    ///
    /// [`get_signal`]: ./struct.RpcConn.html#method.get_signal
    /// [`set_sig_filter`]: ./struct.RpcConn.html#method.set_sig_filter
    /// [`MatchRule`]: ./struct.MatchRule.html
    pub async fn insert_sig_match(&self, sig_match: &MatchRule) -> std::io::Result<()> {
        assert!(!(sig_match.path.is_some() && sig_match.path_namespace.is_some()));
        let mut recv_data = self.recv_data.lock().await;
        let insert_idx = match recv_data.sig_matches.binary_search(sig_match) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "Already exists",
                ))
            }
            Err(i) => i,
        };
        let mut to_insert = sig_match.clone();
        to_insert.queue = Some(MsgQueue::new());
        recv_data.sig_matches.insert(insert_idx, to_insert);
        drop(recv_data);
        let match_str = sig_match.match_string();
        let call = rustbus_core::standard_messages::add_match(&match_str);
        let res = self.send_msg_w_rsp(&call).await?.await?;
        match res.typ {
            MessageType::Reply => Ok(()),
            MessageType::Error => {
                let mut recv_data = self.recv_data.lock().await;
                if let Ok(idx) = recv_data.sig_matches.binary_search(sig_match) {
                    recv_data.sig_matches.remove(idx);
                }
                let err_str: &str = res
                    .body
                    .parser()
                    .get()
                    .unwrap_or("Unknown DBus Error Type!");
                Err(std::io::Error::new(ErrorKind::Other, err_str))
            }
            _ => unreachable!(),
        }
    }
    /// Stop the reception of messages matching `sig_match`
    ///
    /// This method calls `org.freedesktop.DBus.RemoveMatch` to stop the reception of matching signals.
    /// Any messages already received that haven't been retreived are lost.
    /// This method will return an `InavalidInput` if the `MatchRule` is not already present in the `RpcConn`.
    ///
    /// # Examples
    /// See [`insert_sig_path`] for an example.
    ///
    /// [`insert_sig_path`]: ./struct.RpcConn.html#method.insert_sig_path
    pub async fn remove_sig_match(&self, sig_match: &MatchRule) -> std::io::Result<()> {
        let mut recv_data = self.recv_data.lock().await;
        let idx = match recv_data.sig_matches.binary_search(sig_match) {
            Err(_) => {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "MatchRule doesn't exist!",
                ))
            }
            Ok(i) => i,
        };
        recv_data.sig_matches.remove(idx);
        drop(recv_data);
        let match_str = sig_match.match_string();
        let call = rustbus_core::standard_messages::remove_match(&match_str);
        let res = self.send_msg_w_rsp(&call).await?.await?;
        match res.typ {
            MessageType::Reply => Ok(()),
            MessageType::Error => {
                let err_str: &str = res
                    .body
                    .parser()
                    .get()
                    .unwrap_or("Unknown DBus Error Type!");
                Err(std::io::Error::new(ErrorKind::Other, err_str))
            }
            _ => unreachable!(),
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
            /*if matches!(msg.typ, MessageType::Signal) && !(recv_data.sig_filter)(&msg) {
                continue;
            }*/
            if pred(&msg, recv_data) {
                return Ok((msg, false));
            } else {
                match &msg.typ {
                    MessageType::Signal => queue_sig(&recv_data.sig_matches, msg),
                    MessageType::Reply | MessageType::Error => {
                        let idx = msg
                            .dynheader
                            .response_serial
                            .expect("Reply should always have a response serial!");
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
            std::io::Error::new(
                ErrorKind::InvalidInput,
                "Invalid message path or signal match given!",
            )
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
                    self.recv_cond.notify_all();
                    return Ok(msg);
                }
                Either::Right((mut recv_lock, msg_f)) => {
                    match self.queue_msg(&mut recv_lock, &pred) {
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            msg_fut = msg_f;
                            let read_fut = self.conn.readable();
                            let listener = self.recv_cond.wait(recv_lock);
                            pin_mut!(listener);
                            recv_fut = match select(read_fut, listener).await {
                                Either::Left((_, l)) => {
                                    drop(l);     
                                    self.recv_data.lock().boxed()
                                },
                                Either::Right((recv_lock, _)) => ready(recv_lock).boxed()
                            };
                        }
                        Err(e) => {
                            self.recv_cond.notify_all();
                            return Err(e);
                        }
                        Ok((msg, bad)) => {
                            if bad {
                                drop(recv_lock);
                                self.send_msg_wo_rsp(&msg).await?;
                                recv_fut = self.recv_data.lock().boxed();
                                msg_fut = msg_f;
                            } else {
                                self.recv_cond.notify_all();
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
    /// Use the same `sig_match` used with [`insert_sig_match`] to wait for its associated signals.
    /// Signals with this connection as a destination are always sent to this connection regardless
    /// of if there is an matching [`MatchRule`]. If these signals are not filtered out and do not match
    /// a given filter they can be retreived uisng the default `MatchRule`.
    /// # Notes
    /// * While awaiting for a matching signal, this future will process other incoming messages.
    /// This processing may include placing messages in their correct queue or sending simple responses out the connection.
    /// # Examples
    /// See [`insert_sig_match`] for an example.
    ///
    /// [`insert_sig_match`]: ./struct.RpcConn.html#method.insert_sig_match
    pub async fn get_signal(&self, sig_match: &MatchRule) -> std::io::Result<MarshalledMessage> {
        let sig_queue = |recv_data: &mut RecvData| {
            eprintln!("{:?}", sig_match);
            eprintln!("{:?}", recv_data.sig_matches);
            let idx = recv_data.sig_matches.binary_search(sig_match).ok()?;
            Some(
                recv_data.sig_matches[idx]
                    .queue
                    .as_ref()
                    .unwrap()
                    .get_receiver(),
            )
        };
        let sig_pred = |msg: &MarshalledMessage, _: &mut RecvData| sig_match.matches(msg);
        self.get_msg(sig_queue, sig_pred).await
    }
    /// Gets the next call associated with the given path.
    ///
    /// Use `insert_call_path` to setup the `RpcConn` for receiving calls.
    /// An `InvalidInput` error is returned if the path does not have an associated queue.
    /// # Notes
    /// * While awaiting for a matching call, this future will process other incoming messages.
    /// This processing may include placing messages in their correct queue or sending simple responses out the connection.
    ///
    /// [`insert_call_path`]: ./struct.RpcConn.html#method.insert_call_path
    pub async fn get_call<'a, S, D>(&self, path: S) -> std::io::Result<MarshalledMessage>
    where
        S: TryInto<&'a ObjectPath, Error = D>,
        D: std::fmt::Debug,
    {
        let path = path.try_into().map_err(|e| {
            std::io::Error::new(ErrorKind::InvalidInput, format!("Invalid path: {:?}", e))
        })?;
        let call_queue =
            |recv_data: &mut RecvData| Some(recv_data.hierarchy.get_queue(path)?.get_receiver());
        let call_pred = |msg: &MarshalledMessage, recv_data: &mut RecvData| match &msg.typ {
            MessageType::Call => {
                let msg_path =
                    ObjectPath::from_str(msg.dynheader.object.as_ref().unwrap()).unwrap();
                recv_data.hierarchy.is_match(path, msg_path)
            }
            _ => false,
        };
        self.get_msg(call_queue, call_pred).await
    }
    /// Configure what action the `RpcConn` should take when receiving calls for a path or namespace.
    ///
    /// See [`CallAction`] for details on what each action does.
    /// The default action before any `insert_call_path` calls is to drop all incoming messsage calls and reply with an error.
    ///
    /// [`CallAction`]: ./enum.CallAction.html
    pub async fn insert_call_path<'a, S, D>(&self, path: S, action: CallAction) -> Result<(), D>
    where
        S: TryInto<&'a ObjectPath, Error = D>,
    {
        let path = path.try_into()?;
        let mut recv_data = self.recv_data.lock().await;
        recv_data.hierarchy.insert_path(path, action);
        Ok(())
    }
    /// Get the action for a path.
    /// # Returns
    /// Returns the action if there is one for that path.
    /// If there is no action for the given path or the path is invalid `None` is returned.
    pub async fn get_call_path_action<'a, S: TryInto<&'a ObjectPath>>(
        &self,
        path: S,
    ) -> Option<CallAction> {
        let path = path.try_into().ok()?;
        let recv_data = self.recv_data.lock().await;
        recv_data.hierarchy.get_action(path)
    }
    /// Get a receiver for the incoming call queue if there is one.
    ///
    /// This receiver will produce calls from this path/namespace.
    /// Receiving messages from the `Receiver` doesn't actually cause the `RpcConn` to do any work.
    /// It will only produce messages if another future from `get_signal` or `get_call` is being worked on.
    /// This method can be useful if you know that there are other tasks working using this `RpcConn` that will do the work of processing incoming messages.
    pub async fn get_call_recv<'a, S: TryInto<&'a ObjectPath>>(
        &self,
        path: S,
    ) -> Option<CReceiver<MarshalledMessage>> {
        let path = path.try_into().ok()?;
        let recv_data = self.recv_data.lock().await;
        Some(recv_data.hierarchy.get_queue(path)?.get_receiver())
    }
    /// Request a name from the DBus daemon.
    ///
    /// Returns `true` if the name was successfully obtained or if it was already owned by this connection.
    /// otherwise `false` is returned (assuming an IO error did not occur).
    /// The `DO_NOT_QUEUE` flag is used with the request so if the name is not available, this connection will not be queued to own it in the future.
    pub async fn request_name(&self, name: &str) -> std::io::Result<bool> {
        let req = request_name(name, DBUS_NAME_FLAG_DO_NOT_QUEUE);
        let res = self.send_msg_w_rsp(&req).await?.await?;
        if MessageType::Error == res.typ {
            return Ok(false);
        }
        Ok(match res.body.parser().get::<u32>() {
            Ok(ret) => matches!(
                ret,
                DBUS_REQUEST_NAME_REPLY_ALREADY_OWNER | DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER
            ),
            Err(_) => false,
        })
    }
    /// Release a name from the DBus daemon.
    ///
    /// An `Err` is only returned on a IO error.
    /// If the name was not owned by the connection, or the name was invalid `Ok` is still returned.
    pub async fn release_name(&self, name: &str) -> std::io::Result<()> {
        let rel_name = release_name(&name);
        self.send_msg_w_rsp(&rel_name).await?.await?;
        Ok(())
    }
}
impl AsRawFd for RpcConn {
    fn as_raw_fd(&self) -> RawFd {
        self.conn.as_raw_fd()
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
