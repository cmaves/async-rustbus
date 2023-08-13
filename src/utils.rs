use std::convert::TryInto;
use std::ops::{Add, Rem, Sub};

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

pub fn poll_once<O, F: Future<Output = O> + Unpin>(mut fut: F) -> Either<O, F> {
    let mut ctx = std::task::Context::from_waker(noop_waker_ref());
    match fut.poll_unpin(&mut ctx) {
        Poll::Ready(o) => Either::Left(o),
        Poll::Pending => Either::Right(fut),
    }
}

#[allow(dead_code)]
pub enum EitherFut<O, L: Future<Output = O>, R: Future<Output = O>> {
    Left(L),
    Right(R),
}

impl<O, L: Future<Output = O>, R: Future<Output = O>> Future for EitherFut<O, L, R> {
    type Output = O;
    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        use std::pin::Pin;
        unsafe {
            match self.get_unchecked_mut() {
                EitherFut::Left(fut) => Pin::new_unchecked(fut).poll(cx), /*{
                let p: Pin<&mut L> = Pin::new_unchecked(fut);
                p.poll(cx)
                },*/
                EitherFut::Right(fut) => Pin::new_unchecked(fut).poll(cx),
            }
        }
    }
}
