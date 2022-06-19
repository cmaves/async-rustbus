use std::convert::TryFrom;

use futures::future::{try_join, try_join_all};
use futures::FutureExt;
use std::sync::Arc;
use tokio::sync::Barrier;
use tokio::time::error::Elapsed as TimeoutError;

use async_rustbus::rustbus_core;
use async_rustbus::CallAction;
use async_rustbus::RpcConn;
use rustbus_core::message_builder::{MarshalledMessage, MessageBuilder, MessageType};

use rustbus_core::path::ObjectPathBuf;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum TestingError {
    Io(std::io::Error),
    Bad(MarshalledMessage),
    Timeout(TimeoutError),
    JoinErr(tokio::task::JoinError),
}
impl From<std::io::Error> for TestingError {
    fn from(err: std::io::Error) -> Self {
        TestingError::Io(err)
    }
}
impl From<TimeoutError> for TestingError {
    fn from(err: TimeoutError) -> Self {
        TestingError::Timeout(err)
    }
}
impl From<tokio::task::JoinError> for TestingError {
    fn from(err: tokio::task::JoinError) -> Self {
        TestingError::JoinErr(err)
    }
}

fn is_msg_reply(msg: MarshalledMessage) -> Result<(), TestingError> {
    match msg.typ {
        MessageType::Reply => Ok(()),
        _ => Err(TestingError::Bad(msg)),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn threaded_stress_4mb() -> Result<(), TestingError> {
    let mut vec: Vec<u32> = Vec::with_capacity(4 * 1024 * 1024 / 4 - 1);
    for i in 0..(4 * 1024 * 1024 / 4 - 1) {
        vec.push(i);
    }
    threaded_stress(32, 4, vec![vec]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn threaded_stress_1mb() -> Result<(), TestingError> {
    let mut vec: Vec<u32> = Vec::with_capacity(1024 * 1024 / 4 - 1);
    for i in 0..(1024 * 1024 / 4 - 1) {
        vec.push(i);
    }
    threaded_stress(32, 16, vec![vec]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn threaded_stress_1kb() -> Result<(), TestingError> {
    let mut vec: Vec<u32> = Vec::with_capacity(1024 / 4 - 1);
    for i in 0..(1024 / 4 - 1) {
        vec.push(i);
    }
    threaded_stress(32, 256, vec![vec]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn threaded_stress_32() -> Result<(), TestingError> {
    let mut vec: Vec<u32> = Vec::with_capacity(32 / 4 - 1);
    for i in 0..(32 / 4 - 1) {
        vec.push(i);
    }
    threaded_stress(32, 256, vec![vec]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn threaded_stress_empty() -> Result<(), TestingError> {
    threaded_stress(32, 256, vec![Vec::new()]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn threaded_stress_random() -> Result<(), TestingError> {
    let mut outer = Vec::with_capacity(21);
    let mut vec = Vec::with_capacity(4 * 1024 * 1024 / 4 - 1);
    for i in 0..(4 * 1024 * 1024 / 4 - 1) {
        vec.push(i);
    }
    for i in 0..=20 {
        let end = (1 << i) - 1;
        let slice = &vec[..end];
        outer.push(slice.to_vec());
    }
    threaded_stress(32, 32, outer).await
}
async fn threaded_stress(
    threads: usize,
    msgs: usize,
    bodies: Vec<Vec<u32>>,
) -> Result<(), TestingError> {
    use rand::{rngs::SmallRng, Rng, SeedableRng};
    let (conn, recv_conn) =
        try_join(RpcConn::session_conn(false), RpcConn::session_conn(false)).await?;
    let conn = Arc::new(conn);
    let recv_conn = Arc::new(recv_conn);
    let barrier = Arc::new(Barrier::new(threads * 2));
    let bodies: Arc<[Vec<u32>]> = bodies.into();
    let thread_iter = (0..threads)
        .map(|i| {
            let send_clone = conn.clone();
            let recv_clone = recv_conn.clone();
            let recv_name = recv_conn.get_name().to_string();
            let b1 = barrier.clone();
            let b2 = barrier.clone();
            let bodies = bodies.clone();
            (
                tokio::spawn(async move {
                    let target = ObjectPathBuf::try_from(format!("/test{}", i)).unwrap();
                    recv_clone
                        .insert_call_path(&*target, CallAction::Exact)
                        .await
                        .unwrap();
                    println!("threaded_stress(): recv {}: await barrier", i);
                    b1.wait().await;
                    for _j in 0..msgs {
                        let call = recv_clone.get_call(&*target).await?;
                        // println!("threaded_stress(): recv {}: recvd {}", i, _j);
                        let object = call.dynheader.object.as_deref();
                        assert_eq!(object, Some(target.as_str()));
                        let res = call.dynheader.make_response();
                        assert!(recv_clone.send_msg(&res).await?.is_none());
                    }
                    println!("threaded_stress(): recv {}: finished", i);
                    Result::<(), TestingError>::Ok(())
                }),
                tokio::spawn(async move {
                    let target = format!("/test{}", i);
                    let bad_tar = format!("{}/bad", target);
                    let mut calls = Vec::with_capacity(bodies.len());
                    let mut bad_calls = Vec::with_capacity(bodies.len());
                    for body in bodies.iter() {
                        let mut call = MessageBuilder::new()
                            .call(String::from("Echo"))
                            .with_interface(String::from("org.freedesktop.DBus.Testing"))
                            .at(recv_name.clone())
                            .on(target.clone())
                            .build();
                        let mut bad_call = MessageBuilder::new()
                            .call(String::from("Echo"))
                            .with_interface(String::from("org.freedesktop.DBus.Testing"))
                            .at(recv_name.clone())
                            .on(bad_tar.clone())
                            .build();
                        call.body.push_param(body).unwrap();
                        bad_call.body.push_param(body).unwrap();
                        calls.push(call);
                        bad_calls.push(bad_call);
                    }
                    let mut rng = SmallRng::from_entropy();
                    let send_iter = (0..(msgs * 2)).map(|j| {
                        let idx = rng.gen_range(0..calls.len());
                        if j % 2 == 0 {
                            send_clone.send_msg_w_rsp(&calls[idx])
                        } else {
                            send_clone.send_msg_w_rsp(&bad_calls[idx])
                        }
                    });
                    println!("threaded_stress(): send {}: await barrier", i);
                    b2.wait().await;
                    let recv_futs = try_join_all(send_iter).await?;
                    println!("threaded_stress(): send {}: msgs sent", i);
                    /*
                    for (_j, recv_fut) in recv_futs.into_iter().step_by(2).enumerate() {
                        recv_fut.await?;
                        println!("threaded_stress(): recv {}: recvd reply {}", i, _j);
                    }
                    */
                    let msgs = try_join_all(recv_futs.into_iter().step_by(2)).await?;
                    println!("threaded_stress(): send: replies recvd");
                    for res in msgs.into_iter() {
                        is_msg_reply(res)?;
                    }
                    println!("threaded_stress(): send {}: finished", i);
                    Result::<(), TestingError>::Ok(())
                }),
            )
        })
        .flat_map(|p| [p.0, p.1])
        .map(|j_fut| j_fut.map(|j_res| j_res?));

    try_join_all(thread_iter).await?;
    Ok(())
}
