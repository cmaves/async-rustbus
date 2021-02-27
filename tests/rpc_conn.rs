use async_rustbus::RpcConn;
use async_rustbus::conn::DBusAddr;

#[async_std::test]
async fn session_wo_fd() -> std::io::Result<()> {
    RpcConn::session_conn(false).await?;
    Ok(())
}

#[async_std::test]
async fn session_w_fd() -> std::io::Result<()> {
    RpcConn::session_conn(true).await?;
    Ok(())
}

#[async_std::test]
async fn system_wo_fd() -> std::io::Result<()> {
    RpcConn::system_conn(false).await?;
    Ok(())
}

#[async_std::test]
async fn system_w_fd() -> std::io::Result<()> {
    RpcConn::system_conn(true).await?;
    Ok(())
}

#[async_std::test]
async fn tcp_wo_fd() -> std::io::Result<()> {
    let addr: DBusAddr<&str, &str> = DBusAddr::Tcp("localhost:29011");
    RpcConn::connect_to_addr(&addr, false).await?;
    Ok(())
}

#[async_std::test]
async fn tcp_w_fd() -> std::io::Result<()> {
    let addr: DBusAddr<&str, &str> = DBusAddr::Tcp("localhost:29011");
    assert!(RpcConn::connect_to_addr(&addr, true).await.is_err());
    Ok(())
}
