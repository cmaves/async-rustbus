use std::collections::{HashSet, VecDeque};
use std::io::IoSliceMut;
use std::mem;
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use futures::io::{AsyncRead, AsyncWrite};
use futures::prelude::*;

use async_std::net::{TcpStream, ToSocketAddrs};
use async_std::os::unix::net::UnixStream;
use async_std::path::Path;
use async_std::sync::Mutex;
use std::io::ErrorKind;

use super::rustbus_core;

use rustbus_core::message_builder::MarshalledMessage;

mod ancillary;

mod addr;
pub use addr::{get_session_bus_addr, get_system_bus_path, DBusAddr, DBUS_SESS_ENV, DBUS_SYS_PATH};
mod recv;
use recv::InState;
pub(crate) use recv::RecvState;

mod sender;
use sender::{OutState, SendState};

use ancillary::{
    recv_vectored_with_ancillary, send_vectored_with_ancillary, AncillaryData, SocketAncillary,
};

const DBUS_LINE_END_STR: &'static str = "\r\n";
const DBUS_LINE_END: &'static [u8] = DBUS_LINE_END_STR.as_bytes();
const DBUS_MAX_FD_MESSAGE: usize = 32;

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
    pub fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let bufs = &mut [IoSliceMut::new(buf)];
        let mut ancillary = SocketAncillary::new(&mut []);
        recv_vectored_with_ancillary(self.as_raw_fd(), bufs, &mut ancillary)
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
fn fd_or_os_err(fd: i32) -> std::io::Result<i32> {
    if fd == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}
// TODO: if https://github.com/async-rs/async-std/pull/961
// is completed then perhaps this trait can be elimnated
trait ToRawFd {
    fn to_raw_fd(self) -> std::io::Result<RawFd>;
}
impl<T: AsRawFd> ToRawFd for T {
    fn to_raw_fd(self) -> std::io::Result<RawFd> {
        let fd = self.as_raw_fd();
        unsafe { fd_or_os_err(libc::dup(fd)) }
    }
}
impl Conn {
    async fn conn_handshake<T>(mut stream: T, with_fd: bool) -> std::io::Result<Self>
    where
        T: AsyncRead + AsyncWrite + Unpin + ToRawFd,
    {
        do_auth(&mut stream).await?;
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
            let fd = stream.to_raw_fd()?;
            GenStream::from_raw_fd(fd)
        };
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
    pub async fn connect_to_addr<P: AsRef<Path>, S: ToSocketAddrs>(
        addr: &DBusAddr<P, S>,
        with_fd: bool,
    ) -> std::io::Result<Self> {
        match addr {
            DBusAddr::Path(p) => Self::conn_handshake(UnixStream::connect(p).await?, with_fd).await,
            DBusAddr::Tcp(s) => {
                if with_fd {
                    Err(std::io::Error::new(
                        ErrorKind::InvalidInput,
                        "Cannot use Fds over TCP.",
                    ))
                } else {
                    Self::conn_handshake(TcpStream::connect(s).await?, with_fd).await
                }
            }
            #[cfg(target_os = "linux")]
            DBusAddr::Abstract(buf) => unsafe {
                let mut addr: libc::sockaddr_un = mem::zeroed();
                addr.sun_family = libc::AF_UNIX as u16;
                // SAFETY: &[u8] has identical memory layout and size to &[i8]
                let i8_buf: &[i8] = mem::transmute(buf as &[u8]);
                addr.sun_path
                    .get_mut(1..1 + buf.len())
                    .ok_or_else(|| {
                        std::io::Error::new(
                            ErrorKind::InvalidData,
                            "Abstract unix socket address was too long!",
                        )
                    })?
                    .copy_from_slice(i8_buf);
                //SAFETY: errors are apporiately handled
                let fd = fd_or_os_err(libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0))?;
                if let Err(e) = fd_or_os_err(libc::connect(
                    fd,
                    &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                    (mem::size_of_val(&addr) - (108 - buf.len() - 1)) as u32,
                )) {
                    libc::close(fd);
                    return Err(e);
                }
                let stream = UnixStream::from_raw_fd(fd);
                Self::conn_handshake(stream, with_fd).await
            },
        }
    }
    async fn connect_to_path_byteorder<P: AsRef<Path>>(
        p: P,
        with_fd: bool,
    ) -> std::io::Result<Self> {
        let addr: DBusAddr<P, &str> = DBusAddr::Path(p);
        Self::connect_to_addr(&addr, with_fd).await
    }
    pub async fn connect_to_path<P: AsRef<Path>>(p: P, with_fd: bool) -> std::io::Result<Self> {
        Self::connect_to_path_byteorder(p, with_fd).await
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
async fn starts_with<T: AsyncRead + AsyncWrite + Unpin>(
    buf: &[u8],
    stream: &mut T,
) -> std::io::Result<Option<Vec<u8>>> {
    debug_assert!(buf.len() <= 510);
    let mut pos = 0;
    let mut read_buf = [0; 512];
    loop {
        match find_line_ending(&read_buf[..pos]) {
            Some(loc) => {
                if buf.len() > loc {
                    return Ok(None);
                }
                return if &read_buf[..buf.len()] == buf {
                    Ok(Some(read_buf[buf.len()..loc].to_owned()))
                } else {
                    Ok(None)
                };
            }
            None => {
                if pos == 512 {
                    // Line was too long.
                    return Ok(None);
                }
                pos += stream.read(&mut read_buf[pos..]).await?;
            }
        }
    }
}
async fn find_auth_mechs<T: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut T,
) -> std::io::Result<HashSet<String>> {
    stream.write_all(b"AUTH\r\n").await?;
    let ret = starts_with(b"REJECTED", stream).await?;
    match ret {
        Some(s) if s.len() == 0 => Ok(HashSet::new()),
        Some(s) => {
            let s = std::str::from_utf8(&s[..]).map_err(|_| {
                std::io::Error::new(
                    ErrorKind::PermissionDenied,
                    "Invalid AUTH response from remote!",
                )
            })?;

            Ok(s.split(" ").map(|s| s.to_owned()).collect())
        }
        None => Ok(HashSet::new()),
    }
}
async fn await_ok<T: AsyncRead + AsyncWrite + Unpin>(stream: &mut T) -> std::io::Result<()> {
    match starts_with(b"OK", stream).await? {
        Some(_) => Ok(()),
        None => Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "External authentication failed with remote!",
        )),
    }
}
async fn do_external_auth<T: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut T,
) -> std::io::Result<()> {
    let mut to_write = Vec::from(&b"AUTH EXTERNAL "[..]);
    let mut pid = unsafe { libc::geteuid() };
    let mut order = 1;
    loop {
        let next = order * 10;
        if pid / next == 0 {
            break;
        }
        order = next;
    }
    while order > 0 {
        to_write.push(b'3');
        let digit = pid / order;
        to_write.push(0x30 + digit as u8);
        pid -= digit * order;
        order /= 10;
    }
    to_write.extend_from_slice(DBUS_LINE_END);
    stream.write_all(&to_write).await?;
    await_ok(stream).await
}
async fn do_anon_auth<T: AsyncRead + AsyncWrite + Unpin>(stream: &mut T) -> std::io::Result<()> {
    stream.write_all(b"AUTH ANONYMOUS\r\n").await?;
    await_ok(stream).await
}
async fn do_auth<T: AsyncRead + AsyncWrite + Unpin>(stream: &mut T) -> std::io::Result<()> {
    stream.write_all(b"\0").await?;
    let auth_mechs = find_auth_mechs(stream).await?;
    let mut err = None;
    if auth_mechs.contains("EXTERNAL") {
        match do_external_auth(stream).await {
            Ok(_) => return Ok(()),
            Err(e) => err = Some(e),
        }
    }
    if auth_mechs.contains("ANONYMOUS") {
        match do_anon_auth(stream).await {
            Ok(_) => return Ok(()),
            Err(e) => err = Some(e),
        }
    }
    match err {
        Some(err) => Err(err),
        None => Err(std::io::Error::new(
            ErrorKind::PermissionDenied,
            "Remote doesn't support our auth methods!",
        )),
    }
}
async fn negotiate_unix_fds<T: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut T,
) -> std::io::Result<bool> {
    stream.write_all(b"NEGOTIATE_UNIX_FD\r\n").await?;
    starts_with(b"AGREE_UNIX_FD", stream)
        .await
        .map(|o| o.is_some())
}

pub(super) struct AsyncConn {
    pub(super) stream: GenStream,
    pub(super) recv_state: Mutex<RecvState>,
    pub(super) send_state: Mutex<SendState>,
}

impl AsyncConn {}
impl AsRawFd for AsyncConn {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

impl From<Conn> for AsyncConn {
    fn from(conn: Conn) -> Self {
        Self {
            stream: conn.stream,
            recv_state: Mutex::new(conn.recv_state),
            send_state: Mutex::new(conn.send_state),
        }
    }
}
