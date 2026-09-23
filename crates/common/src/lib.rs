#![doc = include_str!("../README.md")]

pub mod ctid;

pub use ctid::Ctid;

#[macro_export]
macro_rules! static_assert {
    ($condition:expr) => {
        const _: () = assert!($condition);
    };
    (@usize_eq: $lhs:expr, $rhs:expr) => {
        const _: [(); $lhs] = [(); $rhs];
    };
}
