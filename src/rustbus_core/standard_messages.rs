//! Some standard messages that are often needed
use crate::rustbus_core;

use rustbus_core::message_builder::DynamicHeader;
use rustbus_core::message_builder::MarshalledMessage;
use rustbus_core::message_builder::MessageBuilder;

pub fn hello() -> MarshalledMessage {
    make_standard_msg("Hello")
}

pub fn ping(dest: String) -> MarshalledMessage {
    MessageBuilder::new()
        .call("Ping")
        .on("/org/freedesktop/DBus")
        .with_interface("org.freedesktop.DBus.Peer")
        .at(dest)
        .build()
}

pub fn ping_bus() -> MarshalledMessage {
    MessageBuilder::new()
        .call("Ping")
        .on("/org/freedesktop/DBus")
        .with_interface("org.freedesktop.DBus.Peer")
        .build()
}

pub fn list_names() -> MarshalledMessage {
    make_standard_msg("ListNames")
}

pub const DBUS_NAME_FLAG_ALLOW_REPLACEMENT: u32 = 1;
pub const DBUS_NAME_FLAG_REPLACE_EXISTING: u32 = 1 << 1;
pub const DBUS_NAME_FLAG_DO_NOT_QUEUE: u32 = 1 << 2;

pub const DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER: u32 = 1;
pub const DBUS_REQUEST_NAME_REPLY_IN_QUEUE: u32 = 2;
pub const DBUS_REQUEST_NAME_REPLY_EXISTS: u32 = 3;
pub const DBUS_REQUEST_NAME_REPLY_ALREADY_OWNER: u32 = 4;

fn make_standard_msg(name: &str) -> MarshalledMessage {
    MessageBuilder::new()
        .call(name)
        .on("/org/freedesktop/DBus")
        .with_interface("org.freedesktop.DBus")
        .at("org.freedesktop.DBus")
        .build()
}
/// Request a name on the bus
pub fn request_name(name: &str, flags: u32) -> MarshalledMessage {
    let mut msg = make_standard_msg("RequestName");
    msg.body.push_param(name).unwrap();
    msg.body.push_param(flags).unwrap();
    msg
}

/// Release a name on the bus
pub fn release_name(name: &str) -> MarshalledMessage {
    let mut msg = make_standard_msg("ReleaseName");
    msg.body.push_param(name).unwrap();
    msg
}

/// Add a match rule to receive signals. e.g. match_rule = "type='signal'" to get all signals
pub fn add_match(match_rule: &str) -> MarshalledMessage {
    let mut msg = make_standard_msg("AddMatch");
    msg.body.push_param(match_rule).unwrap();
    msg
}
/// Remove a match rule to receive signals. e.g. match_rule = "type='signal'" to get all signals
pub fn remove_match(match_rule: &str) -> MarshalledMessage {
    let mut msg = make_standard_msg("RemoveMatch");
    msg.body.push_param(match_rule).unwrap();
    msg
}
/// Error message to tell the caller that this method is not known by your server
pub fn unknown_method(call: &DynamicHeader) -> MarshalledMessage {
    let text = format!(
        "No calls to {}.{} are accepted for object {}",
        call.interface.clone().unwrap_or_else(|| "".to_owned()),
        call.member.clone().unwrap_or_else(|| "".to_owned()),
        call.object.clone().unwrap_or_else(|| "".to_owned()),
    );
    call.make_error_response(
        "org.freedesktop.DBus.Error.UnknownMethod".to_owned(),
        Some(text),
    )
}

/// Error message to tell the caller that this method uses a different interface than what the caller provided as parameters
pub fn invalid_args(call: &DynamicHeader, sig: Option<&str>) -> MarshalledMessage {
    let text = format!(
        "Invalid arguments for calls to {}.{} on object {} {}",
        call.interface.clone().unwrap_or_else(|| "".to_owned()),
        call.member.clone().unwrap_or_else(|| "".to_owned()),
        call.object.clone().unwrap_or_else(|| "".to_owned()),
        if let Some(sig) = sig {
            format!("expected signature: {}", sig)
        } else {
            "".to_owned()
        }
    );

    call.make_error_response(
        "org.freedesktop.DBus.Error.InvalidArgs".to_owned(),
        Some(text),
    )
}
