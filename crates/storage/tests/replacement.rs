use std::{
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use chilidb_storage::{
    BufferError, BufferId, BufferPool, ClockStrategy, MemoryPageStore, Page, PageId, PageStore,
    VictimStrategy,
};

struct PreferLast(usize);
impl VictimStrategy for PreferLast {
    fn access(&mut self, _: BufferId) {}
    fn victim(&mut self, can_evict: &dyn Fn(BufferId) -> bool) -> Option<BufferId> {
        (0..self.0)
            .rev()
            .map(|i| BufferId::new(i as u32))
            .find(|&id| can_evict(id))
    }
}

#[test]
fn custom_policy_controls_replacement_but_not_free_frame_allocation() {
    let pool =
        BufferPool::with_strategy(2, Arc::new(MemoryPageStore::new()), Box::new(PreferLast(2)))
            .unwrap();
    let first = pool.new_page().unwrap();
    let first_page = first.page_id();
    let first_buffer = first.buffer_id();
    first.write(|page| page[0] = 11).unwrap();
    drop(first);
    let second = pool.new_page().unwrap();
    let second_buffer = second.buffer_id();
    assert_ne!(first_buffer, second_buffer);
    drop(second);
    let third = pool.new_page().unwrap();
    assert_eq!(third.buffer_id(), second_buffer);
    let first = pool.pin(first_page).unwrap();
    assert_eq!(first.buffer_id(), first_buffer);
    assert_eq!(first.read(|page| page[0]).unwrap(), 11);
}

#[derive(Default)]
struct CountingStore {
    inner: MemoryPageStore,
    allocations: AtomicUsize,
    fail_allocate: AtomicBool,
}
impl PageStore for CountingStore {
    fn allocate_page(&self) -> io::Result<PageId> {
        self.allocations.fetch_add(1, Ordering::SeqCst);
        if self.fail_allocate.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("allocation failure"));
        }
        self.inner.allocate_page()
    }
    fn read_page(&self, id: PageId, out: &mut Page) -> io::Result<()> {
        self.inner.read_page(id, out)
    }
    fn write_page(&self, id: PageId, data: &Page) -> io::Result<()> {
        self.inner.write_page(id, data)
    }
    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
}
struct BadPolicy(BufferId);
impl VictimStrategy for BadPolicy {
    fn access(&mut self, _: BufferId) {}
    fn victim(&mut self, _: &dyn Fn(BufferId) -> bool) -> Option<BufferId> {
        Some(self.0)
    }
}

#[test]
fn invalid_policy_cannot_evict_pinned_memory_or_allocate_an_orphan() {
    for candidate in [BufferId::new(0), BufferId::new(u32::MAX)] {
        let store = Arc::new(CountingStore::default());
        let pool =
            BufferPool::with_strategy(2, store.clone(), Box::new(BadPolicy(candidate))).unwrap();
        let held = pool.new_page().unwrap();
        held.write(|page| page[0] = 93).unwrap();
        drop(pool.new_page().unwrap());
        assert!(matches!(
            pool.new_page(),
            Err(BufferError::InvalidVictim(_))
        ));
        assert_eq!(store.allocations.load(Ordering::SeqCst), 2);
        assert_eq!(held.read(|page| page[0]).unwrap(), 93);
    }
}

struct Recording {
    clock: ClockStrategy,
    events: Arc<Mutex<Vec<BufferId>>>,
}
impl VictimStrategy for Recording {
    fn access(&mut self, id: BufferId) {
        self.events.lock().unwrap().push(id);
        self.clock.access(id);
    }
    fn victim(&mut self, can_evict: &dyn Fn(BufferId) -> bool) -> Option<BufferId> {
        self.clock.victim(can_evict)
    }
}

#[test]
fn allocations_and_cache_hits_notify_the_policy() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let pool = BufferPool::with_strategy(
        1,
        Arc::new(MemoryPageStore::new()),
        Box::new(Recording {
            clock: ClockStrategy::new(1),
            events: events.clone(),
        }),
    )
    .unwrap();
    let pin = pool.new_page().unwrap();
    let page = pin.page_id();
    let id = pin.buffer_id();
    drop(pin);
    drop(pool.pin(page).unwrap());
    assert_eq!(*events.lock().unwrap(), vec![id, id]);
    pool.flush_all().unwrap();
    assert_eq!(*events.lock().unwrap(), vec![id, id]);
}

#[test]
fn fifo_distinguishes_hits_from_loads_and_keeps_failed_victims() {
    use std::collections::VecDeque;
    #[derive(Default)]
    struct Fifo(VecDeque<BufferId>);
    impl VictimStrategy for Fifo {
        fn access(&mut self, _: BufferId) {}
        fn load(&mut self, id: BufferId) {
            self.0.retain(|&old| old != id);
            self.0.push_back(id);
        }
        fn victim(&mut self, can_evict: &dyn Fn(BufferId) -> bool) -> Option<BufferId> {
            self.0.iter().copied().find(|&id| can_evict(id))
        }
    }
    let store = Arc::new(CountingStore::default());
    let pool = BufferPool::with_strategy(2, store.clone(), Box::new(Fifo::default())).unwrap();
    let a = pool.new_page().unwrap();
    let (a_page, a_buffer) = (a.page_id(), a.buffer_id());
    drop(a);
    let b = pool.new_page().unwrap();
    let b_buffer = b.buffer_id();
    drop(b);
    drop(pool.pin(a_page).unwrap());
    store.fail_allocate.store(true, Ordering::SeqCst);
    assert!(matches!(pool.new_page(), Err(BufferError::Io(_))));
    let c = pool.new_page().unwrap();
    assert_eq!(c.buffer_id(), a_buffer);
    drop(c);
    assert_eq!(pool.new_page().unwrap().buffer_id(), b_buffer);
}
