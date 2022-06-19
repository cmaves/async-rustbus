use std::convert::TryFrom;
use std::io::{self, IoSlice, IoSliceMut};
use std::marker::PhantomData;
use std::mem::{size_of, zeroed};
use std::os::unix::io::RawFd;

#[cfg(target_os = "android")]
use std::ptr::eq;
use std::ptr::read_unaligned;
use std::slice::from_raw_parts;

pub(super) fn recv_vectored_with_ancillary(
    socket: RawFd,
    bufs: &mut [IoSliceMut<'_>],
    ancillary: &mut SocketAncillary<'_>,
) -> std::io::Result<usize> {
    unsafe {
        let mut msg: libc::msghdr = zeroed();
        msg.msg_iov = bufs.as_mut_ptr().cast();
        msg.msg_control = ancillary.buffer.as_mut_ptr().cast();
        #[cfg(any(target_os = "android", all(target_os = "linux", target_env = "gnu")))]
        {
            msg.msg_iovlen = bufs.len() as libc::size_t;
            msg.msg_controllen = ancillary.buffer.len() as libc::size_t;
        }
        #[cfg(any(
            target_os = "dragonfly",
            target_os = "emscripten",
            target_os = "freebsd",
            all(target_os = "linux", target_env = "musl",),
            target_os = "netbsd",
            target_os = "openbsd",
        ))]
        {
            msg.msg_iovlen = bufs.len() as libc::c_int;
            msg.msg_controllen = ancillary.buffer.len() as libc::socklen_t;
        }
        let msg_ptr = &mut msg as *mut libc::msghdr;
        let recvd = libc::recvmsg(socket, msg_ptr, libc::MSG_CMSG_CLOEXEC);
        if recvd == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            ancillary.length = msg.msg_controllen as usize;
            ancillary.truncated = msg.msg_flags & libc::MSG_CTRUNC == libc::MSG_CTRUNC;
            Ok(recvd as usize)
        }
    }
}

pub(super) fn send_vectored_with_ancillary(
    socket: RawFd,
    bufs: &[IoSlice<'_>],
    ancillary: &mut SocketAncillary<'_>,
) -> io::Result<usize> {
    unsafe {
        let mut msg: libc::msghdr = zeroed();
        // SAFETY:  This mut cast is safe because we know it will not be modified.
        msg.msg_iov = bufs.as_ptr() as *mut _;
        msg.msg_control = ancillary.buffer.as_mut_ptr().cast();

        #[cfg(any(target_os = "android", all(target_os = "linux", target_env = "gnu")))]
        {
            msg.msg_iovlen = bufs.len() as libc::size_t;
            msg.msg_controllen = ancillary.length as libc::size_t;
        }
        #[cfg(any(
            target_os = "dragonfly",
            target_os = "emscripten",
            target_os = "freebsd",
            all(target_os = "linux", target_env = "musl",),
            target_os = "netbsd",
            target_os = "openbsd",
        ))]
        {
            msg.msg_iovlen = bufs.len() as libc::c_int;
            msg.msg_controllen = ancillary.length as libc::socklen_t;
        }

        ancillary.truncated = false;
        let msg_ptr = &msg as *const libc::msghdr;
        let sent = libc::sendmsg(socket, msg_ptr, 0);
        if sent == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(sent as usize)
        }
    }
}

fn add_to_ancillary_data<T>(
    buffer: &mut [u8],
    length: &mut usize,
    source: &[T],
    cmsg_level: libc::c_int,
    cmsg_type: libc::c_int,
) -> bool {
    let source_len = if let Some(source_len) = source.len().checked_mul(size_of::<T>()) {
        if let Ok(source_len) = u32::try_from(source_len) {
            source_len
        } else {
            return false;
        }
    } else {
        return false;
    };

    unsafe {
        let additional_space = libc::CMSG_SPACE(source_len) as usize;

        let new_length = if let Some(new_length) = additional_space.checked_add(*length) {
            new_length
        } else {
            return false;
        };

        if new_length > buffer.len() {
            return false;
        }

        buffer[*length..new_length].fill(0);

        *length = new_length;

        let mut msg: libc::msghdr = zeroed();
        msg.msg_control = buffer.as_mut_ptr().cast();

        #[cfg(any(target_os = "android", all(target_os = "linux", target_env = "gnu")))]
        {
            msg.msg_controllen = *length as libc::size_t;
        }
        #[cfg(any(
            target_os = "dragonfly",
            target_os = "emscripten",
            target_os = "freebsd",
            all(target_os = "linux", target_env = "musl",),
            target_os = "netbsd",
            target_os = "openbsd",
        ))]
        {
            msg.msg_controllen = *length as libc::socklen_t;
        }

        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        let mut previous_cmsg = cmsg;
        while !cmsg.is_null() {
            previous_cmsg = cmsg;
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            // Android return the same pointer if it is the last cmsg.
            // Therefore, check it if the previous pointer is the same as the current one.
            #[cfg(target_os = "android")]
            {
                if cmsg == previous_cmsg {
                    break;
                }
            }
        }

        if previous_cmsg.is_null() {
            return false;
        }

        (*previous_cmsg).cmsg_level = cmsg_level;
        (*previous_cmsg).cmsg_type = cmsg_type;
        #[cfg(any(target_os = "android", all(target_os = "linux", target_env = "gnu")))]
        {
            (*previous_cmsg).cmsg_len = libc::CMSG_LEN(source_len) as libc::size_t;
        }
        #[cfg(any(
            target_os = "dragonfly",
            target_os = "emscripten",
            target_os = "freebsd",
            all(target_os = "linux", target_env = "musl",),
            target_os = "netbsd",
            target_os = "openbsd",
        ))]
        {
            (*previous_cmsg).cmsg_len = libc::CMSG_LEN(source_len) as libc::socklen_t;
        }

        let data = libc::CMSG_DATA(previous_cmsg).cast();

        libc::memcpy(data, source.as_ptr().cast(), source_len as usize);
    }
    true
}

struct AncillaryDataIter<'a, T> {
    data: &'a [u8],
    phantom: PhantomData<T>,
}

impl<'a, T> AncillaryDataIter<'a, T> {
    /// Create `AncillaryDataIter` struct to iterate through the data unit in the control message.
    ///
    /// # Safety
    ///
    /// `data` must contain a valid control message.
    unsafe fn new(data: &'a [u8]) -> AncillaryDataIter<'a, T> {
        AncillaryDataIter {
            data,
            phantom: PhantomData,
        }
    }
}

impl<'a, T> Iterator for AncillaryDataIter<'a, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if size_of::<T>() <= self.data.len() {
            unsafe {
                let unit = read_unaligned(self.data.as_ptr().cast());
                self.data = &self.data[size_of::<T>()..];
                Some(unit)
            }
        } else {
            None
        }
    }
}

/// This control message contains file descriptors.
///
/// The level is equal to `SOL_SOCKET` and the type is equal to `SCM_RIGHTS`.
pub struct ScmRights<'a>(AncillaryDataIter<'a, RawFd>);

impl<'a> Iterator for ScmRights<'a> {
    type Item = RawFd;

    fn next(&mut self) -> Option<RawFd> {
        self.0.next()
    }
}

/// The error type which is returned from parsing the type a control message.
#[non_exhaustive]
#[derive(Debug)]
pub enum AncillaryError {
    Unknown { cmsg_level: i32, cmsg_type: i32 },
}

/// This enum represent one control message of variable type.
pub enum AncillaryData<'a> {
    ScmRights(ScmRights<'a>),
}

impl<'a> AncillaryData<'a> {
    /// Create a `AncillaryData::ScmRights` variant.
    ///
    /// # Safety
    ///
    /// `data` must contain a valid control message and the control message must be type of
    /// `SOL_SOCKET` and level of `SCM_RIGHTS`.
    unsafe fn from_data(data: &'a [u8]) -> Self {
        let ancillary_data_iter = AncillaryDataIter::new(data);
        let scm_rights = ScmRights(ancillary_data_iter);
        AncillaryData::ScmRights(scm_rights)
    }

    fn try_from_cmsghdr(cmsg: &'a libc::cmsghdr) -> Result<Self, AncillaryError> {
        unsafe {
            #[cfg(any(
                target_os = "android",
                all(target_os = "linux", target_env = "gnu"),
                all(target_os = "linux", target_env = "uclibc"),
            ))]
            let cmsg_len_zero = libc::CMSG_LEN(0) as libc::size_t;
            #[cfg(any(
                target_os = "dragonfly",
                target_os = "emscripten",
                target_os = "freebsd",
                all(target_os = "linux", target_env = "musl",),
                target_os = "netbsd",
                target_os = "openbsd",
            ))]
            let cmsg_len_zero = libc::CMSG_LEN(0) as libc::socklen_t;

            let data_len = (*cmsg).cmsg_len - cmsg_len_zero;
            let data = libc::CMSG_DATA(cmsg).cast();
            let data = from_raw_parts(data, data_len as usize);

            match (*cmsg).cmsg_level {
                libc::SOL_SOCKET => match (*cmsg).cmsg_type {
                    libc::SCM_RIGHTS => Ok(AncillaryData::from_data(data)),
                    cmsg_type => Err(AncillaryError::Unknown {
                        cmsg_level: libc::SOL_SOCKET,
                        cmsg_type,
                    }),
                },
                cmsg_level => Err(AncillaryError::Unknown {
                    cmsg_level,
                    cmsg_type: (*cmsg).cmsg_type,
                }),
            }
        }
    }
}

/// This struct is used to iterate through the control messages.
pub struct Messages<'a> {
    buffer: &'a [u8],
    current: Option<&'a libc::cmsghdr>,
}

impl<'a> Iterator for Messages<'a> {
    type Item = Result<AncillaryData<'a>, AncillaryError>;

    fn next(&mut self) -> Option<Self::Item> {
        unsafe {
            let mut msg: libc::msghdr = zeroed();
            msg.msg_control = self.buffer.as_ptr() as *mut _;

            #[cfg(any(target_os = "android", all(target_os = "linux", target_env = "gnu")))]
            {
                msg.msg_controllen = self.buffer.len() as libc::size_t;
            }
            #[cfg(any(
                target_os = "dragonfly",
                target_os = "emscripten",
                target_os = "freebsd",
                all(target_os = "linux", target_env = "musl",),
                target_os = "netbsd",
                target_os = "openbsd",
            ))]
            {
                msg.msg_controllen = self.buffer.len() as libc::socklen_t;
            }

            let cmsg = if let Some(current) = self.current {
                libc::CMSG_NXTHDR(&msg, current)
            } else {
                libc::CMSG_FIRSTHDR(&msg)
            };

            let cmsg = cmsg.as_ref()?;
            // Android return the same pointer if it is the last cmsg.
            // Therefore, check it if the previous pointer is the same as the current one.
            #[cfg(target_os = "android")]
            {
                if let Some(current) = self.current {
                    if eq(current, cmsg) {
                        return None;
                    }
                }
            }

            self.current = Some(cmsg);
            let ancillary_result = AncillaryData::try_from_cmsghdr(cmsg);
            Some(ancillary_result)
        }
    }
}

/// A Unix socket Ancillary data struct.
#[derive(Debug)]
pub struct SocketAncillary<'a> {
    buffer: &'a mut [u8],
    length: usize,
    truncated: bool,
}

impl<'a> SocketAncillary<'a> {
    /// Create an ancillary data with the given buffer.
    pub fn new(buffer: &'a mut [u8]) -> Self {
        SocketAncillary {
            buffer,
            length: 0,
            truncated: false,
        }
    }

    /// Returns the capacity of the buffer.
    #[allow(dead_code)]
    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// Returns the number of used bytes.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.length
    }

    /// Returns the iterator of the control messages.
    pub fn messages(&self) -> Messages<'_> {
        Messages {
            buffer: &self.buffer[..self.length],
            current: None,
        }
    }

    /// Is `true` if during a recv operation the ancillary was truncated.
    #[allow(dead_code)]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Add file descriptors to the ancillary data.
    ///
    /// The function returns `true` if there was enough space in the buffer.
    /// If there was not enough space then no file descriptors was appended.
    /// Technically, that means this operation adds a control message with the level `SOL_SOCKET`
    /// and type `SCM_RIGHTS`.
    pub fn add_fds(&mut self, fds: &[RawFd]) -> bool {
        self.truncated = false;
        add_to_ancillary_data(
            self.buffer,
            &mut self.length,
            fds,
            libc::SOL_SOCKET,
            libc::SCM_RIGHTS,
        )
    }

    /// Clears the ancillary data, removing all values.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.length = 0;
        self.truncated = false;
    }
}
