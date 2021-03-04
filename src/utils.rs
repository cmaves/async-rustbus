use super::rustbus_core;
use std::collections::VecDeque;
use std::convert::TryInto;
use std::iter::FusedIterator;
use std::ops::{Add, Rem, Sub};

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

#[cfg(test)]
mod tests {
    use super::{align_num, lazy_drain};
    use std::collections::VecDeque;
    #[test]
    fn lazy_drain_all() {
        let mut d: VecDeque<u8> = (0..32).collect();
        let drain = lazy_drain(&mut d);
        let new: Vec<u8> = drain.collect();
        assert_eq!(d.len(), 0);
        assert!(new.into_iter().eq(0..32))
    }
    #[test]
    fn lazy_drain_partial() {
        let mut d: VecDeque<u8> = (0..32).collect();
        let drain = lazy_drain(&mut d);
        let new: Vec<u8> = drain.take(16).collect();
        assert!(d.into_iter().eq(16..32));
        assert!(new.into_iter().eq(0..16));
    }
    fn take_four<I: Iterator<Item = u8>>(mut i: I) {
        for _ in 0..4 {
            i.next();
        }
    }
    #[test]
    fn lazy_drain_by_ref() {
        let mut d: VecDeque<u8> = (0..32).collect();
        let mut drain = lazy_drain(&mut d);
        take_four(drain.by_ref());
        let new: Vec<u8> = drain.by_ref().take(4).collect();
        let new2: Vec<u8> = drain.collect();
        assert_eq!(d.len(), 0);
        assert!(new.into_iter().eq(4..8));
        assert!(new2.into_iter().eq(8..32));
    }
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
/*
pub(crate) struct CallOnDrop<F: FnOnce()>(pub Option<F>);
impl <F: FnOnce()> CallOnDrop<F> {
    pub fn new(f: F) -> Self {
        CallOnDrop(Some(f))
    }
}

impl<F: FnOnce()> Drop for CallOnDrop<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f()
        }
    }
}
*/
