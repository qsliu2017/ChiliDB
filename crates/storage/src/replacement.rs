use crate::BufferId;

/// Replacement policy independent of page storage and pin accounting.
///
/// Callbacks run under the pool's metadata mutex and must not reenter the pool.
/// `access` records a client cache-hit pin; `load` records successful publication
/// of a new page in a buffer. Internal flush pins generate neither event.
/// `victim` must return an eligible
/// buffer, or `None` only if it found no eligible buffer. The pool always prefers
/// free frames without consulting `victim`, and validates every returned index
/// and pin count: a faulty policy can impair availability, but not pin safety.
pub trait VictimStrategy: Send {
    fn access(&mut self, buffer: BufferId);
    /// Reset per-page history when a slot is assigned to a new page. Policies
    /// such as FIFO or LRU-K can distinguish installation from a cache hit.
    fn load(&mut self, buffer: BufferId) {
        self.access(buffer);
    }
    /// Selection does not confirm removal: subsequent I/O may fail. Keep the
    /// candidate tracked for future selection until load confirms reassignment.
    fn victim(&mut self, can_evict: &dyn Fn(BufferId) -> bool) -> Option<BufferId>;
}

/// Second-chance replacement with policy-owned reference bits and clock hand.
pub struct ClockStrategy {
    referenced: Box<[bool]>,
    hand: usize,
}

impl ClockStrategy {
    /// Construct a clock for `capacity` frames. An empty clock has no victim.
    /// Panics if the capacity exceeds the BufferId address space.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity == 0 || u32::try_from(capacity - 1).is_ok());
        Self {
            referenced: vec![false; capacity].into_boxed_slice(),
            hand: 0,
        }
    }
}

impl VictimStrategy for ClockStrategy {
    fn access(&mut self, buffer: BufferId) {
        self.referenced[buffer.get() as usize] = true;
    }

    fn victim(&mut self, can_evict: &dyn Fn(BufferId) -> bool) -> Option<BufferId> {
        // First sweep clears eligible reference bits; the second selects one.
        for _ in 0..2 {
            for _ in 0..self.referenced.len() {
                let index = self.hand;
                self.hand = (self.hand + 1) % self.referenced.len();
                let buffer = BufferId::new(index as u32);
                if !can_evict(buffer) {
                    continue;
                }
                if !self.referenced[index] {
                    return Some(buffer);
                }
                self.referenced[index] = false;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use crate::{
        BufferError, BufferId, BufferPool, ClockStrategy, MemoryPageStore, Page, PageId, PageStore,
        VictimStrategy,
    };

    #[test]
    fn buffer_id_covers_u32() {
        const ZERO: BufferId = BufferId::new(0);
        const MAX: BufferId = BufferId::new(u32::MAX);
        assert_eq!(ZERO.get(), 0);
        assert_eq!(MAX.get(), u32::MAX);
        assert_eq!(size_of::<BufferId>(), size_of::<u32>());
    }

    #[test]
    fn clock_second_chance_and_eligibility() {
        let mut clock = ClockStrategy::new(3);
        for index in 0..3 {
            clock.access(BufferId::new(index));
        }
        assert_eq!(clock.victim(&|_| false), None);
        assert_eq!(clock.victim(&|id| id.get() != 0), Some(BufferId::new(1)));
        clock.access(BufferId::new(2));
        assert_eq!(clock.victim(&|_| true), Some(BufferId::new(1)));
        assert_eq!(clock.victim(&|_| true), Some(BufferId::new(2)));
    }

    #[test]
    fn empty_clock_never_consults_eligibility() {
        assert_eq!(
            ClockStrategy::new(0).victim(&|_| panic!("empty clock")),
            None
        );
    }

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
            let pool = BufferPool::with_strategy(2, store.clone(), Box::new(BadPolicy(candidate)))
                .unwrap();
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
}
