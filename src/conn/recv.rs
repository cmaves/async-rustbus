use std::collections::VecDeque;
use std::io::{ErrorKind, IoSliceMut, Read, Write};
use std::mem;
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{AncillaryData, SocketAncillary, UnixStream as StdUnixStream};
use std::sync::Arc;

use crate::rustbus_core;
use rustbus::wire::unmarshal;
use rustbus::wire::util::align_offset;
use rustbus_core::message_builder::{DynamicHeader, MarshalledMessage, MessageType};
use rustbus_core::sync_conn;
use rustbus_core::wire::unixfd::UnixFd;
use unmarshal::{UnmarshalResult, HEADER_LEN};

use crate::utils::{align_num, extend_max, lazy_drain, parse_u32};

use super::DBUS_MAX_FD_MESSAGE;

pub enum InState {
    Header(Vec<u8>),
    DynHdr(unmarshal::Header, Vec<u8>),
    Finishing(unmarshal::Header, DynamicHeader, Vec<u8>),
}

impl Default for InState {
    fn default() -> Self {
        InState::Header(Vec::new())
    }
}

impl InState {
    fn to_buf(self) -> Vec<u8> {
        let mut ret = match self {
            InState::Header(b) => b,
            InState::DynHdr(_, b) => b,
            InState::Finishing(_, _, b) => b,
        };
        ret.clear();
        ret
    }
    fn to_hdr(self) -> Self {
        let buf = self.to_buf();
        InState::Header(buf)
    }
    fn bytes_needed_for_next(&self) -> usize {
        match self {
            InState::Header(b) => HEADER_LEN + 4 - b.len(),
            InState::DynHdr(hdr, b) => {
                if b.len() < 4 {
                    4 - b.len()
                } else {
                    let array_len = parse_u32(&b[..4], hdr.byteorder).unwrap() as usize;
                    4 + array_len - b.len()
                }
            }
            InState::Finishing(hdr, _, b) => hdr.body_len as usize - 4,
        }
    }
}

fn try_get_msg<I>(
    stream: &StdUnixStream,
    in_state: &mut InState,
    in_fds: &mut Vec<UnixFd>,
    with_fd: bool,
    new: I,
) -> std::io::Result<Option<MarshalledMessage>>
where
    I: IntoIterator<Item = u8>,
{
    let mut new = new.into_iter();
    let try_block = || {
        match in_state {
            InState::Header(hdr_buf) => {
                use unmarshal::unmarshal_header;
                if !extend_max(hdr_buf, &mut new, HEADER_LEN) {
                    return Err(ErrorKind::WouldBlock.into());
                }

                let (_, hdr) = unmarshal_header(&hdr_buf[..], 0)
                    .map_err(|_| std::io::Error::new(ErrorKind::Other, "Bad header!"))?;
                hdr_buf.clear();
                *in_state = InState::DynHdr(hdr, mem::take(hdr_buf));
                try_get_msg(stream, in_state, in_fds, with_fd, new)
            }
            InState::DynHdr(hdr, dyn_buf) => {
                use unmarshal::unmarshal_dynamic_header;
                if !extend_max(dyn_buf, &mut new, 4) {
                    return Err(ErrorKind::WouldBlock.into());
                }

                // copy bytes for header
                let array_len = parse_u32(&dyn_buf[..4], hdr.byteorder).unwrap() as usize;
                let dyn_hdr_len = align_num(4 + array_len, 8);
                if !extend_max(dyn_buf, &mut new, dyn_hdr_len) {
                    return Err(ErrorKind::WouldBlock.into());
                }
                let (used, dynhdr) = unmarshal_dynamic_header(&hdr, &dyn_buf[..], HEADER_LEN)
                    .map_err(|_| std::io::Error::new(ErrorKind::Other, "Bad header!"))?;

                // DBus Spec says body is aligned to 8 bytes.
                align_offset(8, &dyn_buf[..], used)
                    .map_err(|_| std::io::Error::new(ErrorKind::Other, "Data in offset!"))?;

                // Validate dynhdr
                if dynhdr.num_fds.unwrap_or(0) > 0 && with_fd {
                    return Err(std::io::Error::new(ErrorKind::Other, "Bad header!"));
                }
                dyn_buf.clear();
                *in_state = InState::Finishing(*hdr, dynhdr, mem::take(dyn_buf));
                try_get_msg(stream, in_state, in_fds, with_fd, new)
            }
            InState::Finishing(hdr, dynhdr, body_buf) => {
                use unmarshal::unmarshal_next_message;
                if !extend_max(body_buf, &mut new, hdr.body_len as usize) {
                    return Err(ErrorKind::WouldBlock.into());
                }

                let (used, msg) = unmarshal_next_message(hdr, dynhdr.clone(), body_buf, 0)
                    .map_err(|_| std::io::Error::new(ErrorKind::Other, "Invalid message body!"))?;
                body_buf.clear();
                *in_state = InState::Header(mem::take(body_buf));
                Ok(Some(msg))
            }
        }
    };
    let ret = match try_block() {
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(e) => {
            in_fds.clear();
            *in_state = mem::take(in_state).to_hdr();
            // Parsing errors mean that we need to close the stream
            stream.shutdown(Shutdown::Both).ok();
            Err(e)
        }
        els => els,
    };
    // self.incoming.extend(new);
    ret
}

pub fn get_next_message(
    stream: &StdUnixStream,
    in_fds: &mut Vec<UnixFd>,
    in_state: &mut InState,
    remaining: &mut VecDeque<u8>,
    with_fd: bool,
) -> std::io::Result<MarshalledMessage> {
    let in_iter = lazy_drain(remaining);
    let res = try_get_msg(stream, in_state, in_fds, with_fd, in_iter);
    if let Some(msg) = res? {
        return Ok(msg);
    }
    debug_assert_eq!(remaining.len(), 0);
    let mut buf = [0; 4096];
    loop {
        let mut anc_data = [0; 256];
        let mut anc = SocketAncillary::new(&mut anc_data);
        let buf = if with_fd {
            let needed = in_state.bytes_needed_for_next();
            let buf = &mut buf[..needed.min(4096)];
            let bufs = &mut [IoSliceMut::new(buf)];
            let r = stream.recv_vectored_with_ancillary(bufs, &mut anc)?;
            let anc_fds_iter = anc
                .messages()
                .filter_map(|res| match res.expect("Anc Data should be valid.") {
                    AncillaryData::ScmRights(rights) => Some(rights.map(|fd| UnixFd::new(fd))),
                    _ => None,
                })
                .flatten();
            in_fds.extend(anc_fds_iter);
            if in_fds.len() > DBUS_MAX_FD_MESSAGE {
                // We received too many fds
                *in_state = mem::take(in_state).to_hdr();
                in_fds.clear();
                //TODO: Find better error
                return Err(std::io::Error::new(
                    ErrorKind::Other,
                    "Too many unix fds received!",
                ));
            }
            &buf[..r]
        } else {
            // TODO: test using IoSliceMut and read_vector
            let bufs = &mut [IoSliceMut::new(&mut buf[..])];
            let r = stream.recv_vectored_with_ancillary(bufs, &mut anc)?;
            &buf[..r]
        };
        let mut in_iter = buf.iter().copied();
        let res = try_get_msg(stream, in_state, in_fds, with_fd, in_iter.by_ref());
        remaining.extend(in_iter); // store the remaining bytes
        if let Some(mut msg) = res? {
            if in_fds.len() != msg.dynheader.num_fds.unwrap_or(0) as usize {
                in_fds.clear();
                return Err(std::io::Error::new(
                    ErrorKind::Other,
                    "Unepexted number of fds received!",
                ));
            }
            msg.body.set_fds(mem::take(in_fds));
            return Ok(msg);
        }
    }
}

pub(crate) struct Receiver {
    pub(super) stream: Arc<StdUnixStream>,
    pub(super) in_fds: Vec<UnixFd>,
    pub(super) in_state: InState,
    pub(super) remaining: VecDeque<u8>,
    pub(super) with_fd: bool,
}

impl Receiver {
    // never blocks
    pub fn get_next_message(&mut self) -> std::io::Result<MarshalledMessage> {
        get_next_message(
            &self.stream,
            &mut self.in_fds,
            &mut self.in_state,
            &mut self.remaining,
            self.with_fd,
        )
    }
}

impl AsRawFd for Receiver {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}
