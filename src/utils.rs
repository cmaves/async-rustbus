use super::rustbus_core;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::iter::FusedIterator;
use std::ops::{Add, Rem, Sub};

use rustbus_core::wire::unmarshal;
use rustbus_core::ByteOrder;

/// Expands a Vec from a slice by the minimum amount needed to
/// reach the `target` length.
/// If the `vec` is already >= `target` in length then nothing is done.
pub fn extend_from_slice_max<T: Copy>(vec: &mut Vec<T>, buf: &[T], target: usize) -> usize {
    if vec.len() >= target {
        return 0;
    }
    let needed = target - buf.len();
    let to_cpy = needed.min(buf.len());
    vec.extend_from_slice(&buf[..to_cpy]);
    to_cpy
}

/// Extends a Vec with a Iterator similiar to Vec::extend but only extends,
/// the Vec to `target` length. If a Vec is already this length then it does nothing.
/// Returns `true` if the Vec is the `target` length after calling.
pub fn extend_max<T: Copy, I: Iterator<Item = T>>(
    vec: &mut Vec<T>,
    iter: &mut I,
    target: usize,
) -> bool {
    if vec.len() >= target {
        return true;
    }
    let needed = target - vec.len();
    vec.extend(iter.by_ref().take(needed));
    return vec.len() == target;
}
pub fn parse_u32(number: &[u8], bo: ByteOrder) -> Result<u32, unmarshal::Error> {
    let mut int_buf = number
        .try_into()
        .map_err(|_| unmarshal::Error::NotEnoughBytes)?;
    Ok(match bo {
        ByteOrder::BigEndian => u32::from_be_bytes(int_buf),
        ByteOrder::LittleEndian => u32::from_le_bytes(int_buf),
    })
}

pub fn align_num<T>(num: T, alignment: T) -> T
where
    T: Rem<T, Output = T> + Sub<T, Output = T> + Add<T, Output = T> + Copy,
{
    (alignment - (num % alignment)) % alignment + num
}

pub struct LazyDrain<'a, T> {
    deque: &'a mut VecDeque<T>,
}
impl<T> Iterator for LazyDrain<'_, T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        self.deque.pop_front()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.deque.len(), Some(self.deque.len()))
    }
}
impl<T> ExactSizeIterator for LazyDrain<'_, T> {}

impl<T> DoubleEndedIterator for LazyDrain<'_, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.deque.pop_back()
    }
}
impl<T> FusedIterator for LazyDrain<'_, T> {}
pub fn lazy_drain<T>(deque: &mut VecDeque<T>) -> LazyDrain<T> {
    LazyDrain { deque }
}
