use std::cmp::Ordering as COrdering;
use std::collections::hash_map::Iter;
use std::collections::HashMap;
use std::fmt::Write;
use std::fmt::{Debug, Formatter};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use async_std::path::Path;

use super::rustbus_core;
use super::MsgQueue;
use rustbus_core::message_builder::{MarshalledMessage, MessageType};
use rustbus_core::path::ObjectPath;

static mut MAP_TUPLE: (AtomicU8, MaybeUninit<HashMap<String, CallHierarchy>>) =
    (AtomicU8::new(0), MaybeUninit::uninit());

unsafe fn init_empty_map(flag: u8) -> &'static HashMap<String, CallHierarchy> {
    if flag == 0
        && MAP_TUPLE
            .0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    {
        MAP_TUPLE.1 = MaybeUninit::new(HashMap::new());
        MAP_TUPLE.0.store(2, Ordering::Release);
        return &*MAP_TUPLE.1.as_ptr();
    }
    while MAP_TUPLE.0.load(Ordering::Acquire) != 2 {
        std::hint::spin_loop();
    }
    &*MAP_TUPLE.1.as_ptr()
}
fn get_empty_map() -> &'static HashMap<String, CallHierarchy> {
    unsafe {
        let flag = MAP_TUPLE.0.load(Ordering::Acquire);
        if flag == 2 {
            return &*MAP_TUPLE.1.as_ptr();
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
    children: HashMap<String, CallHierarchy>,
    handler: CallHandler,
}
enum Status<'a> {
    Queue(&'a MsgQueue),
    Intro(Iter<'a, String, CallHierarchy>),
    Dropped,
    Unhandled(Iter<'a, String, CallHierarchy>),
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
        let path = ObjectPath::new(msg.dynheader.object.as_ref().unwrap()).unwrap();
        let tar_comps = path.components();
        match self.send_inner(tar_comps) {
            Status::Queue(queue) => {
                queue.send(msg);
                Ok(())
            }
            Status::Intro(keys) => Err(make_intro_msg(msg, keys)),
            Status::Unhandled(_) | Status::Dropped => Err(make_object_not_found(msg)),
        }
    }
    fn send_inner<'a>(&self, mut tar_comps: impl Iterator<Item = &'a str>) -> Status {
        match tar_comps.next() {
            Some(child) => match self.children.get(child) {
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
        }
    }
    fn insert_inner<'a>(
        &mut self,
        mut tar_comps: impl Iterator<Item = &'a str>,
        action: CallAction,
    ) -> bool {
        match tar_comps.next() {
            Some(child) => match self.children.get_mut(child) {
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
                        self.children.insert(child.to_string(), hierarchy);
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
        }
    }
    pub fn insert_path(&mut self, path: &ObjectPath, handler: CallAction) {
        let tar_comps = path.components();
        self.insert_inner(tar_comps, handler);
    }
    fn find_inner<'a>(&self, mut tar_comps: impl Iterator<Item = &'a str>) -> Option<&CallHandler> {
        match tar_comps.next() {
            Some(child) => self.children.get(child)?.find_inner(tar_comps),
            None => Some(&self.handler),
        }
    }
    fn find_handler(&self, path: &ObjectPath) -> Option<&CallHandler> {
        let tar_comps = path.components();
        self.find_inner(tar_comps)
    }
    pub fn get_queue(&self, path: &ObjectPath) -> Option<&MsgQueue> {
        let handler = self.find_handler(path)?;
        handler.get_queue()
    }
    pub fn get_action(&self, path: &ObjectPath) -> Option<CallAction> {
        let handler = self.find_handler(path)?;
        Some(handler.into())
    }
    fn is_match_inner<'a>(
        &self,
        mut org_comps: impl Iterator<Item = &'a str>,
        mut msg_comps: impl Iterator<Item = &'a str>,
    ) -> bool {
        match msg_comps.next() {
            Some(msg) => match org_comps.next() {
                Some(org) => {
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
                    None => matches!(self.handler, CallHandler::Queue(_)),
                },
            },
            None => match org_comps.next() {
                Some(_) => false,
                None => self.handler.get_queue().is_some(),
            },
        }
    }
    pub fn is_match(&self, org_path: &ObjectPath, msg_path: &ObjectPath) -> bool {
        let org_comps = org_path.components();
        let msg_comps = msg_path.components();
        self.is_match_inner(org_comps, msg_comps)
    }
}

fn make_object_not_found(msg: MarshalledMessage) -> MarshalledMessage {
    msg.dynheader.make_error_response("UnknownObject", None)
}

const INTRO_START: &str = "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\" \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">
 <node>
\t<interface name=\"org.freedesktop.DBus.Introspectable\">
\t\t<method name=\"Introspect\">
\t\t\t<arg name=\"xml_data\" type=\"s\" direction=\"out\"/>
\t\t</method>
\t</interface>\n";
const INTRO_END: &str = " </node>";

fn make_intro_msg(
    msg: MarshalledMessage,
    children: Iter<String, CallHierarchy>,
) -> MarshalledMessage {
    if msg.dynheader.interface.as_ref().unwrap() == "org.freedesktop.DBus.Introspectable" {
        let mut res = msg.dynheader.make_response();
        let mut intro_str = String::from(INTRO_START);
        let children = children.filter_map(|(s, c)| match c.handler {
            CallHandler::Drop => None,
            _ => Some(s),
        });
        for child in children {
            writeln!(intro_str, "\t<node name=\"{}\"/>", child).unwrap();
        }
        intro_str.push_str(INTRO_END);
        res.body.push_param(intro_str).unwrap();
        res
    } else {
        msg.dynheader.make_error_response("UnknownInterface", None)
    }
}

#[derive(Default)]
pub struct Match {
    pub sender: Option<Arc<str>>,
    pub path: Option<Arc<str>>,
    pub path_namespace: Option<Arc<str>>,
    pub interface: Option<Arc<str>>,
    pub member: Option<Arc<str>>,
    pub(super) queue: Option<MsgQueue>,
}
pub const EMPTY_MATCH: &Match = &Match {
    sender: None,
    path: None,
    path_namespace: None,
    interface: None,
    member: None,
    queue: None,
};
impl Debug for Match {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut ds = f.debug_struct("Match");
        ds.field("sender", &self.sender);
        ds.field("path", &self.path);
        ds.field("path_namespace", &self.path_namespace);
        ds.field("interface", &self.interface);
        ds.field("member", &self.member);
        struct EmptyPrintable;
        impl Debug for EmptyPrintable {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "_")
            }
        }
        ds.field("queue", &self.queue.as_ref().map(|_| EmptyPrintable));
        ds.finish()
    }
}
impl Clone for Match {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            path: self.path.clone(),
            path_namespace: self.path_namespace.clone(),
            interface: self.interface.clone(),
            member: self.member.clone(),
            queue: None,
        }
    }
}
impl Match {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn sender<S: Into<String>>(&mut self, sender: S) -> &mut Self {
        self.sender = Some(sender.into().into());
        self
    }
    pub fn path<S: Into<String>>(&mut self, path: S) -> &mut Self {
        self.path = Some(path.into().into());
        self.path_namespace = None; // path_namespace is not allowed with path;
        self
    }
    pub fn path_namespace<S: Into<String>>(&mut self, path_namespace: S) -> &mut Self {
        self.path_namespace = Some(path_namespace.into().into());
        self.path = None;
        self
    }
    pub fn interface<S: Into<String>>(&mut self, interface: S) -> &mut Self {
        self.interface = Some(interface.into().into());
        self
    }
    pub fn member<S: Into<String>>(&mut self, member: S) -> &mut Self {
        self.member = Some(member.into().into());
        self
    }
    pub fn is_empty(&self) -> bool {
        EMPTY_MATCH == self
    }
    pub fn matches(&self, msg: &MarshalledMessage) -> bool {
        if !matches!(msg.typ, MessageType::Signal) {
            return false;
        }
        match (&self.sender, &msg.dynheader.sender) {
            (Some(ss), Some(ms)) => {
                if ss.as_ref() != ms {
                    return false;
                }
            }
            (Some(_), None) => return false,
            (None, _) => {}
        }
        match (&self.path, &msg.dynheader.object) {
            (Some(ss), Some(ms)) => {
                if ss.as_ref() != ms {
                    return false;
                }
            }
            (Some(_), None) => return false,
            (None, _) => {}
        }
        match (&self.path_namespace, &msg.dynheader.object) {
            (Some(ss), Some(ms)) => {
                if !Path::new(ms).starts_with(ss.as_ref()) {
                    return false;
                }
            }
            (Some(_), None) => return false,
            (None, _) => {}
        }
        match (&self.interface, &msg.dynheader.interface) {
            (Some(ss), Some(ms)) => {
                if ss.as_ref() != ms {
                    return false;
                }
            }
            (Some(_), None) => return false,
            (None, _) => {}
        }
        match (&self.member, &msg.dynheader.member) {
            (Some(ss), Some(ms)) => {
                if ss.as_ref() != ms {
                    return false;
                }
            }
            (Some(_), None) => return false,
            (None, _) => {}
        }
        true
    }
    pub fn match_string(&self) -> String {
        let mut match_str = String::new();
        if let Some(sender) = &self.sender {
            match_str.push_str("sender='");
            match_str.push_str(sender);
            match_str.push_str("',");
        }
        if let Some(path) = &self.path {
            match_str.push_str("path='");
            match_str.push_str(path);
            match_str.push_str("',");
        }
        if let Some(path_namespace) = &self.path_namespace {
            match_str.push_str("path_namespace='");
            match_str.push_str(path_namespace);
            match_str.push_str("',");
        }
        if let Some(interface) = &self.interface {
            match_str.push_str("interface='");
            match_str.push_str(interface);
            match_str.push_str("',");
        }
        if let Some(member) = &self.member {
            match_str.push_str("member='");
            match_str.push_str(member);
            match_str.push_str("',");
        }
        match_str.pop();
        match_str
    }
}
impl PartialEq<Match> for Match {
    fn eq(&self, other: &Match) -> bool {
        if self.sender != other.sender {
            return false;
        }
        if self.path != other.path {
            return false;
        }
        if self.path != other.path {
            return false;
        }
        if self.path_namespace != other.path_namespace {
            return false;
        }
        if self.interface != other.interface {
            return false;
        }
        if self.member != other.member {
            return false;
        }
        true
    }
}
impl Eq for Match {}
fn option_ord<T>(left: &Option<T>, right: &Option<T>) -> Option<COrdering> {
    match &left {
        Some(_) => {
            if right.is_none() {
                return Some(COrdering::Less);
            }
        }
        None => {
            if right.is_some() {
                return Some(COrdering::Greater);
            }
        }
    }
    None
}
fn path_subset(left: &Option<Arc<str>>, right: &Option<Arc<str>>) -> Option<COrdering> {
    if let Some(ord) = option_ord(&left, &right) {
        return Some(ord);
    }
    let mut l_path = match &left {
        Some(p) => Path::new(p.as_ref()).components(),
        None => return None,
    };
    let mut r_path = Path::new(right.as_ref().unwrap().as_ref()).components();
    loop {
        break match (l_path.next(), r_path.next()) {
            (Some(l_comp), Some(r_comp)) => {
                if l_comp == r_comp {
                    continue;
                } else {
                    None
                }
            }
            (Some(_), None) => Some(COrdering::Less),
            (None, Some(_)) => Some(COrdering::Greater),
            (None, None) => None,
        };
    }
}
impl Ord for Match {
    fn cmp(&self, other: &Self) -> COrdering {
        /*eprintln!("Match::cmp(\n\
        self: {:#?},\nother: {:#?})", self, other);*/
        if let Some(ord) = option_ord(&self.sender, &other.sender) {
            return ord;
        }
        if let Some(ord) = option_ord(&self.path, &other.path) {
            return ord;
        }
        if let Some(ord) = path_subset(&self.path_namespace, &other.path_namespace) {
            return ord;
        }
        if let Some(ord) = option_ord(&self.interface, &other.interface) {
            return ord;
        }
        if let Some(ord) = option_ord(&self.member, &other.member) {
            return ord;
        }
        let next_ord = match self.sender.cmp(&other.sender) {
            COrdering::Equal => self.path.cmp(&other.path),
            other => return other,
        };
        let next_ord = match next_ord {
            COrdering::Equal => self.path_namespace.cmp(&other.path_namespace),
            other => return other,
        };
        let next_ord = match next_ord {
            COrdering::Equal => self.interface.cmp(&other.interface),
            other => return other,
        };
        match next_ord {
            COrdering::Equal => self.member.cmp(&other.member),
            other => other,
        }
    }
}
impl PartialOrd<Match> for Match {
    fn partial_cmp(&self, other: &Match) -> Option<COrdering> {
        Some(self.cmp(other))
    }
}

pub fn queue_sig(sig_matches: &[Match], sig: MarshalledMessage) {
    for sig_match in sig_matches {
        if sig_match.matches(&sig) {
            sig_match.queue.as_ref().unwrap().send(sig);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rustbus_core;
    use super::{CallAction, CallHierarchy, Match, MsgQueue, EMPTY_MATCH};
    use std::convert::TryInto;

    #[test]
    fn call_hierarchy_insert() {
        let mut hierarchy = CallHierarchy::new();
        hierarchy.insert_path("/usr/local/bin".try_into().unwrap(), CallAction::Queue);
        assert_eq!(
            hierarchy
                .get_action("/usr/local/bin".try_into().unwrap())
                .unwrap(),
            CallAction::Queue
        );
        assert_eq!(
            hierarchy
                .get_action("/usr/local".try_into().unwrap())
                .unwrap(),
            CallAction::Nothing
        );
        assert_eq!(
            hierarchy.get_action("/usr".try_into().unwrap()).unwrap(),
            CallAction::Nothing
        );
        assert_eq!(
            hierarchy.get_action("/".try_into().unwrap()).unwrap(),
            CallAction::Drop
        );
        assert!(hierarchy.is_match(
            "/usr/local/bin".try_into().unwrap(),
            "/usr/local/bin/echo".try_into().unwrap()
        ));
        assert!(hierarchy.is_match(
            "/usr/local/bin".try_into().unwrap(),
            "/usr/local/bin".try_into().unwrap()
        ));
        assert!(!hierarchy.is_match(
            "/usr/local".try_into().unwrap(),
            "/usr/local".try_into().unwrap()
        ));
        assert!(!hierarchy.is_match("/usr".try_into().unwrap(), "/usr/local".try_into().unwrap()));
        assert!(!hierarchy.is_match(
            "/".try_into().unwrap(),
            "/usr/local/bin".try_into().unwrap()
        ));
        assert!(!hierarchy.is_match("/".try_into().unwrap(), "/usr/local".try_into().unwrap()));
        hierarchy.insert_path("/".try_into().unwrap(), CallAction::Queue);
        assert!(hierarchy.is_match("/".try_into().unwrap(), "/usr/local".try_into().unwrap()));
        hierarchy.insert_path("/var".try_into().unwrap(), CallAction::Exact);
        hierarchy.insert_path("/var/log/journal".try_into().unwrap(), CallAction::Queue);
        assert!(hierarchy.is_match(
            "/var/log/journal".try_into().unwrap(),
            "/var/log/journal".try_into().unwrap()
        ));
        assert!(hierarchy.is_match("/var".try_into().unwrap(), "/var/log".try_into().unwrap()));
        assert!(!hierarchy.is_match("/".try_into().unwrap(), "/var/log".try_into().unwrap()));
        assert!(!hierarchy.is_match("/".try_into().unwrap(), "/var".try_into().unwrap()));
    }
    #[test]
    fn trimming() {
        let mut hierarchy = CallHierarchy::new();
        hierarchy.insert_path("/usr/local/bin".try_into().unwrap(), CallAction::Queue);
        hierarchy.insert_path(
            "/usr/local/bin/hello/find".try_into().unwrap(),
            CallAction::Queue,
        );
        hierarchy.insert_path(
            "/usr/local/bin/hello/find".try_into().unwrap(),
            CallAction::Nothing,
        );
        hierarchy.insert_path("/usr/local/bin".try_into().unwrap(), CallAction::Nothing);
        println!("{:#?}", hierarchy);
        assert!(hierarchy.children.is_empty());
        hierarchy.insert_path("/usr/local/bin".try_into().unwrap(), CallAction::Queue);
        hierarchy.insert_path(
            "/usr/local/bin/hello/find".try_into().unwrap(),
            CallAction::Queue,
        );
        hierarchy.insert_path("/usr/local/bin".try_into().unwrap(), CallAction::Nothing);
        hierarchy.insert_path(
            "/usr/local/bin/hello/find".try_into().unwrap(),
            CallAction::Nothing,
        );
        assert!(hierarchy.children.is_empty());
    }

    #[test]
    fn match_order() {
        use rand::seq::SliceRandom;
        use rand::thread_rng;
        let mut w_sender = Match::new();
        w_sender.sender("org.freedesktop.DBus");
        let mut w_path = Match::new();
        w_path.path("/hello");
        let mut w_namespace0 = Match::new();
        w_namespace0.path_namespace("/org");
        let mut w_namespace1 = Match::new();
        w_namespace1.path_namespace("/org/freedesktop");
        let mut w_interface = Match::new();
        w_interface.interface("org.freedesktop.DBus");
        let mut w_member = Match::new();
        w_member.member("Peer");
        let mut array = [
            &w_sender,
            &w_path,
            &w_namespace0,
            &w_namespace1,
            &w_interface,
            &w_member,
        ];
        let mut rng = thread_rng();
        array.shuffle(&mut rng);
        array.sort_unstable();
        assert!(std::ptr::eq(&w_sender, array[0]));
        assert!(std::ptr::eq(&w_path, array[1]));
        assert!(std::ptr::eq(&w_namespace1, array[2]));
        assert!(std::ptr::eq(&w_namespace0, array[3]));
        assert!(std::ptr::eq(&w_interface, array[4]));
        assert!(std::ptr::eq(&w_member, array[5]));
    }
    use rustbus_core::message_builder::MessageBuilder;
    #[test]
    fn matches_single() {
        let m1 = Match::new().interface("io.test.Test1").clone();
        let mut m1_q = m1.clone();
        m1_q.queue = Some(MsgQueue::new());

        let m2 = Match::new().member("TestSig1").clone();
        let mut m2_q = m2.clone();
        m2_q.queue = Some(MsgQueue::new());

        let mut m3 = m2.clone();
        m3.interface = m1.interface.clone();
        let mut m3_q = m3.clone();
        m3_q.queue = Some(MsgQueue::new());

        let m4 = Match::new().path_namespace("/io/test").clone();
        let mut m4_q = m4.clone();
        m4_q.queue = Some(MsgQueue::new());

        let m5 = Match::new().path("/io/test/specific").clone();
        let mut m5_q = m5.clone();
        m5_q.queue = Some(MsgQueue::new());

        let m6 = Match::new().sender("io.test_sender").clone();
        let mut m6_q = m6.clone();
        m6_q.queue = Some(MsgQueue::new());

        let mut msg = MessageBuilder::new()
            .signal("io.test.Test1", "TestSig1", "/")
            .build();
        msg.dynheader.sender = Some("io.other".into());

        assert!(EMPTY_MATCH.matches(&msg));
        assert!(m1.matches(&msg));
        assert!(m1_q.matches(&msg));
        assert!(m2.matches(&msg));
        assert!(m2_q.matches(&msg));
        assert!(m3.matches(&msg));
        assert!(m3_q.matches(&msg));
        assert!(!m4.matches(&msg));
        assert!(!m4_q.matches(&msg));
        assert!(!m5.matches(&msg));
        assert!(!m5_q.matches(&msg));
        assert!(!m6.matches(&msg));
        assert!(!m6_q.matches(&msg));

        let mut other_if = Some("io.test.Test2".to_string());
        std::mem::swap(&mut msg.dynheader.interface, &mut other_if);
        assert!(!m1.matches(&msg));
        assert!(!m1_q.matches(&msg));
        assert!(m2.matches(&msg));
        assert!(m2_q.matches(&msg));
        assert!(!m3.matches(&msg));
        assert!(!m3_q.matches(&msg));
        std::mem::swap(&mut msg.dynheader.interface, &mut other_if);

        msg.dynheader.member = Some("TestSig2".into());
        assert!(m1.matches(&msg));
        assert!(m1_q.matches(&msg));
        assert!(!m2.matches(&msg));
        assert!(!m2_q.matches(&msg));
        assert!(!m3.matches(&msg));
        assert!(!m3_q.matches(&msg));

        msg.dynheader.object = Some("/io/test".into());
        assert!(m4.matches(&msg));
        assert!(m4_q.matches(&msg));
        assert!(!m5.matches(&msg));
        assert!(!m5_q.matches(&msg));

        msg.dynheader.object = Some("/io/test/specific".into());
        assert!(m4.matches(&msg));
        assert!(m4_q.matches(&msg));
        assert!(m5.matches(&msg));
        assert!(m5_q.matches(&msg));

        msg.dynheader.object = Some("/io/test/specific/too".into());
        assert!(m4.matches(&msg));
        assert!(m4_q.matches(&msg));
        assert!(!m5.matches(&msg));
        assert!(!m5_q.matches(&msg));

        msg.dynheader.sender = Some("io.test_sender".into());
        assert!(m6.matches(&msg));
        assert!(m6_q.matches(&msg));
        assert!(EMPTY_MATCH.matches(&msg));
    }
    #[test]
    fn matches_is_empty() {
        assert!(EMPTY_MATCH.is_empty());
        let mut me_q = Match::new();
        me_q.queue = Some(MsgQueue::new());
        assert!(me_q.is_empty());
    }
}
