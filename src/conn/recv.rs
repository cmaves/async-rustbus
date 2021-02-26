use std::collections::VecDeque;
use std::io::{ErrorKind, IoSliceMut};
use std::mem;
use std::net::Shutdown;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::Arc;

use async_std::sync::Mutex;

use crate::rustbus_core;
use rustbus_core::message_builder::{DynamicHeader, MarshalledMessage};
use rustbus_core::wire::unixfd::UnixFd;
use rustbus_core::wire::unmarshal;
use rustbus_core::wire::util::align_offset;
use unmarshal::HEADER_LEN;

use crate::utils::{align_num, extend_max, lazy_drain, parse_u32};

use super::{AncillaryData, GenStream, SocketAncillary, DBUS_MAX_FD_MESSAGE};

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
                    let array_len = parse_u32(&b[..4], hdr.byteorder) as usize;
                    4 + array_len - b.len()
                }
            }
            InState::Finishing(hdr, _, b) => hdr.body_len as usize - b.len(),
        }
    }
}

pub(crate) struct RecvState {
    pub(super) in_state: InState,
    pub(super) in_fds: Vec<UnixFd>,
    pub(super) remaining: VecDeque<u8>,
    pub(super) with_fd: bool,
}
impl RecvState {
    fn try_get_msg<I>(
        &mut self,
        stream: &GenStream,
        new: I,
    ) -> std::io::Result<Option<MarshalledMessage>>
    where
        I: IntoIterator<Item = u8>,
    {
        let mut new = new.into_iter();
        let try_block = || {
            match &mut self.in_state {
                InState::Header(hdr_buf) => {
                    use unmarshal::unmarshal_header;
                    if !extend_max(hdr_buf, &mut new, HEADER_LEN) {
                        return Err(ErrorKind::WouldBlock.into());
                    }

                    let (_, hdr) = unmarshal_header(&hdr_buf[..], 0)
                        .map_err(|_| std::io::Error::new(ErrorKind::Other, "Bad header!"))?;
                    hdr_buf.clear();
                    self.in_state = InState::DynHdr(hdr, mem::take(hdr_buf));
                    self.try_get_msg(stream, new)
                }
                InState::DynHdr(hdr, dyn_buf) => {
                    use unmarshal::unmarshal_dynamic_header;
                    if !extend_max(dyn_buf, &mut new, 4) {
                        return Err(ErrorKind::WouldBlock.into());
                    }

                    // copy bytes for header
                    let array_len = parse_u32(&dyn_buf[..4], hdr.byteorder) as usize;
                    let dyn_hdr_len = align_num(4 + array_len, 8);
                    if !extend_max(dyn_buf, &mut new, dyn_hdr_len) {
                        return Err(ErrorKind::WouldBlock.into());
                    }
                    let (used, dynhdr) =
                        unmarshal_dynamic_header(&hdr, &dyn_buf[..], HEADER_LEN)
                            .map_err(|_| std::io::Error::new(ErrorKind::Other, "Bad header!"))?;

                    // DBus Spec says body is aligned to 8 bytes.
                    align_offset(8, &dyn_buf[..], used)
                        .map_err(|_| std::io::Error::new(ErrorKind::Other, "Data in offset!"))?;

                    // Validate dynhdr
                    if dynhdr.num_fds.unwrap_or(0) > 0 && self.with_fd {
                        return Err(std::io::Error::new(ErrorKind::Other, "Bad header!"));
                    }
                    dyn_buf.clear();
                    self.in_state = InState::Finishing(*hdr, dynhdr, mem::take(dyn_buf));
                    self.try_get_msg(stream, new)
                }
                InState::Finishing(hdr, dynhdr, body_buf) => {
                    use unmarshal::unmarshal_next_message;
                    if !extend_max(body_buf, &mut new, hdr.body_len as usize) {
                        return Err(ErrorKind::WouldBlock.into());
                    }

                    let (used, msg) = unmarshal_next_message(hdr, dynhdr.clone(), body_buf, 0)
                        .map_err(|_| {
                            std::io::Error::new(ErrorKind::Other, "Invalid message body!")
                        })?;
                    debug_assert_eq!(used, hdr.body_len as usize);
                    body_buf.clear();
                    self.in_state = InState::Header(mem::take(body_buf));
                    Ok(Some(msg))
                }
            }
        };
        let ret = match try_block() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => {
                self.in_fds.clear();
                self.in_state = mem::take(&mut self.in_state).to_hdr();
                // Parsing errors mean that we need to close the stream
                stream.shutdown(Shutdown::Both).ok();
                Err(e)
            }
            els => els,
        };
        // self.incoming.extend(new);
        ret
    }

    pub(crate) fn get_next_message(
        &mut self,
        stream: &GenStream,
    ) -> std::io::Result<MarshalledMessage> {
        let mut remaining = mem::take(&mut self.remaining);
        let in_iter = lazy_drain(&mut remaining);
        let res = self.try_get_msg(stream, in_iter);
        self.remaining = remaining;
        if let Some(msg) = res? {
            return Ok(msg);
        }
        debug_assert_eq!(self.remaining.len(), 0);
        let mut buf = [0; 4096];
        loop {
            let buf = if self.with_fd {
                let needed = self.in_state.bytes_needed_for_next();
                let buf = &mut buf[..needed.min(4096)];
                let bufs = &mut [IoSliceMut::new(buf)];
                let mut anc_data = [0; 256];
                let mut anc = SocketAncillary::new(&mut anc_data);
                let r = stream.recv_vectored_with_ancillary(bufs, &mut anc)?;
                let anc_fds_iter = anc
                    .messages()
                    .filter_map(|res| match res.expect("Anc Data should be valid.") {
                        AncillaryData::ScmRights(rights) => Some(rights.map(|fd| UnixFd::new(fd))),
                    })
                    .flatten();
                self.in_fds.extend(anc_fds_iter);
                if self.in_fds.len() > DBUS_MAX_FD_MESSAGE {
                    // We received too many fds
                    self.in_state = mem::take(&mut self.in_state).to_hdr();
                    self.in_fds.clear();
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
                let r = stream
                    .recv_vectored_with_ancillary(bufs, &mut SocketAncillary::new(&mut []))?;
                &buf[..r]
            };
            let mut in_iter = buf.iter().copied();
            let res = self.try_get_msg(stream, in_iter.by_ref());
            self.remaining.extend(in_iter); // store the remaining bytes
            if let Some(mut msg) = res? {
                if self.in_fds.len() != msg.dynheader.num_fds.unwrap_or(0) as usize {
                    self.in_fds.clear();
                    return Err(std::io::Error::new(
                        ErrorKind::Other,
                        "Unepexted number of fds received!",
                    ));
                }
                msg.body.set_fds(mem::take(&mut self.in_fds));
                return Ok(msg);
            }
        }
    }
}

pub(crate) struct Receiver {
    pub(crate) stream: Arc<GenStream>,
    pub(crate) state: Mutex<RecvState>,
}

impl Receiver {
    // never blocks
    /*
    pub fn get_next_message(self, lock) -> std::io::Result<MarshalledMessage> {
        get_next_message(
            &self.stream,
            self.with_fd,
        )
    }*/
}

impl AsRawFd for Receiver {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}
