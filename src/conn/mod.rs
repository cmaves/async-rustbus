use std::collections::VecDeque;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{AncillaryData, SocketAncillary, UnixStream as StdUnixStream};
use std::process::id;
use std::sync::Arc;

use futures::prelude::*;

use async_std::os::unix::net::UnixStream;
use std::io::ErrorKind;
use async_std::path::{Path, PathBuf};

use super::rustbus_core;

use rustbus_core::message_builder::MarshalledMessage;
use rustbus_core::wire::unixfd::UnixFd;

mod recv;
mod sender;
pub(crate) use recv::Receiver;
use recv::{get_next_message, InState};
pub(crate) use sender::Sender;
use sender::{finish_sending_next, write_next_message, OutState};

const DBUS_SYS_PATH: &'static str = "/run/dbus/system_bus_socket";
const DBUS_SESS_ENV: &'static str = "DBUS_SESSION_BUS_ADDRESS";
const DBUS_LINE_END_STR: &'static str = "\r\n";
const DBUS_LINE_END: &'static [u8] = DBUS_LINE_END_STR.as_bytes();
const DBUS_MAX_FD_MESSAGE: usize = 32;

pub async fn get_system_bus_path() -> std::io::Result<&'static Path> {
    let path = Path::new(DBUS_SYS_PATH);
    if path.exists().await {
        Ok(path)
    } else {
        Err(std::io::Error::new(ErrorKind::NotFound, "Could not find system bus."))
    }
}

pub async fn get_session_bus_path() -> std::io::Result<PathBuf> {
    let path: PathBuf = std::env::var_os(DBUS_SESS_ENV)
        .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "No DBus session in environment."))?
        .into();
    if path.exists().await {
        Ok(path)
    } else {
        Err(std::io::Error::new(
            ErrorKind::NotFound, format!("Could not find session bus at {:?}.", path)
        ))
    }
}
/// A synchronous non-blocking connection to DBus session.
///
/// Most people will want to use `RpcConn`. This is a low-level
/// struct used by `RpcConn` to read and write messages to/from the DBus
/// socket. It does minimal processing of data and provides no Async interfaces.
/// # Notes
/// * If you are interested in synchronous interface for DBus, the `rustbus` is a better solution.
pub struct Conn {
    stream: StdUnixStream,
    incoming: VecDeque<u8>,
    in_fds: Vec<UnixFd>,
    in_state: InState,
    out_state: OutState,
    with_fd: bool,
    serial: u32,
}

impl Conn {
    async fn connect_to_path_byteorder<P: AsRef<Path>>(
        p: P,
        with_fd: bool,
    ) -> std::io::Result<Self> {
        //let stream = Async<
        let mut stream = UnixStream::connect(p).await?;
        if !do_auth(&mut stream).await? {
            return Err(std::io::Error::new(ErrorKind::ConnectionAborted, "Auth failed!"));
        }
        if with_fd {
            if !negotiate_unix_fds(&mut stream).await? {
                return Err(std::io::Error::new(ErrorKind::ConnectionAborted, "Failed to negotiate Unix FDs!"));
            }
        }
        stream.write_all(b"BEGIN\r\n").await?;
        // SAFETY: into_raw_fd() gets an "owned" fd
        // that can be taken by the StdUnixStream.
        let stream = unsafe {
            let fd = stream.into_raw_fd();
            StdUnixStream::from_raw_fd(fd)
        };
        Ok(Self {
            incoming: VecDeque::new(),
            in_state: InState::Header(Vec::new()),
            out_state: OutState::default(),
            serial: 0,
            stream,
            with_fd,
            in_fds: Vec::new(),
        })
    }
    pub async fn connect_to_path<P: AsRef<Path>>(
        p: P,
        with_fd: bool,
    ) -> std::io::Result<Self> {
        /*
        #[cfg(target_endian = "little")]
        let endian = ByteOrder::LittleEndian;
        #[cfg(target_endian = "big")]
        let endian = ByteOrder::BigEndian;
        */

        Self::connect_to_path_byteorder(p, with_fd).await
    }
    pub(crate) fn split(self) -> (Sender, Receiver) {
        let stream = Arc::new(self.stream);
        let sender = Sender {
            stream: stream.clone(),
            out_state: self.out_state,
            serial: self.serial,
            with_fd: self.with_fd,
        };
        let receiver = Receiver {
            stream,
            in_state: self.in_state,
            remaining: self.incoming,
            in_fds: self.in_fds,
            with_fd: self.with_fd,
        };
        (sender, receiver)
    }
    pub fn get_next_message(&mut self) -> std::io::Result<MarshalledMessage> {
        get_next_message(
            &self.stream,
            &mut self.in_fds,
            &mut self.in_state,
            &mut self.incoming,
            self.with_fd,
        )
    }
    pub fn finish_sending_next(&mut self) -> std::io::Result<()> {
        finish_sending_next(&self.stream, &mut self.out_state)
    }
    pub fn write_next_message(
        &mut self,
        msg: &MarshalledMessage,
    ) -> std::io::Result<(bool, Option<u32>)> {
        write_next_message(
            &self.stream,
            &mut self.out_state,
            &mut self.serial,
            self.with_fd,
            msg,
        )
    }
}
impl AsRawFd for Conn {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

fn find_line_ending(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == DBUS_LINE_END)
}
async fn starts_with(buf: &[u8], stream: &mut UnixStream) -> std::io::Result<bool> {
    debug_assert!(buf.len() <= 510);
    let mut pos = 0;
    let mut read_buf = [0; 512];
    // get at least enough bytes so we can check for if it starts with buf
    while pos < buf.len() {
        pos += stream.read(&mut read_buf[pos..]).await?;
    }
    if !read_buf.starts_with(buf) {
        return Ok(false);
    }
    while let None = find_line_ending(&read_buf[..pos]) {
        if pos == 512 {
            // It took too long to receive the line ending
            return Ok(false);
        }
        pos += stream.read(&mut read_buf[pos..]).await?;
    }
    Ok(true)
}
async fn do_auth(stream: &mut UnixStream) -> std::io::Result<bool> {
    let to_write = format!("\0AUTH EXTERNAL {:X}\r\n", id());
    stream.write_all(to_write.as_bytes()).await?;
    starts_with(b"OK", stream).await
}
async fn negotiate_unix_fds(stream: &mut UnixStream) -> std::io::Result<bool> {
    stream.write_all(b"NEGOTIATE_UNIX_FD\r\n").await?;
    starts_with(b"AGREE_UNIX_FD", stream).await
}
