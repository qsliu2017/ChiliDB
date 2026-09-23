use std::{
    collections::BTreeSet,
    sync::{Arc, Barrier},
};

use chilidb_storage::{FilePageStore, MemoryPageStore, PAGE_SIZE, PageStore};

fn contract(store: &dyn PageStore) {
    let mut bytes = [0xff; PAGE_SIZE];
    assert!(store.read_page(0, &mut bytes).is_err());
    assert!(store.write_page(0, &bytes).is_err());
    let first = store.allocate_page().unwrap();
    let second = store.allocate_page().unwrap();
    assert_eq!((first, second), (0, 1));
    store.read_page(first, &mut bytes).unwrap();
    assert_eq!(bytes, [0; PAGE_SIZE]);
    let pattern = std::array::from_fn(|i| (i % 251) as u8);
    store.write_page(first, &pattern).unwrap();
    store.sync().unwrap();
    store.read_page(second, &mut bytes).unwrap();
    assert_eq!(bytes, [0; PAGE_SIZE]);
    store.read_page(first, &mut bytes).unwrap();
    assert_eq!(bytes, pattern);
    for bad in [2, u32::MAX] {
        assert!(store.read_page(bad, &mut bytes).is_err());
        assert!(store.write_page(bad, &pattern).is_err());
    }
    assert_eq!(store.allocate_page().unwrap(), 2);
}

#[test]
fn memory_and_file_share_page_contract() {
    contract(&MemoryPageStore::new());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pages");
    contract(&FilePageStore::open(&path).unwrap());
    assert_eq!(
        std::fs::metadata(path).unwrap().len(),
        (3 * PAGE_SIZE) as u64
    );
}

#[test]
fn file_reopen_preserves_bytes_and_allocation_position() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pages");
    {
        let store = FilePageStore::open(&path).unwrap();
        assert_eq!(store.allocate_page().unwrap(), 0);
        store.write_page(0, &[42; PAGE_SIZE]).unwrap();
        store.sync().unwrap();
    }
    let store = FilePageStore::open(&path).unwrap();
    let mut bytes = [0; PAGE_SIZE];
    store.read_page(0, &mut bytes).unwrap();
    assert_eq!(bytes, [42; PAGE_SIZE]);
    assert_eq!(store.allocate_page().unwrap(), 1);
    store.read_page(1, &mut bytes).unwrap();
    assert_eq!(bytes, [0; PAGE_SIZE]);
}

#[test]
fn malformed_file_is_rejected_without_truncation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pages");
    std::fs::write(&path, [1, 2, 3]).unwrap();
    assert!(FilePageStore::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), [1, 2, 3]);
}

fn concurrent_allocation(store: Arc<dyn PageStore>) {
    let barrier = Arc::new(Barrier::new(4));
    let ids = std::thread::scope(|scope| {
        let mut workers = vec![];
        for _ in 0..4 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            workers.push(scope.spawn(move || {
                barrier.wait();
                (0..16)
                    .map(|_| {
                        let id = store.allocate_page().unwrap();
                        store.write_page(id, &[id as u8; PAGE_SIZE]).unwrap();
                        id
                    })
                    .collect::<Vec<_>>()
            }));
        }
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect::<BTreeSet<_>>()
    });
    assert_eq!(ids, (0..64).collect());
    for id in ids {
        let mut bytes = [0; PAGE_SIZE];
        store.read_page(id, &mut bytes).unwrap();
        assert_eq!(bytes, [id as u8; PAGE_SIZE]);
    }
}

#[test]
fn allocation_is_unique_across_threads_for_both_stores() {
    concurrent_allocation(Arc::new(MemoryPageStore::new()));
    let directory = tempfile::tempdir().unwrap();
    concurrent_allocation(Arc::new(
        FilePageStore::open(directory.path().join("pages")).unwrap(),
    ));
}
