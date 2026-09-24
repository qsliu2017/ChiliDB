use std::{
    alloc::Layout,
    cell::UnsafeCell,
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex, MutexGuard, RwLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

use chilidb_common::static_assert;

use crate::{
    BufferError, BufferId, ClockStrategy, PAGE_SIZE, Page, PageId, PageStore, Result,
    VictimStrategy,
};

#[repr(C, align(8192))]
struct AlignedPage(UnsafeCell<Page>);

static_assert!(@usize_eq: size_of::<AlignedPage>(), PAGE_SIZE);

// SAFETY: After construction, these bytes are accessed only while the corresponding
// descriptor's content lock is held. Resident access also owns a pin; replacement
// holds the metadata mutex and requires zero pins. No reference escapes a lock.
unsafe impl Sync for AlignedPage {}

#[repr(align(64))]
struct BufferDesc {
    // The tag is protected by the metadata mutex. Atomic storage
    // permits shared descriptor access without another UnsafeCell or mutex.
    // All u32 page IDs remain valid; u64::MAX denotes an unused descriptor.
    tag: AtomicU64,
    pins: AtomicU32,
    dirty: AtomicBool,
    content_lock: RwLock<()>,
}

static_assert!(size_of::<BufferDesc>().is_multiple_of(64));

impl BufferDesc {
    fn page_id(&self) -> Option<PageId> {
        match self.tag.load(Ordering::Relaxed) {
            u64::MAX => None,
            tag => Some(tag as PageId),
        }
    }
}

struct State {
    pages: HashMap<PageId, usize>,
    strategy: Box<dyn VictimStrategy>,
}

impl State {
    fn victim(&mut self, descriptors: &[BufferDesc]) -> Result<usize> {
        // Assigned frames form a dense prefix and are never unassigned. Failed
        // loads leave the mapping unchanged, so the next unused slot is len().
        if self.pages.len() < descriptors.len() {
            return Ok(self.pages.len());
        }
        // Metadata prevents new pins; concurrent unpins only open eligibility.
        let can_evict = |buffer: BufferId| {
            descriptors
                .get(buffer.get() as usize)
                .is_some_and(|desc| desc.pins.load(Ordering::Acquire) == 0)
        };
        let buffer = self
            .strategy
            .victim(&can_evict)
            .ok_or(BufferError::AllPinned)?;
        // Never trust a dynamic policy before indexing, unsafe access, or I/O.
        if !can_evict(buffer) {
            return Err(BufferError::InvalidVictim(buffer));
        }
        Ok(buffer.get() as usize)
    }

    fn install(&mut self, index: usize, page: PageId, desc: &BufferDesc) {
        if let Some(old) = desc.page_id() {
            self.pages.remove(&old);
        } else {
            debug_assert_eq!(index, self.pages.len());
        }
        desc.tag.store(u64::from(page), Ordering::Relaxed);
        self.pages.insert(page, index);
    }
}

/// Fixed-capacity, page-aligned storage with configurable replacement.
/// The default policy is second-chance (CLOCK).
/// Pins prevent eviction; content latches are held only during callbacks.
/// Dirty pages are written on eviction or explicit flush, never on pool drop.
pub struct BufferPool {
    store: Arc<dyn PageStore>,
    buffer_array: Box<[AlignedPage]>,
    buffer_desc_array: Box<[BufferDesc]>,
    state: Mutex<State>,
}

pub struct GlobalBufferPool;
pub struct LocalBufferPool;

static_assert!(@usize_eq: size_of::<PinnedBuffer<GlobalBufferPool>>(), 8);
static_assert!(@usize_eq: size_of::<PinnedBuffer<LocalBufferPool>>(), 8);

impl AsRef<BufferPool> for GlobalBufferPool {
    fn as_ref(&self) -> &BufferPool {
        todo!()
    }
}

impl AsRef<BufferPool> for LocalBufferPool {
    fn as_ref(&self) -> &BufferPool {
        todo!()
    }
}

impl GlobalBufferPool {
    pub fn pin(&self, page: PageId) -> Result<PinnedBuffer<Self>> {
        self.as_ref().pin(page).map(|pinned| {
            let PinnedBuffer { index, page, .. } = pinned;
            std::mem::forget(pinned);
            PinnedBuffer {
                pool: Self,
                index,
                page,
            }
        })
    }
}

impl fmt::Debug for BufferPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BufferPool")
            .field("capacity", &self.capacity())
            .finish_non_exhaustive()
    }
}

impl BufferPool {
    pub fn new(capacity: usize, store: Arc<dyn PageStore>) -> Result<Self> {
        Self::validate_capacity(capacity)?;
        Self::with_strategy(capacity, store, Box::new(ClockStrategy::new(capacity)))
    }

    fn validate_capacity(capacity: usize) -> Result<()> {
        if capacity == 0
            || u32::try_from(capacity - 1).is_err()
            || Layout::array::<AlignedPage>(capacity).is_err()
            || Layout::array::<BufferDesc>(capacity).is_err()
        {
            return Err(BufferError::InvalidCapacity);
        }
        Ok(())
    }

    /// Construct a pool with an independent replacement policy.
    /// Policy callbacks hold metadata and must not reenter this pool.
    pub fn with_strategy(
        capacity: usize,
        store: Arc<dyn PageStore>,
        strategy: Box<dyn VictimStrategy>,
    ) -> Result<Self> {
        Self::validate_capacity(capacity)?;
        Ok(Self {
            store,
            buffer_array: (0..capacity)
                .map(|_| AlignedPage(UnsafeCell::new([0; PAGE_SIZE])))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            buffer_desc_array: (0..capacity)
                .map(|_| BufferDesc {
                    tag: AtomicU64::new(u64::MAX),
                    pins: AtomicU32::new(0),
                    dirty: AtomicBool::new(false),
                    content_lock: RwLock::new(()),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            state: Mutex::new(State {
                pages: HashMap::new(),
                strategy,
            }),
        })
    }

    pub fn capacity(&self) -> usize {
        self.buffer_array.len()
    }

    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| BufferError::Poisoned)
    }

    fn check(&self, index: usize) -> Result<()> {
        if self.state.is_poisoned() || self.buffer_desc_array[index].content_lock.is_poisoned() {
            Err(BufferError::Poisoned)
        } else {
            Ok(())
        }
    }

    #[inline]
    fn resident_pin<const RECORD_ACCESS: bool>(
        &self,
        state: &mut State,
        page: PageId,
    ) -> Result<Option<PinnedBuffer<&Self>>> {
        let Some(&index) = state.pages.get(&page) else {
            return Ok(None);
        };
        // A resident pin touches no bytes; poisoned content is rejected when
        // a callback or flush attempts to acquire its latch.
        self.buffer_desc_array[index]
            .pins
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pins| {
                pins.checked_add(1)
            })
            .map_err(|_| BufferError::PinOverflow)?;
        let pin = PinnedBuffer {
            pool: self,
            index: BufferId::new(index as u32),
            page,
        };
        if RECORD_ACCESS {
            state.strategy.access(pin.buffer_id());
        }
        Ok(Some(pin))
    }

    /// Pin a stored page without retaining a content latch.
    pub fn pin(&self, page: PageId) -> Result<PinnedBuffer<&Self>> {
        let mut state = self.state()?;
        if let Some(pin) = self.resident_pin::<true>(&mut state, page)? {
            return Ok(pin);
        }
        let index = state.victim(&self.buffer_desc_array)?;
        self.check(index)?;
        let mut incoming = [0; PAGE_SIZE];
        self.store.read_page(page, &mut incoming)?;
        let desc = &self.buffer_desc_array[index];
        self.with_victim(&mut state, index, |state, data| {
            self.flush_victim(desc, data)?;
            // Publish only after all fallible I/O succeeds.
            *data = incoming;
            state.install(index, page, desc);
            desc.dirty.store(false, Ordering::Release);
            desc.pins.store(1, Ordering::Release);
            Ok(())
        })?;
        Ok(self.loaded_pin(&mut state, index, page))
    }

    fn loaded_pin(&self, state: &mut State, index: usize, page: PageId) -> PinnedBuffer<&Self> {
        // Own the pin before invoking policy code so unwind cannot leak it.
        let pin = PinnedBuffer {
            pool: self,
            index: BufferId::new(index as u32),
            page,
        };
        state.strategy.load(pin.buffer_id());
        pin
    }

    fn with_victim<T>(
        &self,
        state: &mut MutexGuard<'_, State>,
        index: usize,
        replace: impl FnOnce(&mut State, &mut Page) -> Result<T>,
    ) -> Result<T> {
        let desc = &self.buffer_desc_array[index];
        // Metadata excludes new pins. Zero pins means no latch holder or waiter;
        // hence taking the content lock while holding metadata cannot deadlock.
        assert_eq!(desc.pins.load(Ordering::Acquire), 0);
        let _lock = desc
            .content_lock
            .write()
            .map_err(|_| BufferError::Poisoned)?;
        self.check(index)?;
        // SAFETY: zero pins under metadata lock and exclusive content lock.
        // The callback cannot let a page reference escape this lock's scope.
        replace(state, unsafe { &mut *self.buffer_array[index].0.get() })
    }

    fn flush_victim(&self, desc: &BufferDesc, data: &Page) -> Result<()> {
        if desc.dirty.load(Ordering::Acquire)
            && let Some(page) = desc.page_id()
        {
            self.store.write_page(page, data)?;
        }
        Ok(())
    }

    /// Allocate a zeroed, clean page and pin it without a content latch.
    /// AllPinned is checked before allocation, so no unreachable page is created.
    pub fn new_page(&self) -> Result<PinnedBuffer<&Self>> {
        let mut state = self.state()?;
        let index = state.victim(&self.buffer_desc_array)?;
        self.check(index)?;
        let desc = &self.buffer_desc_array[index];
        let page = self.with_victim(&mut state, index, |state, data| {
            self.flush_victim(desc, data)?;
            let page = self.store.allocate_page()?;
            data.fill(0);
            state.install(index, page, desc);
            desc.dirty.store(false, Ordering::Release);
            desc.pins.store(1, Ordering::Release);
            Ok(page)
        })?;
        Ok(self.loaded_pin(&mut state, index, page))
    }

    /// Write a resident dirty page. Returns false if absent; does not fsync.
    pub fn flush_page(&self, page: PageId) -> Result<bool> {
        let pin = {
            let mut state = self.state()?;
            let Some(pin) = self.resident_pin::<false>(&mut state, page)? else {
                return Ok(false);
            };
            pin
        };
        pin.read(|data| {
            let desc = &self.buffer_desc_array[pin.index.get() as usize];
            if desc.dirty.load(Ordering::Acquire) {
                self.store.write_page(page, data)?;
                // Retain the read latch through writeback and marking clean.
                desc.dirty.store(false, Ordering::Release);
            }
            Ok::<_, BufferError>(())
        })??;
        Ok(true)
    }

    /// Flush a resident-page snapshot, then sync. Quiesce writers for durable close.
    pub fn flush_all(&self) -> Result<()> {
        let mut pages: Vec<_> = self.state()?.pages.keys().copied().collect();
        pages.sort_unstable();
        for page in pages {
            self.flush_page(page)?;
        }
        self.store.sync()?;
        Ok(())
    }
}

/// An eviction pin retaining a pool handle, independent of the content latch.
/// Pool methods return `PinnedBuffer<&BufferPool>`, borrowing the owning pool:
/// ```compile_fail
/// use chilidb_storage::{BufferPool, MemoryPageStore};
/// use std::sync::Arc;
/// let pin = {
///     let pool = BufferPool::new(1, Arc::new(MemoryPageStore::new())).unwrap();
///     pool.new_page().unwrap()
/// };
/// pin.read(|page| page[0]).unwrap();
/// ```
/// Callbacks cannot return references into a page:
/// ```compile_fail
/// use chilidb_storage::{BufferPool, PinnedBuffer};
/// fn escape<'a, P: AsRef<BufferPool>>(pin: &'a PinnedBuffer<P>) -> &'a chilidb_storage::Page {
///     pin.read(|page| page).unwrap()
/// }
/// ```
/// ```compile_fail
/// use chilidb_storage::{BufferPool, PinnedBuffer};
/// fn escape<'a, P: AsRef<BufferPool>>(pin: &'a PinnedBuffer<P>) -> &'a mut chilidb_storage::Page {
///     pin.write(|page| page).unwrap()
/// }
/// ```
pub struct PinnedBuffer<P: AsRef<BufferPool>> {
    // Constructors retain a handle that resolves to the same pool for the pin's lifetime.
    pool: P,
    index: BufferId,
    page: PageId,
}

impl AsRef<BufferPool> for BufferPool {
    fn as_ref(&self) -> &BufferPool {
        self
    }
}

impl<P: AsRef<BufferPool>> PinnedBuffer<P> {
    pub fn page_id(&self) -> PageId {
        self.page
    }
    pub fn buffer_id(&self) -> BufferId {
        self.index
    }

    pub fn read<T>(&self, read: impl FnOnce(&Page) -> T) -> Result<T> {
        let pool = self.pool.as_ref();
        let desc = &pool.buffer_desc_array[self.index.get() as usize];
        let _lock = desc
            .content_lock
            .read()
            .map_err(|_| BufferError::Poisoned)?;
        pool.check(self.index.get() as usize)?;
        // SAFETY: this pin prevents replacement, the read lock excludes mutation,
        // and the callback's independent return lifetime prevents reference escape.
        Ok(read(unsafe {
            &*pool.buffer_array[self.index.get() as usize].0.get()
        }))
    }

    /// Marks dirty before invoking the callback, even if it does not modify bytes.
    /// Avoid recursively latching this page from a callback (it may deadlock).
    pub fn write<T>(&self, write: impl FnOnce(&mut Page) -> T) -> Result<T> {
        let pool = self.pool.as_ref();
        let desc = &pool.buffer_desc_array[self.index.get() as usize];
        let _lock = desc
            .content_lock
            .write()
            .map_err(|_| BufferError::Poisoned)?;
        pool.check(self.index.get() as usize)?;
        desc.dirty.store(true, Ordering::Release);
        // SAFETY: this pin prevents replacement and the write lock excludes all
        // other byte access. The callback cannot return a borrowed page reference.
        Ok(write(unsafe {
            &mut *pool.buffer_array[self.index.get() as usize].0.get()
        }))
    }
}

impl<P: AsRef<BufferPool>> Drop for PinnedBuffer<P> {
    fn drop(&mut self) {
        // No metadata lock: safe during unwind and while another operation holds
        // metadata. All this pin's callbacks/latches have finished before Drop.
        let old = self.pool.as_ref().buffer_desc_array[self.index.get() as usize]
            .pins
            .fetch_sub(1, Ordering::AcqRel);
        debug_assert!(old > 0);
    }
}

impl<P: AsRef<BufferPool>> fmt::Debug for PinnedBuffer<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedBuffer")
            .field("page_id", &self.page_id())
            .field("buffer_id", &self.buffer_id())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryPageStore;

    #[test]
    fn capacity_and_pin_overflow_are_rejected() {
        for capacity in [0, usize::MAX] {
            assert!(matches!(
                BufferPool::new(capacity, Arc::new(MemoryPageStore::new())),
                Err(BufferError::InvalidCapacity)
            ));
        }
        let pool = BufferPool::new(1, Arc::new(MemoryPageStore::new())).unwrap();
        let pin = pool.new_page().unwrap();
        let desc = &pool.buffer_desc_array[pin.index.get() as usize];
        desc.pins.store(u32::MAX, Ordering::Release);
        assert!(matches!(
            pool.pin(pin.page_id()),
            Err(BufferError::PinOverflow)
        ));
        assert_eq!(desc.pins.load(Ordering::Acquire), u32::MAX);
        desc.pins.store(1, Ordering::Release);
    }

    #[test]
    fn arc_backed_pin_access_and_drop() {
        let pool = Arc::new(BufferPool::new(1, Arc::new(MemoryPageStore::new())).unwrap());
        let borrowed = pool.new_page().unwrap();
        let pin = PinnedBuffer {
            pool: Arc::clone(&pool),
            index: borrowed.index,
            page: borrowed.page,
        };
        // Transfer the existing pin-count increment to the owned handle.
        std::mem::forget(borrowed);
        assert_eq!(Arc::strong_count(&pool), 2);
        pin.write(|page| page[0] = 42).unwrap();
        assert_eq!(pin.read(|page| page[0]).unwrap(), 42);
        assert!(format!("{pin:?}").contains("PinnedBuffer"));
        assert!(matches!(pool.new_page(), Err(BufferError::AllPinned)));
        drop(pin);
        assert_eq!(Arc::strong_count(&pool), 1);
        pool.new_page().unwrap();
    }

    struct FixedVictim(BufferId);

    impl VictimStrategy for FixedVictim {
        fn access(&mut self, _: BufferId) {}

        fn victim(&mut self, _: &dyn Fn(BufferId) -> bool) -> Option<BufferId> {
            Some(self.0)
        }
    }

    #[test]
    fn policy_cannot_select_out_of_range_or_pinned_frames() {
        for victim in [BufferId::new(0), BufferId::new(u32::MAX)] {
            let pool = BufferPool::with_strategy(
                1,
                Arc::new(MemoryPageStore::new()),
                Box::new(FixedVictim(victim)),
            )
            .unwrap();
            // A free frame bypasses even an invalid policy choice.
            let pin = pool.new_page().unwrap();
            assert!(matches!(
                pool.new_page(),
                Err(BufferError::InvalidVictim(id)) if id == victim
            ));
            assert!(matches!(
                pool.pin(99),
                Err(BufferError::InvalidVictim(id)) if id == victim
            ));
            assert_eq!(pin.read(|page| page[0]).unwrap(), 0);
        }
    }

    #[test]
    fn array_layout_and_thread_traits() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<BufferPool>();
        send_sync::<PinnedBuffer<&BufferPool>>();
        send_sync::<PinnedBuffer<Arc<BufferPool>>>();
        let pool = BufferPool::new(4, Arc::new(MemoryPageStore::new())).unwrap();
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<PinnedBuffer<&BufferPool>>(), 16);
        assert_eq!(size_of::<AlignedPage>(), PAGE_SIZE);
        let base = pool.buffer_array.as_ptr() as usize;
        let desc_base = pool.buffer_desc_array.as_ptr() as usize;
        assert_eq!(base % PAGE_SIZE, 0);
        assert_eq!(desc_base % 64, 0);
        for index in 0..pool.capacity() {
            let address = &pool.buffer_array[index] as *const _ as usize;
            assert_eq!(address, base + index * PAGE_SIZE);
            assert_eq!(address % PAGE_SIZE, 0);
            let address = &pool.buffer_desc_array[index] as *const _ as usize;
            assert_eq!(address, desc_base + index * size_of::<BufferDesc>());
            assert_eq!(address % 64, 0);
        }
    }
}
