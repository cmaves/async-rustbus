use std::collections::hash_map::Iter;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write;
use std::fmt::{Debug, Formatter};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU8, Ordering};

use async_std::path::{Component, Components, Path};

use super::rustbus_core;
use super::MsgQueue;
use rustbus_core::message_builder::MarshalledMessage;

static mut MAP_TUPLE: (AtomicU8, MaybeUninit<HashMap<OsString, CallHierarchy>>) =
    (AtomicU8::new(0), MaybeUninit::uninit());

unsafe fn init_empty_map(flag: u8) -> &'static HashMap<OsString, CallHierarchy> {
    if flag == 0 {
        if let Ok(_) = MAP_TUPLE
            .0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
        {
            MAP_TUPLE.1 = MaybeUninit::new(HashMap::new());
            MAP_TUPLE.0.store(2, Ordering::Release);
            return std::mem::transmute(MAP_TUPLE.1.as_ptr());
        }
    }
    while MAP_TUPLE.0.load(Ordering::Acquire) != 2 {
        std::hint::spin_loop();
    }
    std::mem::transmute(MAP_TUPLE.1.as_ptr())
}
fn get_empty_map() -> &'static HashMap<OsString, CallHierarchy> {
    unsafe {
        let flag = MAP_TUPLE.0.load(Ordering::Acquire);
        if flag == 2 {
            return std::mem::transmute(MAP_TUPLE.1.as_ptr());
        }
        init_empty_map(flag)
    }
}

enum CallHandler {
    Queue(MsgQueue),
    Exact(MsgQueue),
    Intro,
    Nothing,
    Drop,
}
impl Debug for CallHandler {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            CallHandler::Exact(_) => write!(f, "CallHandler::Exact"),
            CallHandler::Queue(_) => write!(f, "CallHandler::Queue"),
            CallHandler::Nothing => write!(f, "CallHandler::Nothing"),
            CallHandler::Drop => write!(f, "CallHandler::Drop"),
            CallHandler::Intro => write!(f, "CallHandler::Intro"),
        }
    }
}
impl CallHandler {
    fn is_nothing(&self) -> bool {
        matches!(self, CallHandler::Nothing)
    }
    fn get_queue(&self) -> Option<&MsgQueue> {
        match self {
            CallHandler::Queue(q) | CallHandler::Exact(q) => Some(q),
            _ => None,
        }
    }
}
impl From<CallAction> for CallHandler {
    fn from(action: CallAction) -> Self {
        match action {
            CallAction::Queue => CallHandler::Queue(MsgQueue::new()),
            CallAction::Exact => CallHandler::Exact(MsgQueue::new()),
            CallAction::Drop => CallHandler::Drop,
            CallAction::Nothing => CallHandler::Nothing,
            CallAction::Intro => CallHandler::Intro,
        }
    }
}
#[derive(Debug)]
pub(crate) struct CallHierarchy {
    children: HashMap<OsString, CallHierarchy>,
    handler: CallHandler,
}
enum Status<'a> {
    Queue(&'a MsgQueue),
    Intro(Iter<'a, OsString, CallHierarchy>),
    Dropped,
    Unhandled(Iter<'a, OsString, CallHierarchy>),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallAction {
    Queue,
    Exact,
    Intro,
    Nothing,
    Drop,
}
impl From<&CallHandler> for CallAction {
    fn from(handler: &CallHandler) -> Self {
        match handler {
            CallHandler::Exact(_) => CallAction::Exact,
            CallHandler::Queue(_) => CallAction::Queue,
            CallHandler::Drop => CallAction::Drop,
            CallHandler::Nothing => CallAction::Nothing,
            CallHandler::Intro => CallAction::Intro,
        }
    }
}
impl CallHierarchy {
    pub fn new() -> Self {
        CallHierarchy {
            handler: CallHandler::Drop,
            children: HashMap::new(),
        }
    }
    pub fn send(&self, msg: MarshalledMessage) -> Result<(), MarshalledMessage> {
        let path = Path::new(msg.dynheader.object.as_ref().unwrap());
        let mut tar_comps = path.components();
        assert_eq!(tar_comps.next(), Some(Component::RootDir));
        match self.send_inner(tar_comps) {
            Status::Queue(queue) => {
                queue.send(msg);
                Ok(())
            }
            Status::Intro(keys) => Err(make_intro_msg(msg, keys)),
            Status::Unhandled(_) | Status::Dropped => Err(make_object_not_found(msg)),
        }
    }
    fn send_inner(&self, mut tar_comps: Components) -> Status {
        match tar_comps.next() {
            Some(Component::Normal(child)) => match self.children.get(child) {
                Some(child) => match child.send_inner(tar_comps) {
                    Status::Unhandled(keys) => match &self.handler {
                        CallHandler::Nothing => Status::Unhandled(keys),
                        CallHandler::Queue(q) => Status::Queue(q),
                        CallHandler::Intro => Status::Intro(keys),
                        CallHandler::Exact(_) | CallHandler::Drop => Status::Dropped,
                    },
                    handled => handled,
                },
                None => match &self.handler {
                    CallHandler::Queue(q) => Status::Queue(q),
                    CallHandler::Nothing => Status::Unhandled(get_empty_map().iter()),
                    CallHandler::Intro | CallHandler::Exact(_) | CallHandler::Drop => {
                        Status::Dropped
                    }
                },
            },
            None => match &self.handler {
                CallHandler::Queue(q) | CallHandler::Exact(q) => Status::Queue(q),
                CallHandler::Nothing => Status::Unhandled(self.children.iter()),
                CallHandler::Drop => Status::Dropped,
                CallHandler::Intro => Status::Intro(self.children.iter()),
            },
            _ => unreachable!(),
        }
    }
    fn insert_inner(&mut self, mut tar_comps: Components, action: CallAction) -> bool {
        match tar_comps.next() {
            Some(Component::Normal(child)) => match self.children.get_mut(child) {
                Some(entry) => {
                    if entry.insert_inner(tar_comps, action) {
                        true
                    } else {
                        self.children.remove(child);
                        !(self.children.is_empty() && self.handler.is_nothing())
                    }
                }
                None => {
                    let mut hierarchy = CallHierarchy::new();
                    hierarchy.handler = CallHandler::Nothing;
                    if hierarchy.insert_inner(tar_comps, action) {
                        self.children.insert(child.to_os_string(), hierarchy);
                        //eprintln!("insert_inner(): self: {:#?}", self);
                        true
                    } else {
                        !matches!(self.handler, CallHandler::Nothing)
                    }
                }
            },
            None => {
                self.handler = action.into();
                //eprintln!("insert_inner(): self: {:#?}", self);
                if self.handler.is_nothing() {
                    !self.children.is_empty()
                } else {
                    true
                }
            }
            _ => unreachable!(),
        }
    }
    pub fn insert_path(&mut self, path: &str, handler: CallAction) {
        let path = Path::new(path);
        let mut tar_comps = path.components();
        assert_eq!(tar_comps.next(), Some(Component::RootDir));
        self.insert_inner(tar_comps, handler);
    }
    fn find_inner(&self, mut tar_comps: Components) -> Option<&CallHandler> {
        match tar_comps.next() {
            Some(Component::Normal(child)) => self.children.get(child)?.find_inner(tar_comps),
            None => Some(&self.handler),
            _ => unreachable!(),
        }
    }
    fn find_handler(&self, path: &str) -> Option<&CallHandler> {
        let path = Path::new(path);
        let mut tar_comps = path.components();
        assert_eq!(tar_comps.next(), Some(Component::RootDir));
        self.find_inner(tar_comps)
    }
    pub fn get_queue(&self, path: &str) -> Option<&MsgQueue> {
        let handler = self.find_handler(path)?;
        handler.get_queue()
    }
    pub fn get_action(&self, path: &str) -> Option<CallAction> {
        let handler = self.find_handler(path)?;
        Some(handler.into())
    }
    fn is_match_inner(&self, mut org_comps: Components, mut msg_comps: Components) -> bool {
        match msg_comps.next() {
            Some(Component::Normal(msg)) => match org_comps.next() {
                Some(Component::Normal(org)) => {
                    if org == msg {
                        match self.children.get(org) {
                            Some(child) => child.is_match_inner(org_comps, msg_comps),
                            None => false,
                        }
                    } else {
                        false
                    }
                }
                None => match self.children.get(msg) {
                    Some(child) => match child.send_inner(msg_comps) {
                        Status::Queue(_) | Status::Dropped | Status::Intro(_) => false,
                        Status::Unhandled(_) => self.handler.get_queue().is_some(),
                    },
                    None => matches!(self.handler, CallHandler::Queue(_))
                },
                _ => unreachable!(),
            },
            None => match org_comps.next() {
                Some(_) => false,
                None => self.handler.get_queue().is_some(),
            },
            _ => unreachable!(),
        }
    }
    pub fn is_match(&self, org_path: &str, msg_path: &str) -> bool {
        let mut org_comps = Path::new(org_path).components();
        let mut msg_comps = Path::new(msg_path).components();
        assert_eq!(org_comps.next(), Some(Component::RootDir));
        assert_eq!(msg_comps.next(), Some(Component::RootDir));
        self.is_match_inner(org_comps, msg_comps)
    }
}

fn make_object_not_found(msg: MarshalledMessage) -> MarshalledMessage {
    msg.dynheader
        .make_error_response("UnknownObject".to_string(), None)
}

const INTRO_START: &'static str = "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">
 <node>
\t<interface name=\"org.freedesktop.DBus.Introspectable\">
\t\t<method name=\"Introspect\">
\t\t\t<arg name=\"xml_data\" type=\"s\" direction=\"out\"/>
\t\t</method>
\t</interface>\n";
const INTRO_END: &'static str = " </node>";

fn make_intro_msg(
    msg: MarshalledMessage,
    children: Iter<OsString, CallHierarchy>,
) -> MarshalledMessage {
    if msg.dynheader.interface.as_ref().unwrap() == "org.freedesktop.DBus.Introspectable" {
        let mut res = msg.dynheader.make_response();
        let mut intro_str = String::from(INTRO_START);
        let children = children.filter_map(|(s, c)| match c.handler {
            CallHandler::Drop => None,
            _ => s.to_str(),
        });
        for child in children {
            write!(intro_str, "\t<node name=\"{}\"/>\n", child).unwrap();
        }
        intro_str.push_str(INTRO_END);
        res.body.push_param(intro_str).unwrap();
        res
    } else {
        msg.dynheader
            .make_error_response("UnknownInterface".to_string(), None)
    }
}

#[cfg(test)]
mod tests {
    use super::{CallAction, CallHierarchy};
    #[test]
    fn call_hierarchy_insert() {
        let mut hierarchy = CallHierarchy::new();
        hierarchy.insert_path("/usr/local/bin", CallAction::Queue);
        assert_eq!(
            hierarchy.get_action("/usr/local/bin").unwrap(),
            CallAction::Queue
        );
        assert_eq!(
            hierarchy.get_action("/usr/local").unwrap(),
            CallAction::Nothing
        );
        assert_eq!(hierarchy.get_action("/usr").unwrap(), CallAction::Nothing);
        assert_eq!(hierarchy.get_action("/").unwrap(), CallAction::Drop);
        assert!(hierarchy.is_match("/usr/local/bin", "/usr/local/bin/echo"));
        assert!(hierarchy.is_match("/usr/local/bin", "/usr/local/bin"));
        assert!(!hierarchy.is_match("/usr/local", "/usr/local"));
        assert!(!hierarchy.is_match("/usr", "/usr/local"));
        assert!(!hierarchy.is_match("/", "/usr/local/bin"));
        assert!(!hierarchy.is_match("/", "/usr/local"));
        hierarchy.insert_path("/", CallAction::Queue);
        assert!(hierarchy.is_match("/", "/usr/local"));
        hierarchy.insert_path("/var", CallAction::Exact);
        hierarchy.insert_path("/var/log/journal", CallAction::Queue);
        assert!(hierarchy.is_match("/var/log/journal", "/var/log/journal"));
        assert!(hierarchy.is_match("/var", "/var/log"));
        assert!(!hierarchy.is_match("/", "/var/log"));
        assert!(!hierarchy.is_match("/", "/var"));
    }
    #[test]
    fn trimming() {
        let mut hierarchy = CallHierarchy::new();
        hierarchy.insert_path("/usr/local/bin", CallAction::Queue);
        hierarchy.insert_path("/usr/local/bin/hello/find", CallAction::Queue);
        hierarchy.insert_path("/usr/local/bin/hello/find", CallAction::Nothing);
        hierarchy.insert_path("/usr/local/bin", CallAction::Nothing);
        println!("{:#?}", hierarchy);
        assert!(hierarchy.children.is_empty());
        hierarchy.insert_path("/usr/local/bin", CallAction::Queue);
        hierarchy.insert_path("/usr/local/bin/hello/find", CallAction::Queue);
        hierarchy.insert_path("/usr/local/bin", CallAction::Nothing);
        hierarchy.insert_path("/usr/local/bin/hello/find", CallAction::Nothing);
        assert!(hierarchy.children.is_empty());
    }
}
