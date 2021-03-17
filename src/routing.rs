use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::{Debug, Formatter};

use async_std::path::{Component, Components, Path};

use super::rustbus_core;
use super::MsgQueue;
use rustbus_core::message_builder::MarshalledMessage;

enum CallHandler {
    Queue(MsgQueue),
    Exact(MsgQueue),
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
            CallHandler::Drop | CallHandler::Nothing => None,
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
        }
    }
}
#[derive(Debug)]
pub(crate) struct CallHierarchy {
    children: HashMap<OsString, CallHierarchy>,
    handler: CallHandler,
}
enum Status<'a> {
    Dropped,
    Unhandled,
    Queue(&'a MsgQueue),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallAction {
    Queue,
    Exact,
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
        if let Status::Queue(queue) = self.send_inner(tar_comps) {
            queue.send(msg);
            Ok(())
        } else {
            Err(msg)
        }
    }
    fn send_inner(&self, mut tar_comps: Components) -> Status {
        match tar_comps.next() {
            Some(Component::Normal(child)) => match self.children.get(child) {
                Some(child) => match child.send_inner(tar_comps) {
                    Status::Unhandled => match &self.handler {
                        CallHandler::Queue(q) => Status::Queue(q),
                        CallHandler::Nothing => Status::Unhandled,
                        CallHandler::Exact(_) | CallHandler::Drop => Status::Dropped,
                    },
                    handled => handled,
                },
                None => match &self.handler {
                    CallHandler::Queue(q) => Status::Queue(q),
                    CallHandler::Nothing => Status::Unhandled,
                    CallHandler::Exact(_) | CallHandler::Drop => Status::Dropped,
                },
            },
            None => match &self.handler {
                CallHandler::Queue(q) | CallHandler::Exact(q) => Status::Queue(q),
                CallHandler::Nothing => Status::Unhandled,
                CallHandler::Drop => Status::Dropped,
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
                        eprintln!("insert_inner(): self: {:#?}", self);
                        true
                    } else {
                        !matches!(self.handler, CallHandler::Nothing)
                    }
                }
            },
            None => {
                self.handler = action.into();
                eprintln!("insert_inner(): self: {:#?}", self);
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
                        Status::Queue(_) | Status::Dropped => false,
                        Status::Unhandled => self.handler.get_queue().is_some(),
                    },
                    None => self.handler.get_queue().is_some(),
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
