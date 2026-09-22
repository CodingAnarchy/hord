use std::fmt;

mod inner {
    pub const N: u8 = 1;
}

pub const C: i32 = 1;
pub static S: &str = "s";
pub type Alias = u32;
pub union Bits {
    i: i32,
    u: u32,
}

pub struct Named {
    pub field: i32,
}

pub enum Kind {
    A,
    B { n: i32 },
}

pub trait T {
    fn method(&self);
    type Item;
}

pub fn free() {}

impl T for Named {
    fn method(&self) {}
    type Item = i32;
}

macro_rules! m {
    () => {};
}
