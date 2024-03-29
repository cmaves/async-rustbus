use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tokio::net::ToSocketAddrs;

/// The filesystem path for all system DBuses.
pub const DBUS_SYS_PATH: &str = "/run/dbus/system_bus_socket";
/// The environmental variable that contains address of the session DBus.
pub const DBUS_SESS_ENV: &str = "DBUS_SESSION_BUS_ADDRESS";

/// A address for connecting to a DBus dubs.
/// These can be file systems paths to a Unix socket,
/// a TCP address, or an abstract Unix socket.
pub enum DBusAddr<P: AsRef<Path>, S: ToSocketAddrs, B: AsRef<[u8]>> {
    Path(P),
    Tcp(S),
    #[cfg(target_os = "linux")]
    Abstract(B),
}
/// Create a DbusAddr from a filesystem path.
impl<P: AsRef<Path>> DBusAddr<P, &str, [u8; 0]> {
    pub fn unix_path(path: P) -> Self {
        Self::Path(path)
    }
}

/// Create a DbusAddr from a TCP socket address.
impl<S: ToSocketAddrs> DBusAddr<&str, S, [u8; 0]> {
    pub fn tcp_addr(s: S) -> Self {
        Self::Tcp(s)
    }
}

/// Create a DbusAddr from an abstract unix socket address.
#[cfg(target_os = "linux")]
impl<B: AsRef<[u8]>> DBusAddr<&str, &str, B> {
    pub fn unx_abstract(b: B) -> Self {
        Self::Abstract(b)
    }
}

/// Get the path of the system bus if it exists.
pub async fn get_system_bus_addr() -> std::io::Result<DBusAddr<&'static Path, &'static str, [u8; 0]>>
{
    let path = Path::new(DBUS_SYS_PATH);
    if path.exists() {
        Ok(DBusAddr::Path(path))
    } else {
        Err(std::io::Error::new(
            ErrorKind::NotFound,
            "Could not find system bus.",
        ))
    }
}

const BAD_SESSION_ERR_MSG: &str = "Invalid session bus address in environment.";
fn default_session_err() -> std::io::Error {
    std::io::Error::new(ErrorKind::InvalidData, BAD_SESSION_ERR_MSG)
}
/// Get and parse address of the session DBus from the environment.
pub async fn get_session_bus_addr() -> std::io::Result<DBusAddr<PathBuf, String, Vec<u8>>> {
    let bytes = std::env::var_os(DBUS_SESS_ENV)
        .ok_or_else(|| std::io::Error::new(ErrorKind::NotFound, "No DBus session in environment."))?
        .into_vec();
    let mut iter = bytes.split(|b| *b == b':');
    let family = iter.next().unwrap();
    if family.len() == bytes.len() {
        return Err(default_session_err());
    }
    let data = &bytes[family.len() + 1..];
    let data_pairs: HashMap<&[u8], &[u8]> = data
        .split(|b| *b == b',')
        .filter_map(|pair| {
            let mut split = pair.split(|b| *b == b'=');
            let name = split.next().unwrap();
            let data = split.next()?;
            match split.next() {
                Some(_) => None,
                None => Some((name, data)),
            }
        })
        .collect();
    match family {
        b"unix" => {
            #[cfg(target_os = "linux")]
            {
                if let Some(abs) = data_pairs.get(&b"abstract"[..]) {
                    //return Ok(DBusAddr::Abstract(bytes[14..].to_owned()));
                    return Ok(DBusAddr::Abstract((*abs).to_owned()));
                }
            }
            if let Some(path) = data_pairs.get(&b"path"[..]) {
                let path: &Path = OsStr::from_bytes(path).as_ref();
                return if path.exists() {
                    Ok(DBusAddr::Path(path.to_path_buf()))
                } else {
                    Err(std::io::Error::new(
                        ErrorKind::NotFound,
                        format!("Could not find session bus at {:?}.", path),
                    ))
                };
            }
            Err(default_session_err())
        }
        b"tcp" => {
            let addr = || {
                let host_data = data_pairs.get(&b"host"[..])?;
                let mut host_str = std::str::from_utf8(host_data).ok()?.to_string();
                let port_data = data_pairs.get(&b"port"[..])?;
                let port_str = std::str::from_utf8(port_data).ok()?;
                host_str.push(':');
                host_str.push_str(port_str);
                Some(host_str)
            };
            let addr = addr().ok_or_else(default_session_err)?;
            Ok(DBusAddr::Tcp(addr))
        }
        _ => Err(default_session_err()),
    }
}
