use std::{
    io,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    thread,
};

use chilidb_storage::{
    BufferError, BufferPool, FilePageStore, MemoryPageStore, PAGE_SIZE, Page, PageId, PageStore,
};

#[derive(Default)]
struct MockStore {
    inner: MemoryPageStore,
    reads: AtomicUsize,
    writes: AtomicUsize,
    allocations: AtomicUsize,
    syncs: AtomicUsize,
    fail_read: AtomicBool,
    fail_write: AtomicBool,
    fail_allocate: AtomicBool,
    fail_sync: AtomicBool,
}

fn maybe_fail(flag: &AtomicBool) -> io::Result<()> {
    if flag.swap(false, SeqCst) {
        Err(io::Error::other("injected failure"))
    } else {
        Ok(())
    }
}

impl PageStore for MockStore {
    fn allocate_page(&self) -> io::Result<PageId> {
        self.allocations.fetch_add(1, SeqCst);
        maybe_fail(&self.fail_allocate)?;
        self.inner.allocate_page()
    }
    fn read_page(&self, id: PageId, data: &mut Page) -> io::Result<()> {
        self.reads.fetch_add(1, SeqCst);
        // A failed read may have touched its output buffer.
        if self.fail_read.swap(false, SeqCst) {
            data.fill(0xee);
            return Err(io::Error::other("injected read failure"));
        }
        self.inner.read_page(id, data)
    }
    fn write_page(&self, id: PageId, data: &Page) -> io::Result<()> {
        self.writes.fetch_add(1, SeqCst);
        maybe_fail(&self.fail_write)?;
        self.inner.write_page(id, data)
    }
    fn sync(&self) -> io::Result<()> {
        self.syncs.fetch_add(1, SeqCst);
        maybe_fail(&self.fail_sync)?;
        self.inner.sync()
    }
}

fn setup(capacity: usize) -> (Arc<MockStore>, BufferPool) {
    let store = Arc::new(MockStore::default());
    let pool = BufferPool::new(capacity, store.clone()).unwrap();
    (store, pool)
}

#[test]
fn unused_frames_are_preferred_even_after_failed_loads() {
    let (store, pool) = setup(3);
    let a = store.allocate_page().unwrap();
    let b = store.allocate_page().unwrap();
    let c = store.allocate_page().unwrap();
    assert!(pool.pin(100).is_err());
    pool.pin(a).unwrap().write(|page| page[0] = 7).unwrap();
    pool.pin(b).unwrap().write(|page| page[0] = 8).unwrap();
    for _ in 0..3 {
        assert!(pool.pin(100).is_err());
    }
    drop(pool.pin(c).unwrap());
    assert_eq!(store.writes.load(SeqCst), 0);
    assert_eq!(pool.pin(a).unwrap().read(|page| page[0]).unwrap(), 7);
    assert_eq!(pool.pin(b).unwrap().read(|page| page[0]).unwrap(), 8);
}

#[test]
fn clock_gives_referenced_pages_a_second_chance() {
    let (store, pool) = setup(3);
    let ids: Vec<_> = (0..5).map(|_| store.allocate_page().unwrap()).collect();
    for &id in &ids[..3] {
        drop(pool.pin(id).unwrap());
    }
    drop(pool.pin(ids[3]).unwrap());
    drop(pool.pin(ids[1]).unwrap());
    drop(pool.pin(ids[4]).unwrap());
    assert_eq!(store.reads.load(SeqCst), 5);
    drop(pool.pin(ids[1]).unwrap());
    assert_eq!(store.reads.load(SeqCst), 5);
    drop(pool.pin(ids[2]).unwrap());
    assert_eq!(store.reads.load(SeqCst), 6);
}

#[test]
fn panic_in_writer_releases_pin_but_poisoned_bytes_are_not_exposed() {
    let (store, pool) = setup(1);
    let a = store.allocate_page().unwrap();
    let b = store.allocate_page().unwrap();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let pin = pool.pin(a).unwrap();
        pin.write(|page| {
            page[0] = 9;
            panic!("simulated writer panic");
        })
        .unwrap();
    }));
    assert!(outcome.is_err());
    assert!(matches!(
        pool.pin(a).unwrap().read(|page| page[0]),
        Err(BufferError::Poisoned)
    ));
    // A leaked pin would return AllPinned instead of reaching the poisoned latch.
    assert!(matches!(pool.pin(b), Err(BufferError::Poisoned)));
    assert!(matches!(pool.flush_all(), Err(BufferError::Poisoned)));
}

#[test]
fn zero_capacity_rejected() {
    assert!(matches!(
        BufferPool::new(0, Arc::new(MockStore::default())),
        Err(BufferError::InvalidCapacity)
    ));
}

#[test]
fn cached_pins_hold_residency_until_last_pin_drops() {
    let (store, pool) = setup(1);
    let a = store.allocate_page().unwrap();
    let b = store.allocate_page().unwrap();
    let first = pool.pin(a).unwrap();
    let second = pool.pin(a).unwrap();
    assert_eq!(first.page_id(), a);
    assert_eq!(second.page_id(), a);
    assert_eq!(store.reads.load(SeqCst), 1);
    assert!(matches!(pool.pin(b), Err(BufferError::AllPinned)));
    drop(first);
    assert!(matches!(pool.pin(b), Err(BufferError::AllPinned)));
    drop(second);
    assert_eq!(pool.pin(b).unwrap().page_id(), b);
    assert_eq!(store.reads.load(SeqCst), 2);
}

#[test]
fn new_page_is_zero_and_checks_pins_before_allocating() {
    let (store, pool) = setup(1);
    let page = pool.new_page().unwrap();
    assert_eq!(page.read(|bytes| *bytes).unwrap(), [0; PAGE_SIZE]);
    page.write(|bytes| bytes.fill(7)).unwrap();
    assert!(matches!(pool.new_page(), Err(BufferError::AllPinned)));
    assert_eq!(store.allocations.load(SeqCst), 1);
    drop(page);
    let next = pool.new_page().unwrap();
    assert_eq!(next.read(|bytes| *bytes).unwrap(), [0; PAGE_SIZE]);
    assert_eq!(store.allocations.load(SeqCst), 2);
}

#[test]
fn dirty_eviction_persists_and_page_ids_are_not_frame_indices() {
    let (store, pool) = setup(1);
    for _ in 0..9 {
        store.allocate_page().unwrap();
    }
    let page = pool.new_page().unwrap();
    let id = page.page_id();
    assert!(id >= 9);
    page.write(|bytes| bytes.fill(42)).unwrap();
    drop(page);
    drop(pool.new_page().unwrap());
    assert_eq!(store.writes.load(SeqCst), 1);
    let restored = pool.pin(id).unwrap();
    assert_eq!(restored.page_id(), id);
    assert_eq!(restored.read(|bytes| *bytes).unwrap(), [42; PAGE_SIZE]);
}

#[test]
fn clean_pages_and_read_callbacks_do_not_write() {
    let (store, pool) = setup(1);
    let a = store.allocate_page().unwrap();
    let b = store.allocate_page().unwrap();
    pool.pin(a)
        .unwrap()
        .read(|page| assert_eq!(page[0], 0))
        .unwrap();
    assert!(pool.flush_page(a).unwrap());
    drop(pool.pin(b).unwrap());
    pool.flush_all().unwrap();
    assert_eq!(store.writes.load(SeqCst), 0);
    assert_eq!(store.syncs.load(SeqCst), 1);
    assert!(!pool.flush_page(a).unwrap());
    assert_eq!(store.syncs.load(SeqCst), 1);
}

#[test]
fn drop_does_not_flush_dirty_pages() {
    let (store, pool) = setup(1);
    pool.new_page().unwrap().write(|page| page[0] = 99).unwrap();
    drop(pool);
    assert_eq!(store.writes.load(SeqCst), 0);
    assert_eq!(store.syncs.load(SeqCst), 0);
}

#[test]
fn failed_read_replacement_retains_old_mapping_bytes_and_dirty_state() {
    let (store, pool) = setup(1);
    let a = store.allocate_page().unwrap();
    let b = store.allocate_page().unwrap();
    pool.pin(a).unwrap().write(|page| page.fill(31)).unwrap();
    store.fail_read.store(true, SeqCst);
    assert!(matches!(pool.pin(b), Err(BufferError::Io(_))));
    let reads = store.reads.load(SeqCst);
    assert_eq!(
        pool.pin(a).unwrap().read(|page| *page).unwrap(),
        [31; PAGE_SIZE]
    );
    assert_eq!(store.reads.load(SeqCst), reads);
    drop(pool.pin(b).unwrap());
    assert_eq!(
        pool.pin(a).unwrap().read(|page| *page).unwrap(),
        [31; PAGE_SIZE]
    );
    assert_eq!(store.writes.load(SeqCst), 1);
}

#[test]
fn failed_write_replacement_retains_old_mapping_bytes_and_dirty_state() {
    let (store, pool) = setup(1);
    let a = store.allocate_page().unwrap();
    let b = store.allocate_page().unwrap();
    pool.pin(a).unwrap().write(|page| page.fill(53)).unwrap();
    store.fail_write.store(true, SeqCst);
    assert!(matches!(pool.pin(b), Err(BufferError::Io(_))));
    let reads = store.reads.load(SeqCst);
    assert_eq!(
        pool.pin(a).unwrap().read(|page| *page).unwrap(),
        [53; PAGE_SIZE]
    );
    assert_eq!(store.reads.load(SeqCst), reads);
    drop(pool.pin(b).unwrap());
    assert_eq!(store.writes.load(SeqCst), 2);
    assert_eq!(
        pool.pin(a).unwrap().read(|page| *page).unwrap(),
        [53; PAGE_SIZE]
    );
}

#[test]
fn failed_allocation_retains_old_mapping_and_bytes() {
    let (store, pool) = setup(1);
    let page = pool.new_page().unwrap();
    let id = page.page_id();
    page.write(|bytes| bytes.fill(81)).unwrap();
    drop(page);
    store.fail_allocate.store(true, SeqCst);
    assert!(matches!(pool.new_page(), Err(BufferError::Io(_))));
    let reads = store.reads.load(SeqCst);
    assert_eq!(
        pool.pin(id).unwrap().read(|page| *page).unwrap(),
        [81; PAGE_SIZE]
    );
    assert_eq!(store.reads.load(SeqCst), reads);
    drop(pool.new_page().unwrap());
    assert_eq!(
        pool.pin(id).unwrap().read(|page| *page).unwrap(),
        [81; PAGE_SIZE]
    );
}

#[test]
fn failed_new_page_write_does_not_allocate() {
    let (store, pool) = setup(1);
    let page = pool.new_page().unwrap();
    let id = page.page_id();
    page.write(|bytes| bytes[0] = 3).unwrap();
    drop(page);
    store.fail_write.store(true, SeqCst);
    assert!(matches!(pool.new_page(), Err(BufferError::Io(_))));
    assert_eq!(store.allocations.load(SeqCst), 1);
    assert_eq!(pool.pin(id).unwrap().read(|page| page[0]).unwrap(), 3);
    drop(pool.new_page().unwrap());
    assert_eq!(store.writes.load(SeqCst), 2);
}

#[test]
fn failed_flush_with_retained_pin_retries_dirty_page_without_syncing() {
    let (store, pool) = setup(1);
    let page = pool.new_page().unwrap();
    let id = page.page_id();
    page.write(|bytes| bytes[17] = 9).unwrap();
    store.fail_write.store(true, SeqCst);
    assert!(matches!(pool.flush_page(id), Err(BufferError::Io(_))));
    assert!(pool.flush_page(id).unwrap());
    assert_eq!(store.writes.load(SeqCst), 2);
    assert!(pool.flush_page(id).unwrap());
    assert_eq!(store.writes.load(SeqCst), 2);
    assert_eq!(store.syncs.load(SeqCst), 0);
    let mut persisted = [0; PAGE_SIZE];
    store.read_page(id, &mut persisted).unwrap();
    assert_eq!(persisted[17], 9);
    assert_eq!(page.read(|bytes| bytes[17]).unwrap(), 9);
}

#[test]
fn flush_all_propagates_write_and_sync_failures_and_can_retry() {
    let (store, pool) = setup(1);
    pool.new_page().unwrap().write(|page| page[0] = 5).unwrap();
    store.fail_write.store(true, SeqCst);
    assert!(matches!(pool.flush_all(), Err(BufferError::Io(_))));
    assert_eq!(store.syncs.load(SeqCst), 0);
    store.fail_sync.store(true, SeqCst);
    assert!(matches!(pool.flush_all(), Err(BufferError::Io(_))));
    assert_eq!(store.writes.load(SeqCst), 2);
    pool.flush_all().unwrap();
    assert_eq!(store.syncs.load(SeqCst), 2);
    assert_eq!(store.writes.load(SeqCst), 2);
}

#[test]
fn explicit_flush_persists_across_file_store_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pages");
    let id;
    {
        let store = Arc::new(FilePageStore::open(&path).unwrap());
        let pool = BufferPool::new(2, store).unwrap();
        let page = pool.new_page().unwrap();
        id = page.page_id();
        page.write(|bytes| bytes.fill(127)).unwrap();
        drop(page);
        pool.flush_all().unwrap();
    }
    let pool = BufferPool::new(1, Arc::new(FilePageStore::open(&path).unwrap())).unwrap();
    assert_eq!(
        pool.pin(id).unwrap().read(|page| *page).unwrap(),
        [127; PAGE_SIZE]
    );
}

#[test]
fn concurrent_read_callbacks_share_latch_and_writer_waits() {
    let (store, pool) = setup(1);
    let id = store.allocate_page().unwrap();
    let other = store.allocate_page().unwrap();
    let readers_ready = Barrier::new(3);
    let release_readers = Barrier::new(3);
    let writer_start = Barrier::new(2);
    let active_readers = AtomicUsize::new(0);
    thread::scope(|scope| {
        let mut readers = Vec::new();
        for _ in 0..2 {
            readers.push(scope.spawn(|| {
                let pin = pool.pin(id).unwrap();
                pin.read(|page| {
                    active_readers.fetch_add(1, SeqCst);
                    readers_ready.wait();
                    release_readers.wait();
                    assert_eq!(page, &[0; PAGE_SIZE]);
                    active_readers.fetch_sub(1, SeqCst);
                })
                .unwrap();
            }));
        }
        readers_ready.wait();
        assert_eq!(active_readers.load(SeqCst), 2);
        assert!(matches!(pool.pin(other), Err(BufferError::AllPinned)));
        let writer = scope.spawn(|| {
            let pin = pool.pin(id).unwrap();
            writer_start.wait();
            pin.write(|page| {
                assert_eq!(active_readers.load(SeqCst), 0);
                page.fill(211);
            })
            .unwrap();
        });
        writer_start.wait();
        release_readers.wait();
        for reader in readers {
            reader.join().unwrap();
        }
        writer.join().unwrap();
    });
    drop(pool.pin(other).unwrap());
    assert_eq!(
        pool.pin(id).unwrap().read(|page| *page).unwrap(),
        [211; PAGE_SIZE]
    );
}

#[test]
fn reader_panic_releases_latch_without_poisoning_immutable_contents() {
    let (_, pool) = setup(1);
    let pin = pool.new_page().unwrap();
    pin.write(|page| page[0] = 5).unwrap();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = pin.read(|_| panic!("reader panic"));
    }));
    assert!(outcome.is_err());
    assert_eq!(pin.read(|page| page[0]).unwrap(), 5);
    pin.write(|page| page[0] = 6).unwrap();
    assert!(pool.flush_page(pin.page_id()).unwrap());
}

#[test]
fn retained_pins_allow_sequential_access_and_unpin_independently() {
    let (store, pool) = setup(1);
    let id = store.allocate_page().unwrap();
    let other = store.allocate_page().unwrap();
    let first = pool.pin(id).unwrap();
    let second = pool.pin(id).unwrap();
    assert_eq!(first.buffer_id(), second.buffer_id());
    assert_eq!(first.read(|page| page[0]).unwrap(), 0);
    assert_eq!(
        first
            .write(|page| {
                page[0] = 19;
                "written"
            })
            .unwrap(),
        "written"
    );
    assert_eq!(second.read(|page| page[0]).unwrap(), 19);
    second.write(|page| page[1] = 23).unwrap();
    assert_eq!(first.read(|page| page[1]).unwrap(), 23);
    assert!(matches!(pool.pin(other), Err(BufferError::AllPinned)));
    drop(first);
    assert!(matches!(pool.pin(other), Err(BufferError::AllPinned)));
    assert_eq!(second.read(|page| page[0]).unwrap(), 19);
    drop(second);
    assert_eq!(pool.pin(other).unwrap().page_id(), other);
}

#[test]
fn pin_takes_no_content_latch_and_write_callback_releases_it_before_pin_drop() {
    let (store, pool) = setup(1);
    let id = store.allocate_page().unwrap();
    let writer_entered = Barrier::new(2);
    let pin_acquired = Barrier::new(2);
    let write_finished = Barrier::new(2);
    let accesses_finished = Barrier::new(2);
    thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let pin = pool.pin(id).unwrap();
            pin.write(|page| {
                page[0] = 37;
                writer_entered.wait();
                // The other thread must pin successfully while this latch is held.
                pin_acquired.wait();
            })
            .unwrap();
            write_finished.wait();
            accesses_finished.wait();
            assert_eq!(pin.read(|page| page[0]).unwrap(), 41);
        });
        writer_entered.wait();
        let pin = pool.pin(id).unwrap();
        pin_acquired.wait();
        write_finished.wait();
        assert_eq!(pin.read(|page| page[0]).unwrap(), 37);
        pin.write(|page| page[0] = 41).unwrap();
        accesses_finished.wait();
        writer.join().unwrap();
    });
}

#[test]
fn writes_are_dirty_and_flushable_before_pin_drop() {
    let (store, pool) = setup(1);
    let id = store.allocate_page().unwrap();
    let pin = pool.pin(id).unwrap();
    pin.write(|page| page[29] = 67).unwrap();
    assert_eq!(store.writes.load(SeqCst), 0);
    assert!(pool.flush_page(id).unwrap());
    assert_eq!(store.writes.load(SeqCst), 1);
    let mut persisted = [0; PAGE_SIZE];
    store.read_page(id, &mut persisted).unwrap();
    assert_eq!(persisted[29], 67);
    pin.write(|page| page[29] = 71).unwrap();
    pool.flush_all().unwrap();
    store.read_page(id, &mut persisted).unwrap();
    assert_eq!(persisted[29], 71);
    assert_eq!(pin.read(|page| page[29]).unwrap(), 71);
    assert_eq!(store.writes.load(SeqCst), 2);
    drop(pin);
    assert!(pool.flush_page(id).unwrap());
    assert_eq!(store.writes.load(SeqCst), 2);
}

#[test]
fn even_nonmutating_write_callbacks_mark_dirty() {
    let (store, pool) = setup(1);
    let id = store.allocate_page().unwrap();
    let pin = pool.pin(id).unwrap();
    assert_eq!(pin.write(|page| page[0]).unwrap(), 0);
    assert!(pool.flush_page(id).unwrap());
    assert_eq!(store.writes.load(SeqCst), 1);
}

#[test]
fn new_page_pin_is_zero_and_dropping_it_does_no_io() {
    let (store, pool) = setup(1);
    let pin = pool.new_page().unwrap();
    assert_eq!(pin.read(|page| *page).unwrap(), [0; PAGE_SIZE]);
    assert_eq!(store.allocations.load(SeqCst), 1);
    assert_eq!(store.reads.load(SeqCst), 0);
    drop(pin);
    assert_eq!(store.allocations.load(SeqCst), 1);
    assert_eq!(store.reads.load(SeqCst), 0);
    assert_eq!(store.writes.load(SeqCst), 0);
    assert_eq!(store.syncs.load(SeqCst), 0);
}
