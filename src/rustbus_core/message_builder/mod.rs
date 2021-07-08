//! Build new messages that you want to send over a connection

use std::convert::TryInto;
use std::num::NonZeroU32;

use super::wire::marshal::traits::{Marshal, Signature, SignatureBuffer};
use super::wire::marshal::MarshalContext;
use super::wire::unmarshal::{traits::Unmarshal, UnmarshalContext, HEADER_LEN};
use crate::utils::align_num;

pub use super::org_message_builder::{HeaderFlags, MessageType};
use super::path::ObjectPath;
use super::signature::{Base, Container, Type};
use super::ByteOrder;

use rustbus::params::validation::Error as ValidationErr;
use rustbus::params::validation::{
    validate_busname, validate_errorname, validate_interface, validate_membername,
    validate_signature,
};
use rustbus::wire::util::{insert_u32, unmarshal_signature, write_signature};

mod body;

pub use body::{MarshalledMessageBody, MessageBodyParser};

/// Starting point for new messages. Create either a call or a signal
#[derive(Default)]
pub struct MessageBuilder {
    msg: MarshalledMessage,
}

/// Created by MessageBuilder::call. Use it to make a new call to a service
pub struct CallBuilder {
    msg: MarshalledMessage,
}

/// Created by MessageBuilder::signal. Use it to make a new signal
pub struct SignalBuilder {
    msg: MarshalledMessage,
}

impl MessageBuilder {
    /// New messagebuilder with the default little endian byteorder
    pub fn new() -> MessageBuilder {
        MessageBuilder {
            msg: MarshalledMessage::new(),
        }
    }

    /// New messagebuilder with a chosen byteorder
    pub fn with_byteorder(b: ByteOrder) -> MessageBuilder {
        MessageBuilder {
            msg: MarshalledMessage::with_byteorder(b),
        }
    }

    pub fn call<S: Into<String>>(mut self, member: S) -> CallBuilder {
        self.msg.typ = MessageType::Call;
        self.msg.dynheader.member = Some(member.into());
        CallBuilder { msg: self.msg }
    }
    pub fn signal<S1, S2, S3>(mut self, interface: S1, member: S2, object: S3) -> SignalBuilder
    where
        S1: Into<String>,
        S2: Into<String>,
        S3: Into<String>,
    {
        self.msg.typ = MessageType::Signal;
        self.msg.dynheader.member = Some(member.into());
        self.msg.dynheader.interface = Some(interface.into());
        self.msg.dynheader.object = Some(object.into());
        SignalBuilder { msg: self.msg }
    }
}

impl CallBuilder {
    pub fn on<S: Into<String>>(mut self, object_path: S) -> Self {
        self.msg.dynheader.object = Some(object_path.into());
        self
    }

    pub fn with_interface<S: Into<String>>(mut self, interface: S) -> Self {
        self.msg.dynheader.interface = Some(interface.into());
        self
    }

    pub fn at<S: Into<String>>(mut self, destination: S) -> Self {
        self.msg.dynheader.destination = Some(destination.into());
        self
    }

    pub fn build(self) -> MarshalledMessage {
        self.msg
    }
}

impl SignalBuilder {
    pub fn to<S: Into<String>>(mut self, destination: S) -> Self {
        self.msg.dynheader.destination = Some(destination.into());
        self
    }

    pub fn build(self) -> MarshalledMessage {
        self.msg
    }
}

/// Message received by a connection or in preparation before being sent over a connection.
///
/// This represents a message while it is being built before it is sent over the connection.
/// The body accepts everything that implements the Marshal trait (e.g. all basic types, strings, slices, Hashmaps,.....)
/// And you can of course write an Marshal impl for your own datastructures. See the doc on the Marshal trait what you have
/// to look out for when doing this though.
#[derive(Debug)]
pub struct MarshalledMessage {
    pub body: MarshalledMessageBody,

    pub dynheader: DynamicHeader,

    pub typ: MessageType,
    pub flags: u8,
}

impl Default for MarshalledMessage {
    fn default() -> Self {
        Self::new()
    }
}

impl MarshalledMessage {
    pub fn get_buf(&self) -> &[u8] {
        &self.body.buf()
    }
    pub fn get_sig(&self) -> &str {
        &self.body.sig()
    }

    /// New message with the default little endian byteorder
    pub fn new() -> Self {
        MarshalledMessage {
            typ: MessageType::Invalid,
            dynheader: DynamicHeader::default(),

            flags: 0,
            body: MarshalledMessageBody::new(),
        }
    }

    /// New messagebody with a chosen byteorder
    pub fn with_byteorder(b: ByteOrder) -> Self {
        MarshalledMessage {
            typ: MessageType::Invalid,
            dynheader: DynamicHeader::default(),

            flags: 0,
            body: MarshalledMessageBody::with_byteorder(b),
        }
    }

    /// Reserves space for `additional` bytes in the internal buffer. This is useful to reduce the amount of allocations done while marshalling,
    /// if you can predict somewhat accuratly how many bytes you will be marshalling.
    pub fn reserve(&mut self, additional: usize) {
        self.body.reserve(additional)
    }
    pub fn marshal_header(
        &self,
        serial: NonZeroU32,
        hdr_buf: &mut Vec<u8>,
    ) -> Result<(), super::Error> {
        hdr_buf.clear();
        let mut _fds = Vec::new();
        let mut ctx = MarshalContext {
            byteorder: self.body.byteorder(),
            fds: &mut _fds,
            buf: hdr_buf,
        };
        let bo_byte: u8 = match ctx.byteorder {
            ByteOrder::LittleEndian => b'l',
            ByteOrder::BigEndian => b'B',
        };
        bo_byte.marshal(&mut ctx).unwrap();
        let t_byte: u8 = match self.typ {
            MessageType::Invalid => 0,
            MessageType::Call => 1,
            MessageType::Reply => 2,
            MessageType::Error => 3,
            MessageType::Signal => 4,
        };
        t_byte.marshal(&mut ctx).unwrap();
        self.flags.marshal(&mut ctx).unwrap();
        1u8.marshal(&mut ctx).unwrap();
        (self.body.buf().len() as u32).marshal(&mut ctx).unwrap();
        u32::from(serial).marshal(&mut ctx).unwrap();
        0u32.marshal(&mut ctx).unwrap();
        if let Some(obj) = &self.dynheader.object {
            let path = ObjectPath::from_str(obj)
                .map_err(|_| super::Error::Validation(ValidationErr::InvalidObjectPath))?;
            (1u8, HdrVar::Path(path)).marshal(&mut ctx).unwrap();
        }
        if let Some(iface) = &self.dynheader.interface {
            validate_interface(&iface)?;
            (2u8, HdrVar::Str(iface)).marshal(&mut ctx).unwrap();
        }
        if let Some(member) = &self.dynheader.member {
            validate_membername(member)?;
            (3u8, HdrVar::Str(member)).marshal(&mut ctx).unwrap();
        }
        if let Some(err) = &self.dynheader.error_name {
            validate_errorname(err)?;
            (4u8, HdrVar::Str(err)).marshal(&mut ctx).unwrap();
        }
        if let Some(rsp_idx) = &self.dynheader.response_serial {
            (5u8, HdrVar::U32((*rsp_idx).into()))
                .marshal(&mut ctx)
                .unwrap();
        }
        if let Some(dest) = &self.dynheader.destination {
            validate_busname(dest)?;
            (6u8, HdrVar::Str(dest)).marshal(&mut ctx).unwrap();
        }
        if let Some(sender) = &self.dynheader.sender {
            validate_busname(sender)?;
            (7u8, HdrVar::Str(sender)).marshal(&mut ctx).unwrap();
        }
        if !self.body.buf().is_empty() {
            let sig = self.body.sig();
            debug_assert!(SigStr::new(sig).is_ok());
            let ss = unsafe { SigStr::new_no_val(sig) };
            (8u8, HdrVar::Sig(ss)).marshal(&mut ctx).unwrap();
        }
        let fd_cnt = self.body.fds().len() as u32;
        if fd_cnt != 0 {
            (9u8, HdrVar::U32(fd_cnt)).marshal(&mut ctx).unwrap();
        }
        let len = ctx.buf.len() - (HEADER_LEN + 4);
        insert_u32(ctx.byteorder, len as u32, &mut ctx.buf[HEADER_LEN..]);
        ctx.align_to(8);
        Ok(())
    }
}

/// The dynamic part of a dbus message header
#[derive(Debug, Clone, Default)]
pub struct DynamicHeader {
    pub interface: Option<String>,
    pub member: Option<String>,
    pub object: Option<String>,
    pub destination: Option<String>,
    pub serial: Option<NonZeroU32>,
    pub sender: Option<String>,
    pub signature: Option<String>,
    pub error_name: Option<String>,
    pub response_serial: Option<NonZeroU32>,
    pub num_fds: Option<u32>,
}

impl DynamicHeader {
    /// Make a correctly addressed error response with the correct response serial
    pub fn make_error_response<S: Into<String>>(
        &self,
        error_name: S,
        error_msg: Option<String>,
    ) -> MarshalledMessage {
        let mut err_resp = MarshalledMessage {
            typ: MessageType::Error,
            dynheader: DynamicHeader {
                interface: None,
                member: None,
                object: None,
                destination: self.sender.clone(),
                serial: None,
                num_fds: None,
                sender: None,
                signature: None,
                response_serial: self.serial,
                error_name: Some(error_name.into()),
            },
            flags: 0,
            body: MarshalledMessageBody::new(),
        };
        if let Some(text) = error_msg {
            err_resp.body.push_param(text).unwrap();
        }
        err_resp
    }
    /// Make a correctly addressed response with the correct response serial
    pub fn make_response(&self) -> MarshalledMessage {
        MarshalledMessage {
            typ: MessageType::Reply,
            dynheader: DynamicHeader {
                interface: None,
                member: None,
                object: None,
                destination: self.sender.clone(),
                serial: None,
                num_fds: None,
                sender: None,
                signature: None,
                response_serial: self.serial,
                error_name: None,
            },
            flags: 0,
            body: MarshalledMessageBody::new(),
        }
    }
    pub fn validate(&self) -> bool {
        if let Some(iface) = &self.interface {
            if validate_interface(iface).is_err() {
                return false;
            }
        }
        if let Some(path) = &self.object {
            if ObjectPath::from_str(path).is_err() {
                return false;
            }
        }
        if let Some(member) = &self.member {
            if validate_membername(member).is_err() {
                return false;
            }
        }
        if let Some(err) = &self.error_name {
            if validate_errorname(err).is_err() {
                return false;
            }
        }
        if let Some(dest) = &self.destination {
            if validate_busname(&dest).is_err() {
                return false;
            }
        }
        if let Some(sender) = &self.sender {
            if validate_busname(&sender).is_err() {
                return false;
            }
        }
        if let Some(sig) = &self.signature {
            if validate_signature(&sig).is_err() {
                return false;
            }
        }
        true
    }
}
impl Signature for DynamicHeader {
    fn signature() -> super::signature::Type {
        Type::Container(Container::Dict(
            Base::Byte,
            Box::new(Type::Container(Container::Variant)),
        ))
    }
    fn alignment() -> usize {
        4
    }
}

use super::wire::unmarshal;
impl<'buf, 'fds> Unmarshal<'buf, 'fds> for DynamicHeader {
    fn unmarshal(ctx: &mut UnmarshalContext<'fds, 'buf>) -> unmarshal::UnmarshalResult<Self> {
        let start = ctx.offset;
        let (_, len) = u32::unmarshal(ctx)?;
        let len = len as usize;

        let end = align_num(ctx.offset + len, 8);
        if end > ctx.buf.len() {
            return Err(unmarshal::Error::NotEnoughBytes);
        }
        let mut fields: [Option<HdrVar>; 10] = Default::default();
        let mut array_used = 0;
        while array_used < len {
            let (u, (idx, var)) = <(u8, HdrVar)>::unmarshal(ctx)?;
            fields[idx as usize] = Some(var);
            array_used += u;
        }
        if array_used != len {
            return Err(unmarshal::Error::NotEnoughBytesForCollection);
        }
        let mut ret = DynamicHeader::default();
        if let Some(var) = &fields[1] {
            match var {
                HdrVar::Path(p) => ret.object = Some(p.to_string()),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if let Some(var) = &fields[2] {
            match var {
                HdrVar::Str(p) => ret.interface = Some(p.to_string()),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if let Some(var) = &fields[3] {
            match var {
                HdrVar::Str(p) => ret.member = Some(p.to_string()),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if let Some(var) = &fields[4] {
            match var {
                HdrVar::Str(p) => ret.error_name = Some(p.to_string()),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if let Some(var) = &fields[5] {
            match var {
                HdrVar::U32(p) if *p != 0 => ret.response_serial = Some((*p).try_into().unwrap()),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if let Some(var) = &fields[6] {
            match var {
                HdrVar::Str(p) => ret.destination = Some(p.to_string()),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if let Some(var) = &fields[7] {
            match var {
                HdrVar::Str(p) => ret.sender = Some(p.to_string()),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if let Some(var) = &fields[8] {
            match var {
                HdrVar::Sig(p) => ret.signature = Some(p.to_string()),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if let Some(var) = &fields[9] {
            match var {
                HdrVar::U32(p) => ret.num_fds = Some(*p),
                _ => return Err(unmarshal::Error::InvalidHeaderField),
            }
        }
        if !ret.validate() {
            Err(unmarshal::Error::InvalidHeaderField)
        } else {
            ctx.align_to(8)?;
            Ok((ctx.offset - start, ret))
        }
    }
}

#[derive(Debug)]
#[doc(hidden)]
pub struct SigStr(str);

impl SigStr {
    fn new(sig: &str) -> Result<&Self, ValidationErr> {
        validate_signature(sig)?;
        Ok(unsafe { Self::new_no_val(sig) })
    }
    unsafe fn new_no_val(sig: &str) -> &Self {
        &*(sig as *const str as *const SigStr)
    }
}
use std::ops::Deref;
impl Deref for SigStr {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Signature for &SigStr {
    fn signature() -> Type {
        Type::Base(Base::Signature)
    }
    fn alignment() -> usize {
        Self::signature().get_alignment()
    }
    fn sig_str(s_buf: &mut SignatureBuffer) {
        s_buf.push_static("g");
    }
}
impl<'buf, 'fds> Unmarshal<'buf, 'fds> for &'buf SigStr {
    fn unmarshal(ctx: &mut UnmarshalContext<'fds, 'buf>) -> unmarshal::UnmarshalResult<Self> {
        let (used, sig) = unmarshal_signature(&ctx.buf[ctx.offset..])?;
        ctx.offset += used;
        let ret = SigStr::new(sig)?;
        Ok((used, ret))
    }
}
impl Marshal for &SigStr {
    fn marshal(&self, ctx: &mut MarshalContext) -> Result<(), super::Error> {
        write_signature(&self.0, ctx.buf);
        Ok(())
    }
}

use super::dbus_variant_var;
dbus_variant_var!(HdrVar, Path => &'buf ObjectPath; Str => &'buf str; U32 => u32; Sig => &'buf SigStr);
