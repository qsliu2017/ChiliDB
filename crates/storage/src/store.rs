//! Fixed-size, store-local page I/O.
//!
//! Use one store with one buffer pool. Writing directly to a store while its
//! pages are cached in a pool invalidates that cache; stores do not coordinate
//! cache coherence. Likewise, do not open the same file in independent stores
//! concurrently or modify it externally while a store is open.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::{PAGE_SIZE, Page, PageId};

/// Storage for fixed-size pages. Page IDs belong to this store, not to buffer
/// frames. Allocation produces a zero-filled page; reads and writes never
/// allocate pages implicitly.
///
/// Associate a store with only one buffer pool. Direct writes bypass that
/// pool's cache and can leave cached pages stale.
pub trait PageStore: Send + Sync {
    fn allocate_page(&self) -> io::Result<PageId>;
    fn read_page(&self, id: PageId, data: &mut Page) -> io::Result<()>;
    fn write_page(&self, id: PageId, data: &Page) -> io::Result<()>;
    fn sync(&self) -> io::Result<()>;
}

fn lock<T>(mutex: &Mutex<T>) -> io::Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| io::Error::other("page store mutex poisoned"))
}

fn invalid_id(id: PageId) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("unallocated page ID {id}"),
    )
}

/// An in-memory page store. Newly allocated pages are zero-filled.
#[derive(Default)]
pub struct MemoryPageStore {
    pages: Mutex<Vec<Box<Page>>>,
}

impl MemoryPageStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PageStore for MemoryPageStore {
    fn allocate_page(&self) -> io::Result<PageId> {
        let mut pages = lock(&self.pages)?;
        let id = PageId::try_from(pages.len())
            .map_err(|_| io::Error::other("page ID space exhausted"))?;
        pages.push(Box::new([0; PAGE_SIZE]));
        Ok(id)
    }

    fn read_page(&self, id: PageId, data: &mut Page) -> io::Result<()> {
        let pages = lock(&self.pages)?;
        let index = usize::try_from(id).map_err(|_| invalid_id(id))?;
        let page = pages.get(index).ok_or_else(|| invalid_id(id))?;
        data.copy_from_slice(page.as_ref());
        Ok(())
    }

    fn write_page(&self, id: PageId, data: &Page) -> io::Result<()> {
        let mut pages = lock(&self.pages)?;
        let index = usize::try_from(id).map_err(|_| invalid_id(id))?;
        let page = pages.get_mut(index).ok_or_else(|| invalid_id(id))?;
        page.copy_from_slice(data);
        Ok(())
    }

    fn sync(&self) -> io::Result<()> {
        drop(lock(&self.pages)?);
        Ok(())
    }
}

/// A file-backed store using serialized seek/read/write operations.
///
/// The file contains only page bytes, with no header. Existing files must have
/// a whole number of pages and fit the `PageId` address space. File access is
/// serialized within this instance, not across separately opened stores.
pub struct FilePageStore {
    file: Mutex<File>,
}

impl FilePageStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        page_count(&file)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }
}

fn page_count(file: &File) -> io::Result<u64> {
    let length = file.metadata()?.len();
    if length % PAGE_SIZE as u64 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "page file length is not page-aligned",
        ));
    }
    let count = length / PAGE_SIZE as u64;
    // IDs include both zero and u32::MAX.
    if count > u64::from(PageId::MAX) + 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "page file exceeds page ID address space",
        ));
    }
    Ok(count)
}

fn page_offset(file: &File, id: PageId) -> io::Result<u64> {
    if u64::from(id) >= page_count(file)? {
        return Err(invalid_id(id));
    }
    u64::from(id)
        .checked_mul(PAGE_SIZE as u64)
        .ok_or_else(|| io::Error::other("page offset overflow"))
}

impl PageStore for FilePageStore {
    fn allocate_page(&self) -> io::Result<PageId> {
        let file = lock(&self.file)?;
        // Derive the count from the actual file under the lock, including after
        // an earlier I/O error; do not publish an ID until set_len succeeds.
        let count = page_count(&file)?;
        let id =
            PageId::try_from(count).map_err(|_| io::Error::other("page ID space exhausted"))?;
        let length = count
            .checked_add(1)
            .and_then(|count| count.checked_mul(PAGE_SIZE as u64))
            .ok_or_else(|| io::Error::other("page file length overflow"))?;
        // File extension is zero-filled, including when represented sparsely.
        file.set_len(length)?;
        Ok(id)
    }

    fn read_page(&self, id: PageId, data: &mut Page) -> io::Result<()> {
        let mut file = lock(&self.file)?;
        let offset = page_offset(&file, id)?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(data)
    }

    fn write_page(&self, id: PageId, data: &Page) -> io::Result<()> {
        let mut file = lock(&self.file)?;
        let offset = page_offset(&file, id)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)
    }

    fn sync(&self) -> io::Result<()> {
        lock(&self.file)?.sync_all()
    }
}
