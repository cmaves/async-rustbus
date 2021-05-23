use crate::rustbus_core;
use rustbus_core::dbus_variant_var;
use rustbus_core::path::ObjectPath;
use super::SigStr;

dbus_variant_var!(HdrVar, Path => &'buf ObjectPath; Str => &'buf str; U32 => u32; Sig => &'buf SigStr);
