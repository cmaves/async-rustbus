use std::collections::VecDeque;
use std::io::{ErrorKind, IoSlice};
use std::num::NonZeroU32;
use std::sync::Arc;

use arrayvec::ArrayVec;

use std::os::unix::io::RawFd;

use crate::rustbus_core;
use rustbus_core::message_builder::MarshalledMessage;

use super::{GenStream, SocketAncillary, DBUS_MAX_FD_MESSAGE};

pub(crate) struct SendState {
    pub(super) with_fd: bool,
    pub(super) idx: u64,
    pub(super) queue: VecDeque<RawOut>,
}
pub(super) struct RawOut {
    written: usize,
    header: Vec<u8>,
    body: Arc<Vec<u8>>,
    fds: Vec<RawFd>,
}
impl Drop for RawOut {
    fn drop(&mut self) {
        for fd in self.fds.iter() {
            unsafe {
                libc::close(*fd);
            }
        }
    }
}
fn bufs_left<'a>(written: usize, header: &'a [u8], body: &'a [u8]) -> (&'a [u8], Option<&'a [u8]>) {
    if written < header.len() {
        let right = if body.is_empty() { None } else { Some(body) };
        (&header[written..], right)
    } else {
        let offset = written - header.len();
        (&body[offset..], None)
    }
}

impl RawOut {
    fn to_write(&self) -> usize {
        self.header.len() + self.body.len() - self.written
    }
    fn done(&self) -> bool {
        self.to_write() == 0
    }
    fn out_bufs_left(&self) -> (&[u8], Option<&[u8]>) {
        bufs_left(self.written, &self.header, &self.body)
    }
    fn advance(&mut self, mut adv: usize) -> usize {
        let first_rem = self.header.len().saturating_sub(self.written);
        if first_rem > 0 {
            let to_adv = first_rem.min(adv);
            adv -= to_adv;
            self.written += to_adv;
        }
        if adv == 0 {
            return 0;
        }
        let second_rem = self.to_write();
        if second_rem > 0 {
            let to_adv = second_rem.min(adv);
            adv -= to_adv;
            self.written += to_adv;
        }
        adv
    }
}

/// Populate takes the list of RawOut
/// to be written and fills the IoSlices for a vectored write.
/// FDs sent in a write must be sent in a writee that only contains the correct message.
/// This prevents the FDs from being associated with the wrong message.
fn populate<'a, 'b, const N: usize>(
    queue: &'a VecDeque<RawOut>,
    ios: &'b mut ArrayVec<IoSlice<'a>, N>,
    anc: &mut SocketAncillary,
) {
    for out in queue.iter() {
        if ios.is_full() {
            break;
        }
        if out.written == 0 && !out.fds.is_empty() {
            if ios.is_empty() {
                assert!(anc.add_fds(&out.fds[..]));
            } else {
                break;
            }
        }
        let (buf0, buf1_opt) = out.out_bufs_left();
        ios.push(IoSlice::new(buf0));
        if let (Some(buf1), false) = (buf1_opt, ios.is_full()) {
            ios.push(IoSlice::new(buf1));
        }
    }
}
fn update_written(queue: &mut VecDeque<RawOut>, mut written: usize) -> usize {
    while let Some(out) = queue.front_mut() {
        written = out.advance(written);
        if written == 0 {
            if out.done() {
                queue.pop_front();
            }
            break;
        }
        queue.pop_front();
    }
    written
}

impl SendState {
    pub(crate) fn current_idx(&self) -> u64 {
        self.idx - self.queue.len() as u64
    }
    pub(crate) fn finish_sending_next(&mut self, stream: &GenStream) -> std::io::Result<u64> {
        let mut anc_data = [0; 256];
        while !self.queue.is_empty() {
            let mut anc = SocketAncillary::new(&mut anc_data[..]);
            let mut ios = ArrayVec::<_, 32>::new();
            populate(&self.queue, &mut ios, &mut anc);
            //eprintln!("ios.len(): {}", ios.len());
            match stream.send_vectored_with_ancillary(&ios, &mut anc) {
                Ok(written) => {
                    //eprintln!("written: {}", written);
                    drop(ios);
                    let left = update_written(&mut self.queue, written);
                    debug_assert_eq!(left, 0);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(self.current_idx())
    }

    pub(crate) fn write_next_message(
        &mut self,
        stream: &GenStream,
        msg: &MarshalledMessage,
        serial: NonZeroU32,
    ) -> std::io::Result<Option<u64>> {
        let mut header = Vec::with_capacity(1024);
        msg.marshal_header(serial, &mut header).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "Marshal Failure.")
        })?;
        let fds = msg.body.fds();
        if (!self.with_fd && !fds.is_empty()) || fds.len() > DBUS_MAX_FD_MESSAGE {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "Too many Fds.",
            ));
        }
        let mut out = RawOut {
            header,
            body: msg.body.buf_arc(),
            written: 0,
            fds: Vec::with_capacity(fds.len()),
        };
        for fd in fds {
            let fd = fd.dup().map_err(|_| {
                std::io::Error::new(ErrorKind::InvalidData, "Fds already consumed!")
            })?;
            out.fds.push(fd.take_raw_fd().unwrap());
        }
        let mut anc_data = [0; 256];
        let needed = out.header.len() + msg.get_buf().len();
        loop {
            let mut anc = SocketAncillary::new(&mut anc_data);
            let mut ios = ArrayVec::<_, 34>::new();
            populate(&self.queue, &mut ios, &mut anc);
            if !ios.is_full() && (out.fds.is_empty() || ios.is_empty()) {
                let (buf0, buf1_opt) = bufs_left(out.written, &out.header, msg.get_buf());
                if out.written == 0 && !out.fds.is_empty() {
                    assert!(anc.add_fds(&out.fds[..]));
                }
                ios.push(IoSlice::new(buf0));
                if let (Some(buf1), false) = (buf1_opt, ios.is_full()) {
                    ios.push(IoSlice::new(buf1));
                }
            }
            match stream.send_vectored_with_ancillary(&ios, &mut anc) {
                Ok(written) => {
                    //eprintln!("written: {}", written);
                    drop(ios);
                    let written = update_written(&mut self.queue, written);
                    if written == 0 {
                        continue;
                    }
                    out.written += written;
                    if out.written == needed {
                        return Ok(None);
                    }
                    debug_assert!(out.written < needed);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }

        self.queue.push_back(out);
        let ret = self.idx;
        self.idx += 1;
        Ok(Some(ret))
    }
}
