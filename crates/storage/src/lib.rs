#![doc = include_str!("../README.md")]

mod buffer;
mod replacement;
mod store;

pub use buffer::{BufferError, BufferPool, PinnedBuffer};
pub use replacement::{BufferId, ClockStrategy, VictimStrategy};
pub use store::{FilePageStore, MemoryPageStore, PageStore};

pub const PAGE_SIZE: usize = 8192;
pub type Page = [u8; PAGE_SIZE];
/// A stored-page address within one PageStore, never a buffer-frame index.
pub type PageId = u32;
