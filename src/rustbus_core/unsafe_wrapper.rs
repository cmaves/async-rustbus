use std::ops::Deref;

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub struct Unsafe<T>(T);
impl<T> Unsafe<T> {
    pub unsafe fn new(t: T) -> Self {
        Self(t)
    }
    pub unsafe fn as_mut(&mut self) -> &mut T {
        &mut self.0
    }
    pub fn take(self) -> T {
        self.0
    }
}
impl<T> Deref for Unsafe<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
/*
impl<T> From<Unsafe<T>> for T {
    fn from(us: Unsafe<T>) -> Self {
        us.0
    }
}*/

