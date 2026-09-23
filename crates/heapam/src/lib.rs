#![doc = include_str!("../README.md")]

mod heap;
mod page;

pub use chilidb_common::Ctid;
pub use heap::{HeapScan, HeapTable};
pub use page::MAX_TUPLE_SIZE;

#[derive(Debug, thiserror::Error)]
pub enum HeapError {
    #[error(transparent)]
    Buffer(#[from] chilidb_storage::BufferError),
    #[error("corrupt heap page: {0}")]
    Corrupt(&'static str),
    #[error("tuple size {len} is outside 1..={max}")]
    InvalidTupleSize { len: usize, max: usize },
    #[error("CTID {0:?} does not belong to this heap")]
    InvalidCtid(Ctid),
    #[error("replacement tuple does not fit on its page")]
    NoSpace,
    #[error("heap metadata lock is poisoned")]
    Poisoned,
}

pub type Result<T> = std::result::Result<T, HeapError>;
