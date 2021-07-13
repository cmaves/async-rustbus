use std::convert::TryInto;
use std::ops::{Add, Rem, Sub};

use async_std::channel::{RecvError, SendError};
use async_std::sync::{Arc, Condvar, Mutex, Weak};
use futures::future::Either;
use futures::prelude::*;
use futures::task::{noop_waker_ref, Poll};

use super::rustbus_core;
use rustbus_core::ByteOrder;

/*
/// Expands a Vec from a slice by the minimum amount needed to
/// reach the `target` length.
/// If the `vec` is already >= `target` in length then nothing is done.
/// Returns `true` if the Vec is the `target` length after calling.
pub fn extend_from_slice_max<T: Copy>(vec: &mut Vec<T>, buf: &[T], target: usize) -> bool {
    extend_max(vec, &mut buf.iter().copied(), target)
}
*/

pub fn parse_u32(number: &[u8], bo: ByteOrder) -> u32 {
    let int_buf = number.try_into().unwrap();
    match bo {
        ByteOrder::BigEndian => u32::from_be_bytes(int_buf),
        ByteOrder::LittleEndian => u32::from_le_bytes(int_buf),
    }
}

pub fn align_num<T>(num: T, alignment: T) -> T
where
    T: Rem<T, Output = T> + Sub<T, Output = T> + Add<T, Output = T> + Copy,
{
    (alignment - (num % alignment)) % alignment + num
}

#[cfg(test)]
mod tests {
    use super::align_num;
    #[test]
    fn align_num_0_1024() {
        let mut target = 1;
        while target <= 32 {
            assert_eq!(align_num(0, target), 0);
            let aligned = (0..=(1024 / target))
                .flat_map(|i| std::iter::repeat((i + 1) * target).take(target));
            for (gen, tar) in (1..=1024).map(|i| align_num(i, target)).zip(aligned) {
                assert_eq!(gen, tar);
            }
            target += 1;
        }
    }
}
pub struct OneSender<T> {
    inner: Weak<(Mutex<Option<T>>, Condvar)>,
}

impl<T> OneSender<T> {
    pub fn send(self, val: T) -> Result<(), SendError<T>> {
        let arc = match self.inner.upgrade() {
            Some(a) => a,
            None => return Err(SendError(val)),
        };
        let mut backoff = 0;
        loop {
            if let Some(mut lock) = arc.0.try_lock() {
                *lock = Some(val);
                arc.1.notify_all();
                return Ok(());
            }
            if backoff < 8 {
                backoff += 1;
            }
            for _ in 0..(1 << backoff) {
                std::hint::spin_loop();
            }
        }
    }
}
impl<T> Drop for OneSender<T> {
    fn drop(&mut self) {
        if let Some(arc) = self.inner.upgrade() {
            arc.1.notify_all();
        }
    }
}
pub struct OneReceiver<T> {
    inner: Arc<(Mutex<Option<T>>, Condvar)>,
}
/*pub enum TryRecvError<T> {
    Closed,
    WouldBlock(OneReceiver<T>),
    Empty(OneReceiver<T>),
}*/
impl<T> OneReceiver<T> {
    pub async fn recv(self) -> Result<T, RecvError> {
        let mut val = self.inner.0.lock().await;
        while val.is_none() {
            let val_fut = self.inner.1.wait(val);
            if Arc::weak_count(&self.inner) == 0 {
                return val_fut
                    .now_or_never()
                    .ok_or(RecvError)?
                    .take()
                    .ok_or(RecvError);
            }
            val = val_fut.await;
        }
        Ok(val.take().unwrap())
    }
}

pub fn one_time_channel<T>() -> (OneSender<T>, OneReceiver<T>) {
    let inner = Arc::new((Mutex::new(None), Condvar::new()));

    let sender = OneSender {
        inner: Arc::downgrade(&inner),
    };
    let recv = OneReceiver { inner };
    (sender, recv)
}

#[allow(dead_code)]
pub fn prime_future<O, F: Future<Output = O> + Unpin>(mut fut: F) -> Either<O, F> {
    let mut ctx = async_std::task::Context::from_waker(noop_waker_ref());
    match fut.poll_unpin(&mut ctx) {
        Poll::Ready(o) => Either::Left(o),
        Poll::Pending => Either::Right(fut),
    }
}
