//! Data structures and methods related to DBus wire-format.
//!
//! The stuff in this module is not specific to the crate's implementation of a DBus connection.
//! Almost everything in this module is reexported from [`rustbus`]
//! with the notable exception of the [`path`] module,
//! which is instead home-grown.
//!
//! [`rustbus`]: https://crates.io/crates/rustbus
//! [`path`]: ./path/index.html
pub use rustbus::dbus_variant_var;
use rustbus::message_builder as org_message_builder;
pub use rustbus::signature;
pub use rustbus::wire;
pub use rustbus::ByteOrder;
pub use rustbus::Error;

pub mod path;

pub mod message_builder;

pub mod standard_messages;
