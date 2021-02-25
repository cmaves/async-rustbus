use async_std::os::unix::io::{AsRawFd, RawFd};
use std::io::{ErrorKind, IoSliceMut};
use std::mem;
use std::sync::Arc;

use crate::rustbus_core;
use rustbus_core::message_builder::MarshalledMessage;
use rustbus_core::wire::unixfd::UnixFd;
use rustbus_core::wire::marshal;

use super::{DBUS_MAX_FD_MESSAGE, GenStream, SocketAncillary};

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

pub(super) fn finish_sending_next(
    stream: &GenStream,
    out_state: &mut OutState,
) -> std::io::Result<()> {
    loop {
        let mut anc_data = [0; 256];
        let mut anc = SocketAncillary::new(&mut anc_data[..]);
        match out_state {
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
                        *out_state = OutState::Waiting(mem::take(out_buf));
                    } else {
                        *out_state = OutState::WritingData(mem::take(out_buf), sent);
                    }
                }
            }
            OutState::WritingData(out_buf, sent) => {
                let bufs = &mut [IoSliceMut::new(&mut out_buf[..])];
                *sent += stream.send_vectored_with_ancillary(bufs, &mut anc)?;
                if *sent == out_buf.len() {
                    *out_state = OutState::Waiting(mem::take(out_buf));
                }
            }
        }
    }
}

pub(super) fn write_next_message(
    stream: &GenStream,
    out_state: &mut OutState,
    serial: &mut u32,
    with_fd: bool,
    msg: &MarshalledMessage,
) -> std::io::Result<(bool, Option<u32>)> {
    finish_sending_next(stream, out_state)?;
    let (serial, allocated) = match msg.dynheader.serial {
        Some(serial) => (serial, None),
        None => {
            *serial += 1;
            (*serial, Some(*serial))
        }
    };

    if let OutState::Waiting(out_buf) = out_state {
        out_buf.clear();
        //TODO: improve
        marshal::marshal(&msg, serial, out_buf)
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Marshal Failure."))?;
        let fds = msg
            .body
            .get_duped_fds()
            .map_err(|_| std::io::Error::new(ErrorKind::InvalidInput, "Fds already consumed"))?;

        if (!with_fd && fds.len() > 0) || fds.len() > DBUS_MAX_FD_MESSAGE {
            // TODO: Add better error code
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "Too many Fds.",
            ));
        }
        *out_state = OutState::WritingAnc(mem::take(out_buf), fds);
        match finish_sending_next(stream, out_state) {
            Ok(_) => return Ok((true, allocated)),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok((false, allocated)),
            Err(e) => Err(e),
        }
    } else {
        unreachable!()
    }
}

pub(crate) struct Sender {
    pub(super) stream: Arc<GenStream>,
    pub(super) out_state: OutState,
    pub(super) serial: u32,
    pub(super) with_fd: bool,
}

impl Sender {
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
impl AsRawFd for Sender {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}
