use std::collections::VecDeque;
use std::io::IoSliceMut;
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::process::id;
use std::sync::Arc;

use futures::prelude::*;

use async_std::os::unix::net::UnixStream;
use async_std::path::{Path, PathBuf};
use async_std::sync::Mutex;
use std::io::ErrorKind;

use super::rustbus_core;

use rustbus_core::message_builder::MarshalledMessage;

mod ancillary;
mod recv;
mod sender;

use recv::InState;
pub(crate) use recv::{Receiver, RecvState};

pub(crate) use sender::Sender;
use sender::{OutState, SendState};

use ancillary::{
    recv_vectored_with_ancillary, send_vectored_with_ancillary, AncillaryData, SocketAncillary,
};

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
        Err(std::io::Error::new(
            ErrorKind::NotFound,
            "Could not find system bus.",
        ))
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
            ErrorKind::NotFound,
            format!("Could not find session bus at {:?}.", path),
        ))
    }
}

/// Generic stream
pub(crate) struct GenStream {
    fd: RawFd,
}

impl AsRawFd for GenStream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}
impl FromRawFd for GenStream {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self { fd }
    }
}
impl GenStream {
    fn recv_vectored_with_ancillary(
        &self,
        bufs: &mut [IoSliceMut<'_>],
        ancillary: &mut SocketAncillary<'_>,
    ) -> std::io::Result<usize> {
        recv_vectored_with_ancillary(self.as_raw_fd(), bufs, ancillary)
    }
    fn send_vectored_with_ancillary(
        &self,
        bufs: &mut [IoSliceMut<'_>],
        ancillary: &mut SocketAncillary<'_>,
    ) -> std::io::Result<usize> {
        send_vectored_with_ancillary(self.as_raw_fd(), bufs, ancillary)
    }
    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        let how = match how {
            Shutdown::Read => libc::SHUT_RD,
            Shutdown::Write => libc::SHUT_WR,
            Shutdown::Both => libc::SHUT_RDWR,
        };
        unsafe {
            if libc::shutdown(self.as_raw_fd(), how) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }
}
impl Drop for GenStream {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
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
    stream: GenStream,
    recv_state: RecvState,
    send_state: SendState,
}

impl Conn {
    async fn connect_to_path_byteorder<P: AsRef<Path>>(
        p: P,
        with_fd: bool,
    ) -> std::io::Result<Self> {
        //let stream = Async<
        let mut stream = UnixStream::connect(p).await?;
        if !do_auth(&mut stream).await? {
            return Err(std::io::Error::new(
                ErrorKind::ConnectionAborted,
                "Auth failed!",
            ));
        }
        if with_fd {
            if !negotiate_unix_fds(&mut stream).await? {
                return Err(std::io::Error::new(
                    ErrorKind::ConnectionAborted,
                    "Failed to negotiate Unix FDs!",
                ));
            }
        }
        stream.write_all(b"BEGIN\r\n").await?;
        // SAFETY: into_raw_fd() gets an "owned" fd
        // that can be taken by the StdUnixStream.
        let stream = unsafe {
            let fd = stream.into_raw_fd();
            StdUnixStream::from_raw_fd(fd)
        };
        let stream = unsafe { GenStream::from_raw_fd(stream.into_raw_fd()) };
        Ok(Self {
            recv_state: RecvState {
                in_state: InState::Header(Vec::new()),
                in_fds: Vec::new(),
                with_fd,
                remaining: VecDeque::new(),
            },
            send_state: SendState {
                out_state: OutState::default(),
                serial: 0,
                with_fd,
            },
            stream,
        })
    }
    pub async fn connect_to_path<P: AsRef<Path>>(p: P, with_fd: bool) -> std::io::Result<Self> {
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
            state: Mutex::new(self.send_state),
        };
        let receiver = Receiver {
            stream,
            state: Mutex::new(self.recv_state),
        };
        (sender, receiver)
    }
    pub fn get_next_message(&mut self) -> std::io::Result<MarshalledMessage> {
        self.recv_state.get_next_message(&self.stream)
    }
    pub fn finish_sending_next(&mut self) -> std::io::Result<()> {
        self.send_state.finish_sending_next(&self.stream)
    }
    pub fn write_next_message(
        &mut self,
        msg: &MarshalledMessage,
    ) -> std::io::Result<(bool, Option<u32>)> {
        self.send_state.write_next_message(&self.stream, msg)
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
