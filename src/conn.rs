

use super::sync_conn;

use async_std::path::{Path, PathBuf};

const DBUS_SYS_PATH: &'static str = "/run/dbus/system_bus_socket";
const DBUS_SESS_ENV: &'static str = "DBUS_SESSION_BUS_ADDRESS";
pub async fn get_system_bus_path() -> Result<&'static Path, sync_conn::Error> {
    let path = Path::new(DBUS_SYS_PATH);
    if path.exists().await {
        Ok(path)
    } else {
        Err(sync_conn::Error::PathDoesNotExist(DBUS_SYS_PATH.to_string()))
    }
}

pub async fn get_session_bus_path() -> Result<PathBuf, sync_conn::Error> {
    let path: PathBuf = std::env::var_os(DBUS_SESS_ENV).ok_or(sync_conn::Error::NoAddressFound)?.into();
    if path.exists().await {
        Ok(path)
    } else {
        Err(sync_conn::Error::PathDoesNotExist(path.to_string_lossy().to_string()))
    }
}
