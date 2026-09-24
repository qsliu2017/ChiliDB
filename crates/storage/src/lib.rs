#![doc = include_str!("../README.md")]

mod buffer;
#[cfg(test)]
mod models;
mod replacement;
mod store;

pub use buffer::{BufferPool, PinnedBuffer};
pub use replacement::{ClockStrategy, VictimStrategy};
pub use store::{FilePageStore, MemoryPageStore, PageStore};

pub const PAGE_SIZE: usize = 8192;
pub type Page = [u8; PAGE_SIZE];
/// A stored-page address within one PageStore, never a buffer-frame index.
pub type PageId = u32;

/// A pool-local frame index, not a stored-page address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferId(u32);

impl BufferId {
    pub const fn new(index: u32) -> Self {
        Self(index)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BufferError {
    #[error(
        "buffer capacity must be nonzero and fit the frame address space and allocation layout"
    )]
    InvalidCapacity,
    #[error("all buffer frames are pinned")]
    AllPinned,
    #[error("replacement policy selected an invalid or pinned victim: {0:?}")]
    InvalidVictim(BufferId),
    #[error("buffer lock is poisoned")]
    Poisoned,
    #[error("page pin count overflow")]
    PinOverflow,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

type Result<T> = std::result::Result<T, BufferError>;
