//use std::sync::Arc;
use super::{ByteOrder, SigStr};
use crate::rustbus_core;
use rustbus_core::message_builder::{MarshalContext, UnmarshalContext};
use rustbus_core::wire::marshal::traits::Marshal;
use rustbus_core::wire::unixfd::UnixFd;
use rustbus_core::wire::unmarshal::traits::Unmarshal;
use rustbus_core::wire::validate_raw;

/// The body accepts everything that implements the Marshal trait (e.g. all basic types, strings, slices, Hashmaps,.....)
/// And you can of course write an Marshal impl for your own datastrcutures
#[derive(Debug)]
pub struct MarshalledMessageBody {
    buf: Vec<u8>,
    sig: String,

    // out of band data
    raw_fds: Vec<rustbus_core::wire::UnixFd>,

    byteorder: ByteOrder,
}

impl Default for MarshalledMessageBody {
    fn default() -> Self {
        Self::new()
    }
}

impl MarshalledMessageBody {
    /// New messagebody with the default little endian byteorder
    pub fn new() -> Self {
        MarshalledMessageBody {
            buf: Vec::new(),
            raw_fds: Vec::new(),
            sig: String::new(),
            byteorder: ByteOrder::LittleEndian,
        }
    }

    /// New messagebody with a chosen byteorder
    pub fn with_byteorder(b: ByteOrder) -> Self {
        MarshalledMessageBody {
            buf: Vec::new(),
            raw_fds: Vec::new(),
            sig: String::new(),
            byteorder: b,
        }
    }

    pub fn from_parts(
        buf: Vec<u8>,
        raw_fds: Vec<rustbus_core::wire::UnixFd>,
        sig: String,
        byteorder: ByteOrder,
    ) -> Self {
        Self {
            buf,
            sig,
            raw_fds,
            byteorder,
        }
    }
    pub fn sig(&self) -> &str {
        &self.sig
    }
    pub fn buf(&self) -> &[u8] {
        &self.buf
    }
    pub fn fds(&self) -> &[UnixFd] {
        &self.raw_fds
    }
    pub fn byteorder(&self) -> ByteOrder {
        self.byteorder
    }
    /// Get a clone of all the `UnixFd`s in the body.
    ///
    /// Some of the `UnixFd`s may already have their `RawFd`s taken.
    pub fn get_fds(&self) -> Vec<UnixFd> {
        self.raw_fds.clone()
    }
    /// Clears the buffer and signature but holds on to the memory allocations. You can now start pushing new
    /// params as if this were a new message. This allows to reuse the OutMessage for the same dbus-message with different
    /// parameters without allocating the buffer every time.
    pub fn reset(&mut self) {
        self.sig.clear();
        self.buf.clear();
    }

    /// Reserves space for `additional` bytes in the internal buffer. This is useful to reduce the amount of allocations done while marshalling,
    /// if you can predict somewhat accuratly how many bytes you will be marshalling.
    pub fn reserve(&mut self, additional: usize) {
        self.buf.reserve(additional)
    }

    fn create_ctx(&mut self) -> MarshalContext {
        MarshalContext {
            buf: &mut self.buf,
            fds: &mut self.raw_fds,
            byteorder: self.byteorder,
        }
    }

    /// Append something that is Marshal to the message body
    pub fn push_param<P: Marshal>(&mut self, p: P) -> Result<(), rustbus_core::Error> {
        let mut ctx = self.create_ctx();
        p.marshal(&mut ctx)?;
        P::signature().to_str(&mut self.sig);
        Ok(())
    }

    /// Append two things that are Marshal to the message body
    pub fn push_param2<P1: Marshal, P2: Marshal>(
        &mut self,
        p1: P1,
        p2: P2,
    ) -> Result<(), rustbus_core::Error> {
        self.push_param(p1)?;
        self.push_param(p2)?;
        Ok(())
    }

    /// Append three things that are Marshal to the message body
    pub fn push_param3<P1: Marshal, P2: Marshal, P3: Marshal>(
        &mut self,
        p1: P1,
        p2: P2,
        p3: P3,
    ) -> Result<(), rustbus_core::Error> {
        self.push_param(p1)?;
        self.push_param(p2)?;
        self.push_param(p3)?;
        Ok(())
    }

    /// Append four things that are Marshal to the message body
    pub fn push_param4<P1: Marshal, P2: Marshal, P3: Marshal, P4: Marshal>(
        &mut self,
        p1: P1,
        p2: P2,
        p3: P3,
        p4: P4,
    ) -> Result<(), rustbus_core::Error> {
        self.push_param(p1)?;
        self.push_param(p2)?;
        self.push_param(p3)?;
        self.push_param(p4)?;
        Ok(())
    }

    /// Append five things that are Marshal to the message body
    pub fn push_param5<P1: Marshal, P2: Marshal, P3: Marshal, P4: Marshal, P5: Marshal>(
        &mut self,
        p1: P1,
        p2: P2,
        p3: P3,
        p4: P4,
        p5: P5,
    ) -> Result<(), rustbus_core::Error> {
        self.push_param(p1)?;
        self.push_param(p2)?;
        self.push_param(p3)?;
        self.push_param(p4)?;
        self.push_param(p5)?;
        Ok(())
    }

    /// Append any number of things that have the same type that is Marshal to the message body
    pub fn push_params<P: Marshal>(&mut self, params: &[P]) -> Result<(), rustbus_core::Error> {
        for p in params {
            self.push_param(p)?;
        }
        Ok(())
    }

    /// Append something that is Marshal to the body but use a dbus Variant in the signature. This is necessary for some APIs
    pub fn push_variant<P: Marshal>(&mut self, p: P) -> Result<(), rustbus_core::Error> {
        let mut ctx = self.create_ctx();
        marshal_as_variant(p, &mut ctx)
    }
    /// Validate the all the marshalled elements of the body.
    pub fn validate(&self) -> Result<(), rustbus_core::wire::unmarshal::Error> {
        if self.sig.is_empty() && self.buf.is_empty() {
            return Ok(());
        }
        let types = rustbus_core::signature::Type::parse_description(&self.sig)?;
        let mut used = 0;
        for typ in types {
            used += validate_raw::validate_marshalled(self.byteorder, used, &self.buf, &typ)
                .map_err(|(_, e)| e)?;
        }
        if used == self.buf.len() {
            Ok(())
        } else {
            Err(rustbus_core::wire::unmarshal::Error::NotAllBytesUsed)
        }
    }
    /// Create a parser to retrieve parameters from the body.
    #[inline]
    pub fn parser(&self) -> MessageBodyParser {
        MessageBodyParser::new(&self)
    }
}

/// Iterate over the messages parameters
///
/// Because dbus allows for multiple toplevel params without an enclosing struct, this provides a simple Iterator (sadly not std::iterator::Iterator, since the types
/// of the parameters can be different)
/// that you can use to get the params one by one, calling `get::<T>` until you have obtained all the parameters.
/// If you try to get more parameters than the signature has types, it will return None, if you try to get a parameter that doesn not
/// fit the current one, it will return an Error::WrongSignature, but you can safely try other types, the iterator stays valid.
#[derive(Debug)]
pub struct MessageBodyParser<'body> {
    buf_idx: usize,
    sig_idx: usize,
    sigs: Vec<rustbus_core::signature::Type>,
    body: &'body MarshalledMessageBody,
}

impl<'ret, 'fds, 'body: 'ret + 'fds> MessageBodyParser<'body> {
    pub fn new(body: &'body MarshalledMessageBody) -> Self {
        let sigs = match rustbus_core::signature::Type::parse_description(&body.sig) {
            Ok(sigs) => sigs,
            Err(e) => match e {
                rustbus_core::signature::Error::EmptySignature => Vec::new(),
                _ => panic!("MarshalledMessageBody has bad signature: {:?}", e),
            },
        };
        Self {
            buf_idx: 0,
            sig_idx: 0,
            sigs,
            body,
        }
    }

    /// Get the next params signature (if any are left)
    pub fn get_next_sig(&self) -> Option<&rustbus_core::signature::Type> {
        self.sigs.get(self.sig_idx)
    }

    /// Get the remaining params signature (if any are left)
    pub fn get_left_sigs(&self) -> Option<&[rustbus_core::signature::Type]> {
        self.sigs.get(self.sig_idx..)
    }

    /// Get the next param, use get::<TYPE> to specify what type you expect. For example `let s = parser.get::<String>()?;`
    /// This checks if there are params left in the message and if the type you requested fits the signature of the message.
    pub fn get<T: Unmarshal<'body, 'fds>>(
        &mut self,
    ) -> Result<T, rustbus_core::wire::unmarshal::Error> {
        if self.sig_idx >= self.sigs.len() {
            return Err(rustbus_core::wire::unmarshal::Error::EndOfMessage);
        }
        if self.sigs[self.sig_idx] != T::signature() {
            return Err(rustbus_core::wire::unmarshal::Error::WrongSignature);
        }

        let mut ctx = UnmarshalContext {
            byteorder: self.body.byteorder,
            buf: &self.body.buf,
            offset: self.buf_idx,
            fds: &self.body.raw_fds,
        };
        match T::unmarshal(&mut ctx) {
            Ok((bytes, res)) => {
                self.buf_idx += bytes;
                self.sig_idx += 1;
                Ok(res)
            }
            Err(e) => Err(e),
        }
    }
    /// Perform error handling for `get2(), get3()...` if `get_calls` fails.
    fn get_mult_helper<T, F>(
        &mut self,
        count: usize,
        get_calls: F,
    ) -> Result<T, rustbus_core::wire::unmarshal::Error>
    where
        F: FnOnce(&mut Self) -> Result<T, rustbus_core::wire::unmarshal::Error>,
    {
        if self.sig_idx + count > self.sigs.len() {
            return Err(rustbus_core::wire::unmarshal::Error::EndOfMessage);
        }
        let start_sig_idx = self.sig_idx;
        let start_buf_idx = self.buf_idx;
        match get_calls(self) {
            Ok(ret) => Ok(ret),
            Err(err) => {
                self.sig_idx = start_sig_idx;
                self.buf_idx = start_buf_idx;
                Err(err)
            }
        }
    }

    /// Get the next two params, use get2::<TYPE, TYPE> to specify what type you expect. For example `let s = parser.get2::<String, i32>()?;`
    /// This checks if there are params left in the message and if the type you requested fits the signature of the message.
    pub fn get2<T1, T2>(&mut self) -> Result<(T1, T2), rustbus_core::wire::unmarshal::Error>
    where
        T1: Unmarshal<'body, 'fds>,
        T2: Unmarshal<'body, 'fds>,
    {
        let get_calls = |parser: &mut Self| {
            let ret1 = parser.get()?;
            let ret2 = parser.get()?;
            Ok((ret1, ret2))
        };
        self.get_mult_helper(2, get_calls)
    }

    /// Get the next three params, use get3::<TYPE, TYPE, TYPE> to specify what type you expect. For example `let s = parser.get3::<String, i32, u64>()?;`
    /// This checks if there are params left in the message and if the type you requested fits the signature of the message.
    pub fn get3<T1, T2, T3>(&mut self) -> Result<(T1, T2, T3), rustbus_core::wire::unmarshal::Error>
    where
        T1: Unmarshal<'body, 'fds>,
        T2: Unmarshal<'body, 'fds>,
        T3: Unmarshal<'body, 'fds>,
    {
        let get_calls = |parser: &mut Self| {
            let ret1 = parser.get()?;
            let ret2 = parser.get()?;
            let ret3 = parser.get()?;
            Ok((ret1, ret2, ret3))
        };
        self.get_mult_helper(3, get_calls)
    }

    /// Get the next four params, use get4::<TYPE, TYPE, TYPE, TYPE> to specify what type you expect. For example `let s = parser.get4::<String, i32, u64, u8>()?;`
    /// This checks if there are params left in the message and if the type you requested fits the signature of the message.
    pub fn get4<T1, T2, T3, T4>(
        &mut self,
    ) -> Result<(T1, T2, T3, T4), rustbus_core::wire::unmarshal::Error>
    where
        T1: Unmarshal<'body, 'fds>,
        T2: Unmarshal<'body, 'fds>,
        T3: Unmarshal<'body, 'fds>,
        T4: Unmarshal<'body, 'fds>,
    {
        let get_calls = |parser: &mut Self| {
            let ret1 = parser.get()?;
            let ret2 = parser.get()?;
            let ret3 = parser.get()?;
            let ret4 = parser.get()?;
            Ok((ret1, ret2, ret3, ret4))
        };
        self.get_mult_helper(4, get_calls)
    }

    /// Get the next five params, use get5::<TYPE, TYPE, TYPE, TYPE, TYPE> to specify what type you expect. For example `let s = parser.get4::<String, i32, u64, u8, bool>()?;`
    /// This checks if there are params left in the message and if the type you requested fits the signature of the message.
    pub fn get5<T1, T2, T3, T4, T5>(
        &mut self,
    ) -> Result<(T1, T2, T3, T4, T5), rustbus_core::wire::unmarshal::Error>
    where
        T1: Unmarshal<'body, 'fds>,
        T2: Unmarshal<'body, 'fds>,
        T3: Unmarshal<'body, 'fds>,
        T4: Unmarshal<'body, 'fds>,
        T5: Unmarshal<'body, 'fds>,
    {
        let get_calls = |parser: &mut Self| {
            let ret1 = parser.get()?;
            let ret2 = parser.get()?;
            let ret3 = parser.get()?;
            let ret4 = parser.get()?;
            let ret5 = parser.get()?;
            Ok((ret1, ret2, ret3, ret4, ret5))
        };
        self.get_mult_helper(5, get_calls)
    }
}

fn marshal_as_variant<T: Marshal>(
    val: T,
    ctx: &mut MarshalContext,
) -> Result<(), rustbus_core::Error> {
    let sig = T::signature();
    let mut s_str = String::with_capacity(255);
    sig.to_str(&mut s_str);
    // SAFETY: assertion makes this safe
    debug_assert!(SigStr::new(&s_str).is_ok());
    let ss = unsafe { SigStr::new_no_val(&s_str) };
    ss.marshal(ctx).unwrap();
    val.marshal(ctx)
}

#[test]
fn test_marshal_trait() {
    let mut body = MarshalledMessageBody::new();
    let bytes: &[&[_]] = &[&[4u64]];
    body.push_param(bytes).unwrap();

    assert_eq!(
        vec![12, 0, 0, 0, 8, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0],
        body.buf
    );
    assert_eq!(body.sig.as_str(), "aat");

    let mut body = MarshalledMessageBody::new();
    let mut map = std::collections::HashMap::new();
    map.insert("a", 4u32);

    body.push_param(&map).unwrap();
    assert_eq!(
        vec![12, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, b'a', 0, 0, 0, 4, 0, 0, 0,],
        body.buf
    );
    assert_eq!(body.sig.as_str(), "a{su}");

    let mut body = MarshalledMessageBody::new();
    body.push_param((11u64, "str", true)).unwrap();
    assert_eq!(body.sig.as_str(), "(tsb)");
    assert_eq!(
        vec![11, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, b's', b't', b'r', 0, 1, 0, 0, 0,],
        body.buf
    );

    struct MyStruct {
        x: u64,
        y: String,
    }

    use rustbus_core::wire::marshal::traits::Signature;
    use rustbus_core::wire::marshal::MarshalContext;
    impl Signature for &MyStruct {
        fn signature() -> rustbus_core::signature::Type {
            rustbus_core::signature::Type::Container(rustbus_core::signature::Container::Struct(
                rustbus_core::signature::StructTypes::new(vec![
                    u64::signature(),
                    String::signature(),
                ])
                .unwrap(),
            ))
        }

        fn alignment() -> usize {
            8
        }
    }
    impl Marshal for &MyStruct {
        fn marshal(&self, ctx: &mut MarshalContext) -> Result<(), rustbus_core::Error> {
            // always align to 8
            ctx.align_to(8);
            self.x.marshal(ctx)?;
            self.y.marshal(ctx)?;
            Ok(())
        }
    }

    let mut body = MarshalledMessageBody::new();
    body.push_param(&MyStruct {
        x: 100,
        y: "A".to_owned(),
    })
    .unwrap();
    assert_eq!(body.sig.as_str(), "(ts)");
    assert_eq!(
        vec![100, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, b'A', 0,],
        body.buf
    );

    let mut body = MarshalledMessageBody::new();
    let emptymap: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    let mut map = std::collections::HashMap::new();
    let mut map2 = std::collections::HashMap::new();
    map.insert("a", 4u32);
    map2.insert("a", &map);

    body.push_param(&map2).unwrap();
    body.push_param(&emptymap).unwrap();
    assert_eq!(body.sig.as_str(), "a{sa{su}}a{su}");
    assert_eq!(
        vec![
            28, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, b'a', 0, 0, 0, 12, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0,
            0, b'a', 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0
        ],
        body.buf
    );

    // try to unmarshal stuff
    let mut body_iter = MessageBodyParser::new(&body);

    // first try some stuff that has the wrong signature
    type WrongNestedDict =
        std::collections::HashMap<String, std::collections::HashMap<String, u64>>;
    assert_eq!(
        body_iter.get::<WrongNestedDict>().err().unwrap(),
        rustbus_core::wire::unmarshal::Error::WrongSignature
    );
    type WrongStruct = (u64, i32, String);
    assert_eq!(
        body_iter.get::<WrongStruct>().err().unwrap(),
        rustbus_core::wire::unmarshal::Error::WrongSignature
    );

    // the get the correct type and make sure the content is correct
    type NestedDict = std::collections::HashMap<String, std::collections::HashMap<String, u32>>;
    let newmap2: NestedDict = body_iter.get().unwrap();
    assert_eq!(newmap2.len(), 1);
    assert_eq!(newmap2.get("a").unwrap().len(), 1);
    assert_eq!(*newmap2.get("a").unwrap().get("a").unwrap(), 4);

    // again try some stuff that has the wrong signature
    assert_eq!(
        body_iter.get::<WrongNestedDict>().err().unwrap(),
        rustbus_core::wire::unmarshal::Error::WrongSignature
    );
    assert_eq!(
        body_iter.get::<WrongStruct>().err().unwrap(),
        rustbus_core::wire::unmarshal::Error::WrongSignature
    );

    // get the empty map next
    let newemptymap: std::collections::HashMap<&str, u32> = body_iter.get().unwrap();
    assert_eq!(newemptymap.len(), 0);

    // test get2()
    let mut body_iter = body.parser();
    assert_eq!(
        body_iter.get2::<NestedDict, u16>().unwrap_err(),
        rustbus_core::wire::unmarshal::Error::WrongSignature
    );
    assert_eq!(
        body_iter
            .get3::<NestedDict, std::collections::HashMap<&str, u32>, u32>()
            .unwrap_err(),
        rustbus_core::wire::unmarshal::Error::EndOfMessage
    );

    // test to make sure body_iter is left unchanged from last failure and the map is
    // pulled out identically from above
    let (newmap2, newemptymap): (NestedDict, std::collections::HashMap<&str, u32>) =
        body_iter.get2().unwrap();
    // repeat assertions from above
    assert_eq!(newmap2.len(), 1);
    assert_eq!(newmap2.get("a").unwrap().len(), 1);
    assert_eq!(*newmap2.get("a").unwrap().get("a").unwrap(), 4);
    assert_eq!(newemptymap.len(), 0);
    assert_eq!(
        body_iter.get::<u16>().unwrap_err(),
        rustbus_core::wire::unmarshal::Error::EndOfMessage
    );

    // test mixed get() and get_param()
    let mut body_iter = body.parser();

    // test to make sure body_iter is left unchanged from last failure and the map is
    // pulled out identically from above
    let newmap2: NestedDict = body_iter.get().unwrap();
    // repeat assertions from above
    assert_eq!(newmap2.len(), 1);
    assert_eq!(newmap2.get("a").unwrap().len(), 1);
    assert_eq!(*newmap2.get("a").unwrap().get("a").unwrap(), 4);
}
