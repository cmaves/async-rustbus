use std::collections::VecDeque;
use std::io::{ErrorKind, IoSliceMut};
use std::mem;
use std::net::Shutdown;
use std::sync::atomic::Ordering;

use crate::rustbus_core;
use rustbus_core::message_builder::{DynamicHeader, MarshalledMessage};
use rustbus_core::wire::unixfd::UnixFd;
use rustbus_core::wire::unmarshal;
use rustbus_core::wire::util::align_offset;
use unmarshal::HEADER_LEN;

use crate::utils::{align_num, extend_max, lazy_drain, parse_u32};
use crate::READ_COUNT;

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
            InState::Header(b) | InState::DynHdr(_, b) | InState::Finishing(_, _, b) => b,
        };
        ret.clear();
        ret
    }
    fn to_hdr(self) -> Self {
        let buf = self.to_buf();
        InState::Header(buf)
    }
    fn get_mut_buf(&mut self) -> &mut Vec<u8> {
        match self {
            InState::Header(b) | InState::DynHdr(_, b) | InState::Finishing(_, _, b) => b,
        }
    }
    fn bytes_needed_for_next(&self) -> usize {
        match self {
            InState::Header(b) => HEADER_LEN + 4 - b.len(),
            InState::DynHdr(hdr, b) => {
                if b.len() < 16 {
                    16 - b.len()
                } else {
                    let array_len = parse_u32(&b[12..16], hdr.byteorder) as usize;
                    align_num(HEADER_LEN + 4 + array_len, 8) - b.len()
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
                        return Ok(None);
                    }

                    let (_, hdr) = unmarshal_header(&hdr_buf[..], 0)
                        .map_err(|_e| std::io::Error::new(ErrorKind::Other, "Bad header!"))?;
                    self.in_state = InState::DynHdr(hdr, mem::take(hdr_buf));
                    self.try_get_msg(stream, new)
                }
                InState::DynHdr(hdr, dyn_buf) => {
                    use unmarshal::unmarshal_dynamic_header;
                    if !extend_max(dyn_buf, &mut new, HEADER_LEN + 4) {
                        return Ok(None);
                    }

                    // copy bytes for header
                    let array_len =
                        parse_u32(&dyn_buf[HEADER_LEN..HEADER_LEN + 4], hdr.byteorder) as usize;
                    let total_hdr_len = align_num(HEADER_LEN + 4 + array_len, 8);
                    if !extend_max(dyn_buf, &mut new, total_hdr_len) {
                        return Ok(None);
                    }
                    let (used, dynhdr) = unmarshal_dynamic_header(&hdr, &dyn_buf[..], HEADER_LEN)
                        .map_err(|e| {
                        std::io::Error::new(ErrorKind::Other, format!("Bad header!: {:?}", e))
                    })?;

                    // DBus Spec says body is aligned to 8 bytes.
                    align_offset(8, &dyn_buf[..], HEADER_LEN + used)
                        .map_err(|_| std::io::Error::new(ErrorKind::Other, "Data in offset!"))?;

                    // Validate dynhdr
                    if dynhdr.num_fds.unwrap_or(0) > 0 && !self.with_fd {
                        return Err(std::io::Error::new(ErrorKind::Other, "Bad header!"));
                    }
                    dyn_buf.clear();
                    self.in_state = InState::Finishing(*hdr, dynhdr, mem::take(dyn_buf));
                    self.try_get_msg(stream, new)
                }
                InState::Finishing(hdr, dynhdr, body_buf) => {
                    use unmarshal::unmarshal_next_message;
                    if !extend_max(body_buf, &mut new, hdr.body_len as usize) {
                        return Ok(None);
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
        let mut anc_buf = [0; 256];
        loop {
            let needed = self.in_state.bytes_needed_for_next();
            let mut anc = if self.with_fd {
                SocketAncillary::new(&mut anc_buf)
            } else {
                SocketAncillary::new(&mut anc_buf[..0])
            };
            let mut buf = [0; 4 * 1024];
            let buf = if self.with_fd || needed > 4096 {
                // Read the stream directly into the in_state buffer

                debug_assert!(needed > 0);
                let vec = self.in_state.get_mut_buf();
                unsafe {
                    /* SAFETY 1) we reserve the bytes we need as uninitialized bytes in the Vec
                     * 2) We create a mutable slice to the uninitialized bytes.
                     * Because they are not being read this is safe.
                     * 3) We read the Fd into the bytes directly.
                     * 4) Set the new buffer len.
                     */
                    vec.reserve(needed);
                    let uninit_buf = vec.as_mut_ptr().add(vec.len());
                    let uninit_slice = std::slice::from_raw_parts_mut(uninit_buf, needed);
                    let bufs = &mut [IoSliceMut::new(uninit_slice)];
                    let gotten = stream.recv_vectored_with_ancillary(bufs, &mut anc)?;
                    vec.set_len(vec.len() + gotten);
                }
                READ_COUNT.fetch_add(1, Ordering::Relaxed);
                &buf[..0]
            } else {
                let bufs = &mut [IoSliceMut::new(&mut buf[..])];
                let r = stream.recv_vectored_with_ancillary(bufs, &mut anc)?;
                READ_COUNT.fetch_add(1, Ordering::Relaxed);
                &buf[..r]
            };
            if self.with_fd {
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
            }
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
