use std::io::{ErrorKind, IoSliceMut};
use std::mem;
use std::num::NonZeroU32;

use async_std::os::unix::io::RawFd;

use crate::rustbus_core;
use rustbus_core::message_builder::MarshalledMessage;
use rustbus_core::wire::marshal;
use rustbus_core::wire::unixfd::UnixFd;

use super::{GenStream, SocketAncillary, DBUS_MAX_FD_MESSAGE};

pub(super) enum OutState {
    Waiting(Vec<u8>),
    WritingAnc(Vec<u8>, Vec<UnixFd>),
    WritingData(Vec<u8>, usize),
}
impl Default for OutState {
    fn default() -> Self {
        OutState::Waiting(Vec::new())
    }
}
pub(crate) struct SendState {
    pub(super) out_state: OutState,
    // pub(super) serial: u32,
    pub(super) with_fd: bool,
}
impl SendState {
    pub(crate) fn finish_sending_next(&mut self, stream: &GenStream) -> std::io::Result<()> {
        loop {
            let mut anc_data = [0; 256];
            let mut anc = SocketAncillary::new(&mut anc_data[..]);
            match &mut self.out_state {
                OutState::Waiting(_) => return Ok(()),
                OutState::WritingAnc(out_buf, out_fds) => {
                    let bufs = &mut [IoSliceMut::new(&mut out_buf[..])];
                    let fds: Vec<RawFd> = out_fds
                        .into_iter()
                        .map(|f| f.get_raw_fd().unwrap())
                        .collect();
                    if !anc.add_fds(&fds[..]) {
                        panic!("Wasn't enough room for ancillary data!");
                    }
                    let sent = stream.send_vectored_with_ancillary(bufs, &mut anc)?;

                    if sent > 0 {
                        // we sent the anc data so move to next state
                        if sent == out_buf.len() {
                            self.out_state = OutState::Waiting(mem::take(out_buf));
                        } else {
                            self.out_state = OutState::WritingData(mem::take(out_buf), sent);
                        }
                    }
                }
                OutState::WritingData(out_buf, sent) => {
                    let bufs = &mut [IoSliceMut::new(&mut out_buf[*sent..])];
                    *sent += stream.send_vectored_with_ancillary(bufs, &mut anc)?;
                    if *sent == out_buf.len() {
                        self.out_state = OutState::Waiting(mem::take(out_buf));
                    }
                }
            }
        }
    }

    pub(crate) fn write_next_message(
        &mut self,
        stream: &GenStream,
        msg: &MarshalledMessage,
        serial: NonZeroU32,
    ) -> std::io::Result<bool> {
        self.finish_sending_next(stream)?;

        if let OutState::Waiting(out_buf) = &mut self.out_state {
            out_buf.clear();
            //TODO: improve
            marshal::marshal(&msg, serial.into(), out_buf).map_err(|_e| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "Marshal Failure.")
            })?;
            out_buf.extend(msg.get_buf());
            let mut fds = Vec::new();
            for fd in msg.body.get_fds() {
                let fd = fd.dup().map_err(|_| {
                    std::io::Error::new(ErrorKind::InvalidData, "Fds already consumed!")
                })?;
                fds.push(fd);
            }

            if (!self.with_fd && fds.len() > 0) || fds.len() > DBUS_MAX_FD_MESSAGE {
                // TODO: Add better error code
                return Err(std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "Too many Fds.",
                ));
            }
            self.out_state = OutState::WritingAnc(mem::take(out_buf), fds);
            match self.finish_sending_next(stream) {
                Ok(_) => return Ok(true),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
                Err(e) => Err(e),
            }
        } else {
            unreachable!()
        }
    }
}
