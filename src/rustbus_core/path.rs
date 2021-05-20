//! Structs for validating, marshalling and unmarshalling DBus object paths.
//!
//! [`ObjectPath`] and [`ObjectPathBuf`] are based on [`Path`] and [`PathBuf`] from `std::path` respectively.
//! Most of the same methods are implemented with minor semantic differences.
//! These differences arise because all valid `ObjectPath`s are absolute.
//! See each method for details.
//! `ObjectPath` implements `Deref` for `Path`
//! allowing it to be easily used in context requring a standard library path.
//! Also because all `ObjectPath`s are valid Rust `str`s,
//! there are easy methods to convert them to `&str`s.
//!
//! `ObjectPath`s are subsets of `str`s and `Path`s
//! and can be created from them if they meet the rules detailed in the section below.
//! These methods can also be used to simpily validate `str`s or `Path`s.
//!
//! # Restrictions on valid DBus object paths
//! * All DBus object paths are absolute. They always start with a `/`.
//! * Each element in the path are seperated by `/`.
//!   These elements can contain the ASCII characters `[A-Z][a-z][0-9]_`.
//! * There cannot be consecutive `/` seperators.
//!   In otherwords, each element must be a non-zero length.
//! * The last character cannot be a `/` seperator, unless the path is a root path (a single `/`).
//!
//! The relevant portion of the DBus Specification can be found [here].
//!
//! [here]: https://dbus.freedesktop.org/doc/dbus-specification.html#message-protocol-marshaling-object-path
use crate::rustbus_core;
//use crate::RpcConn;
//use rustbus_core::message_builder::MessageBuilder;
use rustbus_core::signature::{Base, Type};
use rustbus_core::wire::marshal::traits::{Marshal, Signature};
use rustbus_core::wire::marshal::MarshalContext;
use rustbus_core::wire::unmarshal::traits::Unmarshal;
use rustbus_core::wire::unmarshal::Error as UnmarshalError;
use rustbus_core::wire::unmarshal::{UnmarshalContext, UnmarshalResult};

use std::borrow::{Borrow, ToOwned};
use std::cmp::Ordering;
use std::convert::TryFrom;
use std::ffi::{OsStr, OsString};
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, StripPrefixError};
use std::str::FromStr;

/// Error type enumerating typical ways a standard path may be an invalid object path.
#[derive(Debug)]
pub enum InvalidObjectPath {
    NoRoot,
    ContainsInvalidCharacters,
    ConsecutiveSlashes,
    TrailingSlash,
}

/// A slice of a Dbus object path akin to a [`str`] or [`std::path::Path`].
///
/// Contains some methods for manipulating Dbus object paths,
/// similiar to `std::path::Path` with some minor differences.
#[derive(PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ObjectPath {
    inner: Path,
}
impl ObjectPath {
    fn validate_skip_root(path: &Path) -> Result<(), InvalidObjectPath> {
        let path_str = path
            .to_str()
            .ok_or(InvalidObjectPath::ContainsInvalidCharacters)?;
        let mut last_was_sep = false;
        for character in path_str.chars() {
            match character {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '_' => {
                    last_was_sep = false;
                }
                '/' => {
                    if last_was_sep {
                        return Err(InvalidObjectPath::ConsecutiveSlashes);
                    } else {
                        last_was_sep = true;
                    }
                }
                _ => return Err(InvalidObjectPath::ContainsInvalidCharacters),
            }
        }
        if path_str.len() != 1 && path_str.ends_with('/') {
            return Err(InvalidObjectPath::TrailingSlash);
        }
        Ok(())
    }
    fn validate<P: AsRef<Path>>(path: P) -> Result<(), InvalidObjectPath> {
        let path = path.as_ref();
        if !path.has_root() {
            return Err(InvalidObjectPath::NoRoot);
        }
        Self::validate_skip_root(path)
    }
    fn debug_assert_validitity(&self) {
        #[cfg(debug_assertions)]
        Self::validate(self).expect("Failed to validate the object path!");
    }
    /// Validate and make a `ObjectPath` from a normal path.
    ///
    /// See module [root] for the rules of a valid `ObjectPath`.
    /// # Examples
    /// ```
    /// use async_rustbus::rustbus_core::path::ObjectPath;
    /// let path = ObjectPath::new("/example/path").unwrap();
    /// ObjectPath::new("invalid/because/not/absolute").unwrap_err();
    /// ObjectPath::new("/invalid/because//double/sep").unwrap_err();
    /// ```
    /// [root]: ./index.html#restrictions-on-valid-dbus-object-paths
    pub fn new<P: AsRef<Path> + ?Sized>(p: &P) -> Result<&ObjectPath, InvalidObjectPath> {
        let path = p.as_ref();
        let ret = unsafe {
            Self::validate(path)?;
            Self::new_no_val(path)
        };
        Ok(ret)
    }
    unsafe fn new_no_val(p: &Path) -> &ObjectPath {
        &*(p as *const Path as *const ObjectPath)
    }
    /// Get the bytes that make up an `ObjectPath`.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_os_str().as_bytes()
    }
    /// Get the `ObjectPath` as a `&str`.
    ///
    /// Unlike ordinary `std::path::Path`, `ObjectPath`s are always valid Rust `str`s making this possible.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.debug_assert_validitity();

        let bytes = self.as_bytes();
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }
    /// Strip the prefix of the `ObjectPath`.
    ///
    /// Unlike [`Path::strip_prefix`] this method will always leave the path will always remain absolute.
    /// # Examples
    /// ```
    /// use async_rustbus::rustbus_core::path::ObjectPath;
    /// let original  = ObjectPath::new("/example/path/to_strip").unwrap();
    /// let target = ObjectPath::new("/path/to_strip").unwrap();
    /// /* These two lines are equivelent because paths must always remain absolute,
    ///    so the root '/' is readded in the second example.
    ///    Note the second line is not a valid ObjectPath */
    /// let stripped0 = original.strip_prefix("/example").unwrap();
    /// let stripped1 = original.strip_prefix("/example/").unwrap();
    /// assert_eq!(stripped0, target);
    /// assert_eq!(stripped1, target);
    ///
    /// original.strip_prefix("/example/other").unwrap_err();
    /// original.strip_prefix("/example/pa").unwrap_err();
    ///
    /// // Because the only thing stripped is the root sep this does nothing as it gets readded.
    /// let stripped2 = original.strip_prefix("/").unwrap();
    /// assert_eq!(stripped2, original);
    /// let stripped3 = original.strip_prefix(original).unwrap();
    /// assert_eq!(stripped3, ObjectPath::root_path());
    /// ```
    /// [`Path::strip_prefix`]: https://doc.rust-lang.org/std/path/struct.Path.html#method.strip_prefix
    pub fn strip_prefix<P: AsRef<Path> + ?Sized>(
        &self,
        p: &P,
    ) -> Result<&ObjectPath, StripPrefixError> {
        let stripped = self.inner.strip_prefix(p.as_ref())?;
        if stripped == Path::new("") {
            Ok(unsafe { ObjectPath::new_no_val("/".as_ref()) })
        } else {
            // Get a stripped path that includes the leading seperator.
            // This leading seperator is exists because ObjectPath.inner must be absolute;
            let self_bytes = self.as_bytes();
            let self_len = self_bytes.len(); // Unix-only
            let stripped_len = stripped.as_os_str().len();
            let ret_bytes = &self_bytes[self_len - 1 - stripped_len..];

            // convert bytes to ObjectPath
            let ret = OsStr::from_bytes(ret_bytes);
            let ret = unsafe { ObjectPath::new_no_val(ret.as_ref()) };
            ret.debug_assert_validitity();
            Ok(ret)
        }
    }
    /// Get the parent of the `ObjectPath` by removing the last element.
    /// If the `ObjectPath` is a root path then `None` is returned.
    pub fn parent(&self) -> Option<&ObjectPath> {
        let pp = self.inner.parent()?;
        let ret = unsafe { Self::new_no_val(pp) };
        ret.debug_assert_validitity();
        Some(ret)
    }
    /// Retrieves the last element of the `ObjectPath`.
    /// If the `ObjectPath` is a root path then `None` is returned.
    #[inline]
    pub fn file_name(&self) -> Option<&str> {
        self.debug_assert_validitity();
        let bytes = self.inner.file_name()?.as_bytes();
        unsafe { Some(std::str::from_utf8_unchecked(bytes)) }
    }
    /// Return a `ObjectPath` consisting of a single `/` seperator.
    #[inline]
    pub fn root_path() -> &'static Self {
        unsafe { ObjectPath::new_no_val("/".as_ref()) }
    }
    /// Returns an `Iterator` over the elements of an `ObjectPath`.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.debug_assert_validitity();
        self.inner.components().skip(1).map(|c| match c {
            Component::Normal(os) => unsafe { std::str::from_utf8_unchecked(os.as_bytes()) },
            _ => unreachable!("All the components of a ObjectPath are normal!"),
        })
    }
    pub fn to_object_path_buf(&self) -> ObjectPathBuf {
        ObjectPathBuf::from(self)
    }
}
impl Display for ObjectPath {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
impl Display for ObjectPathBuf {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.deref().fmt(f)
    }
}
impl Marshal for &ObjectPath {
    fn marshal(&self, ctx: &mut MarshalContext) -> Result<(), rustbus_core::Error> {
        self.as_str().marshal(ctx)
    }
}
impl Marshal for ObjectPathBuf {
    fn marshal(&self, ctx: &mut MarshalContext) -> Result<(), rustbus_core::Error> {
        self.deref().marshal(ctx)
    }
}

impl Deref for ObjectPath {
    type Target = std::path::Path;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl ToOwned for ObjectPath {
    type Owned = ObjectPathBuf;
    #[inline]
    fn to_owned(&self) -> Self::Owned {
        self.to_object_path_buf()
    }
}

impl Borrow<Path> for ObjectPath {
    #[inline]
    fn borrow(&self) -> &Path {
        self.deref()
    }
}
impl AsRef<Path> for ObjectPath {
    #[inline]
    fn as_ref(&self) -> &Path {
        self.deref()
    }
}
impl AsRef<ObjectPath> for ObjectPath {
    #[inline]
    fn as_ref(&self) -> &ObjectPath {
        self
    }
}
impl AsRef<str> for ObjectPath {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl AsRef<OsStr> for ObjectPath {
    #[inline]
    fn as_ref(&self) -> &OsStr {
        self.inner.as_ref()
    }
}

impl<'a> From<&'a ObjectPath> for &'a str {
    #[inline]
    fn from(path: &'a ObjectPath) -> Self {
        path.as_str()
    }
}
impl<'a> TryFrom<&'a str> for &'a ObjectPath {
    type Error = InvalidObjectPath;
    fn try_from(s: &'a str) -> Result<Self, Self::Error> {
        ObjectPath::new(s)
    }
}
impl<'a> TryFrom<&'a Path> for &'a ObjectPath {
    type Error = InvalidObjectPath;
    fn try_from(p: &'a Path) -> Result<Self, Self::Error> {
        ObjectPath::new(p)
    }
}

impl Signature for &ObjectPath {
    fn signature() -> Type {
        Type::Base(Base::ObjectPath)
    }
    fn alignment() -> usize {
        Self::signature().get_alignment()
    }
}

impl<'buf, 'fds> Unmarshal<'buf, 'fds> for &'buf ObjectPath {
    fn unmarshal(ctx: &mut UnmarshalContext<'fds, 'buf>) -> UnmarshalResult<Self> {
        let (bytes, val) = <&str>::unmarshal(ctx)?;
        let path = ObjectPath::new(val).map_err(|_| UnmarshalError::InvalidType)?;
        Ok((bytes, path))
    }
}
impl Signature for ObjectPathBuf {
    fn signature() -> Type {
        <&ObjectPath>::signature()
    }
    fn alignment() -> usize {
        <&ObjectPath>::alignment()
    }
}
impl<'buf, 'fds> Unmarshal<'buf, 'fds> for ObjectPathBuf {
    fn unmarshal(ctx: &mut UnmarshalContext<'fds, 'buf>) -> UnmarshalResult<Self> {
        <&ObjectPath>::unmarshal(ctx).map(|(size, op)| (size, op.to_owned()))
    }
}

#[derive(Eq, Clone, Debug, Default)]
pub struct ObjectPathBuf {
    inner: Option<PathBuf>,
}
impl Hash for ObjectPathBuf {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.deref().hash(state);
    }
}
impl PartialEq<ObjectPathBuf> for ObjectPathBuf {
    fn eq(&self, other: &ObjectPathBuf) -> bool {
        self.deref().eq(other.deref())
    }
}
impl PartialOrd<ObjectPathBuf> for ObjectPathBuf {
    fn partial_cmp(&self, other: &ObjectPathBuf) -> Option<Ordering> {
        self.deref().partial_cmp(other)
    }
}
impl Ord for ObjectPathBuf {
    fn cmp(&self, other: &Self) -> Ordering {
        self.deref().cmp(other)
    }
}
/// An owned, mutable Dbus object path akin to [`String`] or [`std::path::PathBuf`].
///
/// [`push`], [`pop`] and others can be used used to modify the `ObjectPathBuf` in-place.
/// `ObjectPathBuf` implements `Deref` to `ObjectPath`
/// allowing for all methods on `ObjectPath` to be used on `ObjectPathBuf`.
/// # Notes
/// * `ObjectPathBuf` is stored as a wrapper around `Option<PathBuf>`, where the `None` case
///   is equivelent to a root path.
///   
/// * As a result of the above point,
///   root paths (a single `/` seperator) are special case that does not result in a heap allocation.
///   This means that [`new`] does not result in an allocation on its own.
///
/// [`push`]: ./struct.ObjectPathBuf.html#method.push
/// [`pop`]: ./struct.ObjectPathBuf.html#method.pop
/// [`new`]: ./struct.ObjectPathBuf.html#method.new
impl ObjectPathBuf {
    /// Create a new root path consisting of a single `/` seperator.
    ///
    /// The `ObjectPathBuf` returned by this method does not result in an allocation until it is modified.
    #[inline]
    pub fn new() -> ObjectPathBuf {
        ObjectPathBuf { inner: None }
    }
    /// Create a new root path and preallocate space on the heap for additions to the path.
    ///
    /// If the size of the object path is known ahead time,
    /// this method can provide a performance benefit by avoid multiple reallocations.
    ///
    /// When `capacity` is zero this method is equivelent to `new`.
    pub fn with_capacity(capacity: usize) -> ObjectPathBuf {
        let inner = if capacity == 0 {
            None
        } else {
            let mut pb = PathBuf::with_capacity(capacity);
            pb.push("/");
            Some(pb)
        };
        ObjectPathBuf { inner }
    }
    /// Coerces to a [`ObjectPath`] slice.
    #[inline]
    pub fn as_object_path(&self) -> &ObjectPath {
        self.deref()
    }
    unsafe fn from_path_buf(pb: PathBuf) -> Self {
        Self { inner: Some(pb) }
    }
    /// Truncates the object path into a root path.
    ///
    /// This does not affect the capacity of the `ObjectPathBuf`.
    #[inline]
    pub fn clear(&mut self) {
        if let Some(buf) = &mut self.inner {
            buf.clear();
        }
    }
    #[inline]
    /// Append an `ObjectPath` to this one.
    pub fn push(&mut self, path: &ObjectPath) {
        let path = Path::strip_prefix(path, "/").expect("All object paths start with '/'");
        unsafe {
            self.push_path_unchecked(path);
        }
    }
    unsafe fn push_path_unchecked(&mut self, path: &Path) {
        let len = path.as_os_str().len();
        if len == 0 {
            return;
        }
        self.reserve(len + 1);
        let inner = self
            .inner
            .as_mut()
            .expect("The reserve call cause a PathBuf to allows be allocated.");
        inner.push(path);
        self.debug_assert_validitity();
    }
    /// Append a `Path` to this one.
    ///
    /// If `path` is invalid this method panics.
    /// If it is unknown if `path` is valid use [`push_path_checked`] instead.
    ///
    /// # Panics
    /// `path` must be a valid object path with two exceptions:
    /// `path` can be relative or empty.
    /// If the above conditions are not met this function will panic.
    ///
    /// # Examples
    /// ```
    /// use std::convert::TryFrom;
    /// use async_rustbus::rustbus_core::path::{ObjectPath, ObjectPathBuf};
    /// let target = ObjectPath::new("/example/path/to_append").unwrap();
    ///
    /// let mut opb0  = ObjectPathBuf::try_from("/example").unwrap();
    /// let mut opb1 = opb0.clone();
    /// opb0.push_path("/path/to_append");
    /// opb1.push_path("path/to_append");
    /// assert_eq!(&opb0, target);
    /// assert_eq!(&opb1, target);
    /// ```
    /// These should panic for different reasons.
    /// ```should_panic
    /// use std::convert::TryFrom;
    /// use async_rustbus::rustbus_core::path::{ObjectPath, ObjectPathBuf};
    /// let target = ObjectPath::new("/example/path/to_append").unwrap();
    /// let mut original = ObjectPathBuf::try_from("/example").unwrap();
    ///
    /// // Each line panics for different reasons
    /// original.push_path("/path//consecutive/slash");
    /// original.push_path("/p@th");
    /// ```
    #[inline]
    pub fn push_path<P: AsRef<Path>>(&mut self, path: P) {
        self.push_path_checked(path)
            .expect("Invalid path was passed!");
    }
    /// Check and append `path` to the `ObjectPathBuf` if it is valid.
    ///
    /// `path` must be a valid DBus object path with two exceptions:
    /// `path` can be relative or empty.
    /// If these conditions are not met then an `Err` is returned.
    /// # Examples
    /// ```
    /// use std::convert::TryFrom;
    /// use async_rustbus::rustbus_core::path::{ObjectPath, ObjectPathBuf};
    /// let target = ObjectPath::new("/example/path/to_append").unwrap();
    ///
    /// let mut opb0  = ObjectPathBuf::try_from("/example").unwrap();
    /// let mut opb1 = opb0.clone();
    /// opb0.push_path_checked("/path/to_append").unwrap();
    /// opb1.push_path_checked("path/to_append").unwrap();
    /// assert_eq!(&opb0, target);
    /// assert_eq!(&opb1, target);
    ///
    /// opb0.push_path_checked("/path//consecutive/slash").unwrap_err();
    /// opb1.push_path_checked("/p@th").unwrap_err();
    /// ```
    pub fn push_path_checked<P: AsRef<Path>>(&mut self, path: P) -> Result<(), InvalidObjectPath> {
        let path = path.as_ref();
        let path = path.strip_prefix("/").unwrap_or(path);
        ObjectPath::validate_skip_root(path)?;
        unsafe {
            self.push_path_unchecked(path);
        };
        Ok(())
    }
    /// Truncate the `ObjectPathBuf` to [`parent`].
    /// Returns true if the path changed.
    ///
    /// [`parent`]: ./struct.ObjectPath.html#method.parent
    #[inline]
    pub fn pop(&mut self) -> bool {
        self.inner.as_mut().map_or(false, PathBuf::pop)
    }
    pub fn reserve(&mut self, additional: usize) {
        if additional == 0 {
            return;
        }
        match &mut self.inner {
            Some(buf) => buf.reserve(additional),
            None => *self = Self::with_capacity(additional + 1),
        }
    }
    pub fn reserve_exact(&mut self, additional: usize) {
        if additional == 0 {
            return;
        }
        match &mut self.inner {
            Some(buf) => buf.reserve_exact(additional),
            None => {
                let mut buf = PathBuf::new();
                buf.reserve_exact(additional + 1);
                self.inner = Some(buf);
            }
        }
    }
    /// Get the capacity of the current heap allocation.
    ///
    /// If capacity is zero then there is no heap allocation, and the `ObjectPathBuf` is a root path.
    /// The root path is special case that can be stored without a heap allocation despite being length 1.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.inner.as_ref().map_or(0, PathBuf::capacity)
    }
}
impl TryFrom<OsString> for ObjectPathBuf {
    type Error = InvalidObjectPath;
    fn try_from(value: OsString) -> Result<Self, Self::Error> {
        ObjectPath::validate(&value)?;
        Ok(unsafe { ObjectPathBuf::from_path_buf(value.into()) })
    }
}
impl TryFrom<String> for ObjectPathBuf {
    type Error = InvalidObjectPath;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(OsString::from(value))
    }
}
impl TryFrom<PathBuf> for ObjectPathBuf {
    type Error = InvalidObjectPath;
    fn try_from(value: PathBuf) -> Result<Self, Self::Error> {
        ObjectPath::validate(&value)?;
        Ok(unsafe { ObjectPathBuf::from_path_buf(value) })
    }
}
impl TryFrom<&str> for ObjectPathBuf {
    type Error = InvalidObjectPath;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Ok(ObjectPath::new(value)?.to_object_path_buf())
    }
}
impl TryFrom<&OsStr> for ObjectPathBuf {
    type Error = InvalidObjectPath;
    fn try_from(value: &OsStr) -> Result<Self, Self::Error> {
        Ok(ObjectPath::new(value)?.to_object_path_buf())
    }
}
impl TryFrom<&Path> for ObjectPathBuf {
    type Error = InvalidObjectPath;
    fn try_from(value: &Path) -> Result<Self, Self::Error> {
        Ok(ObjectPath::new(value)?.to_object_path_buf())
    }
}

impl Deref for ObjectPathBuf {
    type Target = ObjectPath;
    fn deref(&self) -> &Self::Target {
        match &self.inner {
            Some(buf) => unsafe { ObjectPath::new_no_val(&buf) },
            None => ObjectPath::root_path(),
        }
    }
}
impl Borrow<ObjectPath> for ObjectPathBuf {
    fn borrow(&self) -> &ObjectPath {
        self.deref()
    }
}
impl AsRef<ObjectPath> for ObjectPathBuf {
    fn as_ref(&self) -> &ObjectPath {
        self.deref()
    }
}
impl AsRef<str> for ObjectPathBuf {
    fn as_ref(&self) -> &str {
        self.deref().as_ref()
    }
}
impl AsRef<Path> for ObjectPathBuf {
    fn as_ref(&self) -> &Path {
        self.deref().as_ref()
    }
}
impl FromStr for ObjectPathBuf {
    type Err = InvalidObjectPath;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let path: &Path = s.as_ref();
        let obj_path = ObjectPath::new(path)?;
        Ok(obj_path.to_owned())
    }
}
impl From<ObjectPathBuf> for PathBuf {
    fn from(buf: ObjectPathBuf) -> Self {
        match buf.inner {
            Some(buf) => buf,
            None => PathBuf::from("/"),
        }
    }
}
impl From<ObjectPathBuf> for String {
    fn from(path: ObjectPathBuf) -> Self {
        path.debug_assert_validitity();
        let bytes = match path.inner {
            Some(buf) => buf.into_os_string().into_vec(),
            None => Vec::from(&b"/"[..]),
        };

        unsafe { std::string::String::from_utf8_unchecked(bytes) }
    }
}
impl From<&ObjectPath> for ObjectPathBuf {
    fn from(path: &ObjectPath) -> Self {
        let ret = if path == ObjectPath::root_path() {
            ObjectPathBuf::new()
        } else {
            unsafe { ObjectPathBuf::from_path_buf(path.into()) }
        };
        ret.debug_assert_validitity();
        ret
    }
}

impl PartialEq<ObjectPath> for ObjectPathBuf {
    fn eq(&self, other: &ObjectPath) -> bool {
        self.deref().eq(other)
    }
}
#[cfg(test)]
mod tests {
    use super::{ObjectPath, ObjectPathBuf};
    use std::borrow::Borrow;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::path::Path;

    fn test_objpaths() -> Vec<&'static ObjectPath> {
        vec![
            ObjectPath::new("/org/freedesktop/NetworkManager").unwrap(),
            ObjectPath::new("/org/freedesktop/NetworkManager/ActiveConnection").unwrap(),
        ]
    }
    fn test_objpathbufs() -> Vec<ObjectPathBuf> {
        test_objpaths()
            .into_iter()
            .map(|op| op.to_owned())
            .collect()
    }
    // Borrow requires that the Ord trait prodces equivelent values before and after
    fn compare_ord_borrow<A, T, U>(pre: &[A]) -> Option<(usize, usize)>
    where
        T: Ord + Borrow<U> + ?Sized,
        U: Ord + ?Sized,
        A: Borrow<T>,
    {
        let pre_iter = pre.iter().map(|p| p.borrow());
        for (i, pre_i) in pre_iter.clone().enumerate() {
            for (j, pre_j) in pre_iter.clone().enumerate().skip(i) {
                let pre_ord = pre_i.cmp(pre_j);
                let post_i = pre_i.borrow();
                let post_j = pre_j.borrow();
                let post_ord = post_i.cmp(post_j);
                if pre_ord != post_ord {
                    return Some((i, j));
                }
            }
        }
        None
    }
    // Borrow requires that Hash trait produces equivelent values for before and after borrow()
    // This tests that invariant
    fn compare_hasher_borrow<A, T, U>(pre: &[A]) -> Option<usize>
    where
        T: Hash + Borrow<U> + ?Sized,
        U: Hash + ?Sized,
        A: Borrow<T>,
    {
        let pre_iter = pre.iter().map(|p| p.borrow());
        for (i, (pre, post)) in pre_iter
            .clone()
            .zip(pre_iter.map(|p| p.borrow()))
            .enumerate()
        {
            let mut pre_borrow_hasher = DefaultHasher::new();
            let mut post_borrow_hasher = DefaultHasher::new();
            pre.hash(&mut pre_borrow_hasher);
            post.hash(&mut post_borrow_hasher);
            if pre_borrow_hasher.finish() != post_borrow_hasher.finish() {
                return Some(i);
            }
        }
        None
    }
    #[test]
    fn test_objectpathbuf_borrow_objectpath() {
        let objpathbufs = test_objpathbufs();
        if let Some(i) =
            compare_hasher_borrow::<ObjectPathBuf, ObjectPathBuf, ObjectPath>(&objpathbufs[..])
        {
            panic!("Hash didn't match: {}", i);
        }
        if let Some((i, j)) =
            compare_ord_borrow::<ObjectPathBuf, ObjectPathBuf, ObjectPath>(&objpathbufs[..])
        {
            panic!("Ord didn't match for: {} {}", i, j);
        }
    }
    #[test]
    fn test_objectpath_borrow_path() {
        let objpaths = test_objpaths();
        if let Some(i) = compare_hasher_borrow::<&ObjectPath, ObjectPath, Path>(&objpaths[..]) {
            panic!("Hash didn't match: {}", i);
        }
        if let Some((i, j)) = compare_ord_borrow::<&ObjectPath, ObjectPath, Path>(&objpaths[..]) {
            panic!("Ord didn't match for: {} {}", i, j);
        }
    }
    #[test]
    fn test_push() {
        let objpath = ObjectPath::new("/dbus/test").unwrap();
        let objpath2 = ObjectPath::new("/freedesktop/more").unwrap();
        let mut objpathbuf = ObjectPathBuf::new();
        objpathbuf.push(objpath);
        assert_eq!(objpathbuf, *objpath);
        objpathbuf.push(objpath2);
        assert_eq!(
            &objpathbuf,
            ObjectPath::new("/dbus/test/freedesktop/more").unwrap()
        );
        assert!(objpathbuf.starts_with(objpath));
        assert!(!objpathbuf.starts_with(objpath2));
        assert_eq!(objpathbuf.strip_prefix(objpath).unwrap(), objpath2);
    }
}
/*
pub struct Child {
    path: ObjectPathBuf,
    interface: Vec<String>,
}
impl Child {
    pub fn path(&self) -> &ObjectPath {
        &self.path
    }
    pub fn interfaces(&self) -> &[String] {
        &self.interface[..]
    }
}
impl From<Child> for ObjectPathBuf {
    fn from(child: Child) -> Self {
        child.path
    }
}

pub async fn get_children<S: AsRef<str>, P: AsRef<ObjectPath>>(
    conn: &RpcConn,
    dest: S,
    path: P,
) -> std::io::Result<Vec<Child>> {
    use xml::reader::EventReader;
    let path: &str = path.as_ref().as_ref();
    let call = MessageBuilder::new()
        .call(String::from("Introspect"))
        .with_interface(String::from("org.freedesktop.DBus.Introspectable"))
        .on(path.to_string())
        .at(dest.as_ref().to_string())
        .build();
    let res = conn.send_msg_with_reply(&call).await?.await?;
    let s: &str = res.body.parser().get().unwrap();
    let mut reader = EventReader::from_str(s);
    eprintln!("get_children: {:?}", reader.next());
    unimplemented!()
}*/
