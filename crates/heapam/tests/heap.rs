use std::{
    collections::HashSet,
    io,
    iter::FusedIterator,
    num::NonZeroU16,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use chilidb_heapam::{Ctid, HeapError, HeapTable, MAX_TUPLE_SIZE};
use chilidb_storage::{BufferPool, FilePageStore, MemoryPageStore, Page, PageId, PageStore};

fn memory_pool(capacity: usize) -> Arc<BufferPool> {
    Arc::new(BufferPool::new(capacity, Arc::new(MemoryPageStore::new())).unwrap())
}

fn rows(heap: &HeapTable) -> Vec<(Ctid, Vec<u8>)> {
    heap.scan().unwrap().collect::<Result<_, _>>().unwrap()
}

#[test]
fn empty_and_variable_binary_records_are_owned() {
    let heap = HeapTable::create(memory_pool(1)).unwrap();
    assert!(rows(&heap).is_empty());
    let mut expected = Vec::new();
    for len in [1, 2, 31, 255, 1000] {
        let bytes: Vec<_> = (0..len).map(|i| (i % 256) as u8).collect();
        let id = heap.insert(&bytes).unwrap();
        assert_eq!(heap.get(id).unwrap(), Some(bytes.clone()));
        expected.push((id, bytes));
    }
    assert_eq!(rows(&heap), expected);
    let mut copy = heap.get(expected[0].0).unwrap().unwrap();
    copy[0] = 255;
    assert_eq!(heap.get(expected[0].0).unwrap(), Some(vec![0]));
}

#[test]
fn multiple_pages_and_scans_release_pins_with_one_frame() {
    let pool = memory_pool(1);
    let heap = HeapTable::create(pool.clone()).unwrap();
    let expected: Vec<_> = (0..5)
        .map(|i| {
            let bytes = vec![i; MAX_TUPLE_SIZE];
            (heap.insert(&bytes).unwrap(), bytes)
        })
        .collect();
    assert_eq!(
        expected
            .iter()
            .map(|(id, _)| id.page_id)
            .collect::<HashSet<_>>()
            .len(),
        5
    );
    let mut scan = heap.scan().unwrap();
    assert_eq!(scan.next().unwrap().unwrap(), expected[0]);
    // A suspended scan owns bytes, not a pin, latch, or heap metadata lock.
    assert_eq!(
        heap.get(expected[4].0).unwrap(),
        Some(expected[4].1.clone())
    );
    drop(pool.new_page().unwrap());
    assert_eq!(scan.collect::<Result<Vec<_>, _>>().unwrap(), expected[1..]);
    let reopened = HeapTable::open(pool, heap.head_page_id()).unwrap();
    assert_eq!(rows(&reopened), expected);
}

#[test]
fn compaction_preserves_ctids_and_never_reuses_deleted_slots() {
    let heap = HeapTable::create(memory_pool(1)).unwrap();
    let a = heap.insert(&[1; 2000]).unwrap();
    let b = heap.insert(&[2; 2000]).unwrap();
    let c = heap.insert(&[3; 2000]).unwrap();
    assert!(heap.delete(b).unwrap());
    assert!(heap.replace(a, &[4; 3500]).unwrap());
    assert_eq!(heap.get(c).unwrap(), Some(vec![3; 2000]));
    assert!(heap.replace(a, b"small").unwrap());
    let d = heap.insert(&[5; 3000]).unwrap();
    assert_eq!(d.page_id, a.page_id);
    assert!(d.slot_id > c.slot_id);
    assert_eq!(
        rows(&heap),
        vec![
            (a, b"small".to_vec()),
            (c, vec![3; 2000]),
            (d, vec![5; 3000])
        ]
    );
    let missing = Ctid {
        page_id: a.page_id,
        slot_id: NonZeroU16::new(u16::MAX).unwrap(),
    };
    for id in [b, missing] {
        assert_eq!(heap.get(id).unwrap(), None);
        assert!(!heap.delete(id).unwrap());
        assert!(!heap.replace(id, b"not resurrected").unwrap());
    }
}

#[test]
fn foreign_heap_ctids_are_rejected_for_all_mutations_and_reads() {
    let pool = memory_pool(1);
    let heap = HeapTable::create(pool.clone()).unwrap();
    let other = HeapTable::create(pool).unwrap();
    let foreign = other.insert(b"foreign").unwrap();
    assert!(matches!(heap.get(foreign), Err(HeapError::InvalidCtid(id)) if id == foreign));
    assert!(matches!(heap.delete(foreign), Err(HeapError::InvalidCtid(id)) if id == foreign));
    assert!(
        matches!(heap.replace(foreign, b"x"), Err(HeapError::InvalidCtid(id)) if id == foreign)
    );
    assert_eq!(other.get(foreign).unwrap(), Some(b"foreign".to_vec()));
}

#[derive(Default)]
struct ObservedStore {
    inner: MemoryPageStore,
    allocations: AtomicUsize,
    fail_next_read: AtomicBool,
}
impl PageStore for ObservedStore {
    fn allocate_page(&self) -> io::Result<PageId> {
        let id = self.inner.allocate_page()?;
        self.allocations.fetch_add(1, Ordering::SeqCst);
        Ok(id)
    }
    fn read_page(&self, id: PageId, bytes: &mut Page) -> io::Result<()> {
        if self.fail_next_read.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("injected read failure"));
        }
        self.inner.read_page(id, bytes)
    }
    fn write_page(&self, id: PageId, bytes: &Page) -> io::Result<()> {
        self.inner.write_page(id, bytes)
    }
    fn sync(&self) -> io::Result<()> {
        self.inner.sync()
    }
}

#[test]
fn size_limits_are_checked_before_allocation_or_modification() {
    let store = Arc::new(ObservedStore::default());
    let pool = Arc::new(BufferPool::new(1, store.clone()).unwrap());
    let heap = HeapTable::create(pool).unwrap();
    let maximum = vec![9; MAX_TUPLE_SIZE];
    let id = heap.insert(&maximum).unwrap();
    for invalid in [vec![], vec![0; MAX_TUPLE_SIZE + 1]] {
        assert!(
            matches!(heap.insert(&invalid), Err(HeapError::InvalidTupleSize { len, max }) if len == invalid.len() && max == MAX_TUPLE_SIZE)
        );
        assert!(matches!(
            heap.replace(id, &invalid),
            Err(HeapError::InvalidTupleSize { .. })
        ));
    }
    assert_eq!(store.allocations.load(Ordering::SeqCst), 1);
    assert_eq!(heap.get(id).unwrap(), Some(maximum));
}

#[test]
fn replacement_no_space_is_atomic_for_old_record_and_neighbors() {
    let heap = HeapTable::create(memory_pool(1)).unwrap();
    let a = heap.insert(&[1; 3000]).unwrap();
    let b = heap.insert(&[2; 3000]).unwrap();
    let c = heap.insert(&[3; 1000]).unwrap();
    let before = rows(&heap);
    assert_eq!(a.page_id, c.page_id);
    assert!(matches!(
        heap.replace(b, &[4; 5000]),
        Err(HeapError::NoSpace)
    ));
    assert_eq!(rows(&heap), before);
}

#[test]
fn scan_excludes_new_slots_new_pages_and_insertions_after_exhaustion() {
    fn fused(_: &impl FusedIterator) {}
    let heap = HeapTable::create(memory_pool(1)).unwrap();
    let a = heap.insert(b"before").unwrap();
    let mut scan = heap.scan().unwrap();
    fused(&scan);
    let b = heap.insert(b"same tail").unwrap();
    assert_eq!(a.page_id, b.page_id);
    let c = heap.insert(&vec![3; MAX_TUPLE_SIZE]).unwrap();
    assert_ne!(a.page_id, c.page_id);
    assert_eq!(scan.next().unwrap().unwrap(), (a, b"before".to_vec()));
    assert!(scan.next().is_none());
    heap.insert(b"after exhaustion").unwrap();
    assert!(scan.next().is_none());
    assert!(scan.next().is_none());
    assert_eq!(rows(&heap).len(), 4);
}

#[test]
fn scan_reads_pages_when_visited_not_an_mvcc_snapshot() {
    let heap = HeapTable::create(memory_pool(1)).unwrap();
    let a = heap.insert(&[1; 3000]).unwrap();
    let b = heap.insert(&[2; 3000]).unwrap();
    let c = heap.insert(&[3; 3000]).unwrap();
    let d = heap.insert(&[4; 3000]).unwrap();
    assert_eq!(a.page_id, b.page_id);
    assert_eq!(c.page_id, d.page_id);
    assert_ne!(a.page_id, c.page_id);
    let mut scan = heap.scan().unwrap();
    heap.replace(a, b"changed before page visit").unwrap();
    assert_eq!(
        scan.next().unwrap().unwrap(),
        (a, b"changed before page visit".to_vec())
    );
    heap.delete(b).unwrap();
    heap.replace(c, b"changed unread page").unwrap();
    heap.delete(d).unwrap();
    // b was copied with a; c and d have not been read yet.
    assert_eq!(scan.next().unwrap().unwrap(), (b, vec![2; 3000]));
    assert_eq!(
        scan.next().unwrap().unwrap(),
        (c, b"changed unread page".to_vec())
    );
    assert!(scan.next().is_none());
    assert_eq!(heap.get(b).unwrap(), None);
}

#[test]
fn flush_drop_and_reopen_preserves_root_chain_and_ctids() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("heap.pages");
    let (root, dead, expected) = {
        let store = Arc::new(FilePageStore::open(&path).unwrap());
        let pool = Arc::new(BufferPool::new(1, store).unwrap());
        // Ensure that reopening does not assume the heap root is page zero.
        drop(pool.new_page().unwrap());
        let heap = HeapTable::create(pool.clone()).unwrap();
        let ids: Vec<_> = (0..4)
            .map(|i| heap.insert(&vec![i; MAX_TUPLE_SIZE]).unwrap())
            .collect();
        heap.delete(ids[1]).unwrap();
        heap.replace(ids[2], b"persisted replacement").unwrap();
        let expected = rows(&heap);
        pool.flush_all().unwrap();
        (heap.head_page_id(), ids[1], expected)
    };
    let store = Arc::new(FilePageStore::open(&path).unwrap());
    let pool = Arc::new(BufferPool::new(1, store).unwrap());
    let heap = HeapTable::open(pool, root).unwrap();
    assert_eq!(heap.head_page_id(), root);
    assert_eq!(rows(&heap), expected);
    assert_eq!(heap.get(dead).unwrap(), None);
    for (id, bytes) in expected {
        assert_eq!(heap.get(id).unwrap(), Some(bytes));
    }
    let appended = heap.insert(b"after reopen").unwrap();
    assert_eq!(heap.get(appended).unwrap(), Some(b"after reopen".to_vec()));
}

fn link(pool: &BufferPool, from: PageId, to: PageId) {
    pool.pin(from)
        .unwrap()
        .write(|page| {
            assert_eq!(&page[..8], b"CHILIHP1");
            page[12..16].copy_from_slice(&to.to_le_bytes());
            page[16] = 1;
        })
        .unwrap();
}

#[test]
fn open_rejects_corrupt_headers_foreign_owners_and_chain_cycles() {
    for offset in [0, 16, 17, 21, 22] {
        let pool = memory_pool(1);
        let heap = HeapTable::create(pool.clone()).unwrap();
        let root = heap.head_page_id();
        pool.pin(root)
            .unwrap()
            .write(|page| page[offset] = 255)
            .unwrap();
        assert!(
            matches!(HeapTable::open(pool, root), Err(HeapError::Corrupt(_))),
            "offset {offset}"
        );
    }
    let pool = memory_pool(1);
    let heap = HeapTable::create(pool.clone()).unwrap();
    let other = HeapTable::create(pool.clone()).unwrap();
    let root = heap.head_page_id();
    link(&pool, root, other.head_page_id());
    assert!(matches!(
        HeapTable::open(pool.clone(), root),
        Err(HeapError::Corrupt(_))
    ));
    // Reassign the second page's owner to isolate cycle detection from ownership.
    pool.pin(other.head_page_id())
        .unwrap()
        .write(|page| {
            page[8..12].copy_from_slice(&root.to_le_bytes());
        })
        .unwrap();
    link(&pool, other.head_page_id(), root);
    assert!(matches!(
        HeapTable::open(pool.clone(), root),
        Err(HeapError::Corrupt(_))
    ));
    link(&pool, root, root);
    assert!(matches!(
        HeapTable::open(pool, root),
        Err(HeapError::Corrupt(_))
    ));
}

#[test]
fn failed_tail_refetch_leaves_no_published_record_and_retry_is_usable() {
    let store = Arc::new(ObservedStore::default());
    let pool = Arc::new(BufferPool::new(1, store.clone()).unwrap());
    let heap = HeapTable::create(pool.clone()).unwrap();
    let first = heap.insert(&vec![1; MAX_TUPLE_SIZE]).unwrap();
    // The tail is resident. Allocation evicts it; linking then must read it back.
    store.fail_next_read.store(true, Ordering::SeqCst);
    assert!(matches!(heap.insert(b"failed"), Err(HeapError::Buffer(_))));
    assert_eq!(store.allocations.load(Ordering::SeqCst), 2);
    assert_eq!(rows(&heap), vec![(first, vec![1; MAX_TUPLE_SIZE])]);
    let orphan = Ctid {
        page_id: first.page_id + 1,
        slot_id: NonZeroU16::new(1).unwrap(),
    };
    assert!(matches!(heap.get(orphan), Err(HeapError::InvalidCtid(_))));
    let retry = heap.insert(b"retry").unwrap();
    assert_ne!(retry.page_id, orphan.page_id);
    let reopened = HeapTable::open(pool, heap.head_page_id()).unwrap();
    assert_eq!(
        rows(&reopened),
        vec![(first, vec![1; MAX_TUPLE_SIZE]), (retry, b"retry".to_vec())]
    );
}

#[test]
fn concurrent_shared_handle_inserts_have_unique_ctids() {
    let heap = Arc::new(HeapTable::create(memory_pool(1)).unwrap());
    let barrier = Arc::new(Barrier::new(4));
    let workers: Vec<_> = (0..4u8)
        .map(|worker| {
            let heap = heap.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                (0..12u8)
                    .map(|i| {
                        let mut bytes = vec![worker; 1000];
                        bytes[0] = i;
                        (heap.insert(&bytes).unwrap(), bytes)
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let inserted: Vec<_> = workers
        .into_iter()
        .flat_map(|w| w.join().unwrap())
        .collect();
    assert_eq!(
        inserted
            .iter()
            .map(|(id, _)| *id)
            .collect::<HashSet<_>>()
            .len(),
        48
    );
    assert_eq!(rows(&heap).len(), 48);
    for (id, bytes) in inserted {
        assert_eq!(heap.get(id).unwrap(), Some(bytes));
    }
}
