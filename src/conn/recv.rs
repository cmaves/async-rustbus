use std::io::{ErrorKind, IoSliceMut};
use std::mem;
use std::net::Shutdown;
use std::num::NonZeroU32;

use crate::rustbus_core;
use rustbus_core::message_builder::{DynamicHeader, MarshalledMessage, MarshalledMessageBody};
use rustbus_core::wire::marshal::traits::SignatureBuffer;
use rustbus_core::wire::util::align_offset;
use rustbus_core::wire::{unmarshal, UnixFd};
use unmarshal::traits::Unmarshal;
use unmarshal::HEADER_LEN;

use crate::utils::{align_num, parse_u32};

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
    fn into_buf(self) -> Vec<u8> {
        let mut ret = match self {
            InState::Header(b) | InState::DynHdr(_, b) | InState::Finishing(_, _, b) => b,
        };
        ret.clear();
        ret
    }
    fn into_hdr(self) -> Self {
        let buf = self.into_buf();
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
    pub(super) remaining: Vec<u8>,
    pub(super) rem_loc: usize,
    pub(super) with_fd: bool,
}

fn extend_max(vec: &mut Vec<u8>, buf: &[u8], loc: &mut usize, target: usize) -> bool {
    if vec.len() >= target {
        return true;
    }
    let buf = &buf[*loc..];
    let needed = target - vec.len();
    if needed > buf.len() {
        vec.extend_from_slice(buf);
        *loc += buf.len();
        false
    } else {
        vec.extend_from_slice(&buf[..needed]);
        *loc += needed;
        true
    }
}
impl RecvState {
    fn try_get_msg(
        &mut self,
        stream: &GenStream,
    ) -> std::io::Result<Option<(unmarshal::Header, DynamicHeader, Vec<u8>)>> {
        let mut try_block = || {
            match &mut self.in_state {
                InState::Header(hdr_buf) => {
                    use unmarshal::unmarshal_header;
                    if !extend_max(hdr_buf, &self.remaining, &mut self.rem_loc, HEADER_LEN) {
                        return Ok(None);
                    }

                    let (_, hdr) = unmarshal_header(&hdr_buf[..], 0).map_err(|_e| {
                        eprintln!("{:?} ({:?}", _e, hdr_buf);
                        std::io::Error::new(ErrorKind::Other, "Bad header!")
                    })?;
                    self.in_state = InState::DynHdr(hdr, mem::take(hdr_buf));
                    self.try_get_msg(stream)
                }
                InState::DynHdr(hdr, dyn_buf) => {
                    if !extend_max(dyn_buf, &self.remaining, &mut self.rem_loc, HEADER_LEN + 4) {
                        return Ok(None);
                    }

                    // copy bytes for header
                    let array_len =
                        parse_u32(&dyn_buf[HEADER_LEN..HEADER_LEN + 4], hdr.byteorder) as usize;
                    let total_hdr_len = align_num(HEADER_LEN + 4 + array_len, 8);
                    if !extend_max(dyn_buf, &self.remaining, &mut self.rem_loc, total_hdr_len) {
                        return Ok(None);
                    }
                    let mut ctx = unmarshal::UnmarshalContext {
                        byteorder: hdr.byteorder,
                        offset: HEADER_LEN,
                        buf: &dyn_buf[..],
                        fds: &[],
                    };
                    let (used, mut dynhdr) = DynamicHeader::unmarshal(&mut ctx).map_err(|e| {
                        std::io::Error::new(ErrorKind::Other, format!("Bad header!: {:?}", e))
                    })?;
                    drop(ctx);
                    let serial = NonZeroU32::new(hdr.serial)
                        .ok_or_else(|| std::io::Error::new(ErrorKind::Other, "Serial was zero!"))?;
                    dynhdr.serial = Some(serial);

                    // DBus Spec says body is aligned to 8 bytes.
                    align_offset(8, &dyn_buf[..], HEADER_LEN + used)
                        .map_err(|_| std::io::Error::new(ErrorKind::Other, "Data in offset!"))?;

                    // Validate dynhdr
                    if dynhdr.num_fds.unwrap_or(0) > 0 && !self.with_fd {
                        return Err(std::io::Error::new(ErrorKind::Other, "Bad header!"));
                    }
                    dyn_buf.clear();
                    dyn_buf.reserve(hdr.body_len as usize);
                    self.in_state = InState::Finishing(*hdr, dynhdr, mem::take(dyn_buf));
                    self.try_get_msg(stream)
                }
                InState::Finishing(hdr, dynhdr, body_buf) => {
                    if !extend_max(
                        body_buf,
                        &self.remaining,
                        &mut self.rem_loc,
                        hdr.body_len as usize,
                    ) {
                        return Ok(None);
                    }
                    let hdr = *hdr;
                    let dynhdr = mem::take(dynhdr);
                    let body = mem::take(body_buf);
                    self.in_state = InState::default();
                    Ok(Some((hdr, dynhdr, body)))
                }
            }
        };
        match try_block() {
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(None), // TODO: is this reachable?
            Err(e) => {
                self.in_fds.clear();
                self.in_state = mem::take(&mut self.in_state).into_hdr();
                // Parsing errors mean that we need to close the stream
                stream.shutdown(Shutdown::Both).ok();
                Err(e)
            }
            els => els,
        }
    }

    pub(crate) fn get_next_message(
        &mut self,
        stream: &GenStream,
    ) -> std::io::Result<MarshalledMessage> {
        let res = self.try_get_msg(stream);
        if let Some((hdr, dynhdr, body)) = res? {
            let msg = mm_from_raw(hdr, dynhdr, body, Vec::new());
            match msg.body.validate() {
                Ok(_) => return Ok(msg),
                Err(e) => {
                    stream.shutdown(Shutdown::Both).ok();
                    return Err(std::io::Error::new(
                        ErrorKind::Other,
                        format!("Bad message body!: {:?}", e),
                    ));
                }
            }
        }
        let mut anc_buf = [0; 256];
        loop {
            debug_assert_eq!(self.remaining.len(), self.rem_loc);
            debug_assert!(self.remaining.capacity() >= 4096);
            self.remaining.clear();
            self.rem_loc = 0;
            let needed = self.in_state.bytes_needed_for_next();
            // Read the stream directly into the in_state buffer
            debug_assert!(needed > 0);
            let vec = self.in_state.get_mut_buf();
            // SAFETY: uninit_buf is never read from
            let uninit_buf = unsafe { vec_uninit_slice(vec, Some(needed)) };
            let uninit_len = uninit_buf.len();

            debug_assert!(self.remaining.is_empty());
            let mut rem: Vec<u8> = mem::take(&mut self.remaining);
            let mut bufs = [IoSliceMut::new(uninit_buf), IoSliceMut::new(&mut [])];
            let (bufs, mut anc) = if self.with_fd {
                (&mut bufs[..1], SocketAncillary::new(&mut anc_buf[..]))
            } else {
                // SAFETY: rem_buf is never read from
                let rem_buf = unsafe { vec_uninit_slice(&mut rem, None) };
                bufs[1] = IoSliceMut::new(rem_buf);
                (&mut bufs[..], SocketAncillary::new(&mut []))
            };
            let res = stream.recv_vectored_with_ancillary(bufs, &mut anc);
            let gotten = match &res {
                Ok(0) | Err(_) => {
                    self.remaining = rem;
                    res?; // return if err otherwise return Hungup in case of Ok(0)
                    return Err(std::io::Error::new(
                        ErrorKind::BrokenPipe,
                        "DBus daemon hung up!",
                    ));
                }
                Ok(i) => *i,
            };
            unsafe {
                if gotten > uninit_len {
                    vec.set_len(vec.len() + uninit_len);
                    rem.set_len(gotten - uninit_len);
                } else {
                    vec.set_len(vec.len() + gotten);
                }
            }
            self.remaining = rem;
            if self.with_fd {
                let anc_fds_iter =
                    anc.messages()
                        .flat_map(|res| match res.expect("Anc Data should be valid.") {
                            AncillaryData::ScmRights(rights) => rights.map(UnixFd::new),
                        });
                self.in_fds.extend(anc_fds_iter);
                if self.in_fds.len() > DBUS_MAX_FD_MESSAGE {
                    // We received too many fds
                    self.in_state = mem::take(&mut self.in_state).into_hdr();
                    self.in_fds.clear();
                    //TODO: Find better error
                    return Err(std::io::Error::new(
                        ErrorKind::Other,
                        "Too many unix fds received!",
                    ));
                }
            }
            let res = self.try_get_msg(stream);
            if let Some((hdr, dynhdr, body)) = res? {
                if self.in_fds.len() != dynhdr.num_fds.unwrap_or(0) as usize {
                    self.in_fds.clear();
                    return Err(std::io::Error::new(
                        ErrorKind::Other,
                        "Unepexted number of fds received!",
                    ));
                }
                let msg = mm_from_raw(hdr, dynhdr, body, mem::take(&mut self.in_fds));
                match msg.body.validate() {
                    Ok(_) => return Ok(msg),
                    Err(e) => {
                        stream.shutdown(Shutdown::Both).ok();
                        return Err(std::io::Error::new(
                            ErrorKind::Other,
                            format!("Bad message body!: {:?}", e),
                        ));
                    }
                }
            }
        }
    }
    #[allow(dead_code)]
    pub fn pos_next_msg(&self) -> bool {
        let needed = self.in_state.bytes_needed_for_next();
        self.remaining.len() >= needed
    }
}

/// Get a slice to the uninitialized portion of an `Vec<u8>`
///
/// `wanted` determines how long the slice should be. If it is `None` then the
/// slice will point to the remaining capacity.
// SAFETY: The slice returned by this function must never be read from
unsafe fn vec_uninit_slice(vec: &mut Vec<u8>, wanted: Option<usize>) -> &mut [u8] {
    let target = match wanted {
        Some(wanted) => {
            vec.reserve(wanted);
            wanted
        }
        None => vec.capacity() - vec.len(),
    };
    let rem_buf = vec.as_mut_ptr().add(vec.len());
    std::slice::from_raw_parts_mut(rem_buf, target)
}
fn mm_from_raw(
    hdr: unmarshal::Header,
    dynhdr: DynamicHeader,
    body: Vec<u8>,
    fds: Vec<UnixFd>,
) -> MarshalledMessage {
    let sig = dynhdr.signature.as_deref().unwrap_or("");
    let sig = SignatureBuffer::from_string(sig.to_string());
    MarshalledMessage {
        typ: hdr.typ,
        flags: hdr.flags,
        body: MarshalledMessageBody::from_parts(body, fds, sig, hdr.byteorder),
        dynheader: dynhdr,
    }
}
