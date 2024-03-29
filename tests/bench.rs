use std::convert::TryFrom;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::{try_join, try_join_all};
use futures::FutureExt;
use tokio::sync::Barrier;
use tokio::time::{error::Elapsed as TimeoutError, timeout};

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
async fn bench_4mb() -> Result<(), TestingError> {
    let mut vec: Vec<u32> = Vec::with_capacity(4 * 1024 * 1024 / 4 - 1);
    for i in 0..(4 * 1024 * 1024 / 4 - 1) {
        vec.push(i);
    }
    threaded_bench(32, 16, vec![vec]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn bench_1mb() -> Result<(), TestingError> {
    let mut vec: Vec<u32> = Vec::with_capacity(1024 * 1024 / 4 - 1);
    for i in 0..(1024 * 1024 / 4 - 1) {
        vec.push(i);
    }
    threaded_bench(32, 64, vec![vec]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn bench_1kb() -> Result<(), TestingError> {
    let mut vec: Vec<u32> = Vec::with_capacity(1024 / 4 - 1);
    for i in 0..(1024 / 4 - 1) {
        vec.push(i);
    }
    threaded_bench(32, 2048, vec![vec]).await
}
#[tokio::test(flavor = "multi_thread")]
async fn bench_32() -> Result<(), TestingError> {
    let mut vec: Vec<u32> = Vec::with_capacity(32 / 4 - 1);
    for i in 0..(32 / 4 - 1) {
        vec.push(i);
    }
    threaded_bench(32, 2048, vec![vec]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn bench_empty() -> Result<(), TestingError> {
    threaded_bench(32, 2048, vec![Vec::new()]).await
}

#[tokio::test(flavor = "multi_thread")]
async fn bench_random() -> Result<(), TestingError> {
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
    threaded_bench(32, 128, outer).await
}
async fn threaded_bench(
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
    let recv_cntr = Arc::new(AtomicU32::new(0));
    let thread_iter = (0..threads)
        .map(|i| {
            let send_clone = conn.clone();
            let recv_clone = recv_conn.clone();
            let recv_name = recv_conn.get_name().to_string();
            let b1 = barrier.clone();
            let b2 = barrier.clone();
            let bodies = bodies.clone();
            let rc_clone = recv_cntr.clone();
            (
                tokio::spawn(async move {
                    let target = ObjectPathBuf::try_from(format!("/test{}", i)).unwrap();
                    recv_clone
                        .insert_call_path(&*target, CallAction::Exact)
                        .await
                        .unwrap();
                    println!("threaded_bench(): recv {}: await barrier", i);
                    b1.wait().await;
                    let to = Duration::from_secs(20);
                    for _j in 0..msgs {
                        //let call = recv_clone.get_call(&*target).await?;
                        let c_fut = recv_clone.get_call(&*target);
                        let call = timeout(to, c_fut).await.unwrap().unwrap();
                        let object = call.dynheader.object.as_deref();
                        assert_eq!(object, Some(target.as_str()));
                        let res = call.dynheader.make_response();
                        assert!(recv_clone.send_msg(&res).await?.is_none());
                    }
                    let msgs = msgs as u32;
                    let recvd = rc_clone.fetch_add(msgs, Ordering::Relaxed) + msgs;
                    println!("threaded_bench(): recv {}: finished ({})", i, recvd);
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
                    println!("threaded_bench(): send {}: await barrier", i);
                    b2.wait().await;
                    let to0 = Duration::from_secs(5);
                    let to1 = Duration::from_secs(15);
                    for j in 0..(msgs * 2 - 1) {
                        let idx = rng.gen_range(0..calls.len());
                        let send_fut = if j % 2 == 0 {
                            send_clone.send_msg_w_rsp(&calls[idx])
                        } else {
                            send_clone.send_msg_w_rsp(&bad_calls[idx])
                        };
                        //let res = send_fut.await?.await?;
                        let res = timeout(to0, send_fut).await.unwrap().unwrap();
                        let res = timeout(to1, res).await.unwrap().unwrap();
                        if j % 2 == 0 {
                            is_msg_reply(res)?;
                        }
                    }
                    println!("threaded_bench(): send {}: finsihed", i);
                    Result::<(), TestingError>::Ok(())
                }),
            )
        })
        .flat_map(|p| [p.0, p.1])
        .map(|j_fut| j_fut.map(|j_res| j_res?));

    try_join_all(thread_iter).await?;
    Ok(())
}

use rustbus_core::dbus_variant_var;
use std::collections::HashMap;

#[test]
fn marshalling() {
    dbus_variant_var!(TestVar, Str => &'buf str; U32 => u32; Buf => &'buf [u8]);
    let mut map = HashMap::new();
    map.insert("A", 123456i32);
    map.insert("B", 111111i32);
    map.insert("C", 101010i32);
    map.insert("D", 000000i32);
    map.insert("E", 654321i32);
    let array: Vec<_> = (0..4).map(|i| format!("{}{}{}{}", i, i, i, i)).collect();
    let var_array = [
        TestVar::Str("Hello World!"),
        TestVar::U32(1234),
        TestVar::Buf(&[4, 5, 6, 7]),
    ];
    let mut msg = MessageBuilder::new()
        .signal("com.example", "ExampleSig", "/com/example")
        .build();
    let mut buf = Vec::new();
    for i in 0u64..(1 << 21) {
        msg.body.reset();
        msg.body
            .push_param3("TestTestTestTest", i, (i, "InnerTestInnerTest"))
            .unwrap();
        msg.body.push_param3(&map, &array, &var_array[..]).unwrap();
        assert_eq!(msg.body.buf().len(), 244);
        assert_eq!(msg.body.sig(), "st(ts)a{si}asav");
        let serial = std::num::NonZeroU32::new(1).unwrap();
        msg.marshal_header(serial, &mut buf).unwrap();
    }
}
