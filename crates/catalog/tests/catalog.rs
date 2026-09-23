use chilidb_binder::{Binder, Catalog, LogicalType, TableSource, bound};
use chilidb_catalog::{CatalogError, HeapCatalog};
use chilidb_heapam::HeapTable;
use chilidb_storage::{BufferPool, FilePageStore, MemoryPageStore, PageStore};
use std::{borrow::Cow, num::NonZeroU32, sync::Arc};

#[test]
fn many_tables_reopen_across_interleaved_metadata_pages() {
    let store = Arc::new(MemoryPageStore::new());
    let (root, expected) = {
        let pool = Arc::new(BufferPool::new(1, store.clone()).unwrap());
        let catalog = HeapCatalog::create(pool.clone()).unwrap();
        let mut expected = Vec::new();
        for i in 0..80 {
            let mut ddl = declaration();
            ddl.name = format!("t_{i}_{}", "n".repeat(96)).into();
            let source = catalog.create_table(&ddl).unwrap();
            let ctid = source.heap().insert(b"opaque row").unwrap();
            expected.push((ddl.name.into_owned(), source.heap().head_page_id(), ctid));
        }
        pool.flush_all().unwrap();
        (catalog.root_page_id(), expected)
    };
    let pool = Arc::new(BufferPool::new(1, store).unwrap());
    let catalog = HeapCatalog::open(pool, root).unwrap();
    for (name, head, ctid) in expected {
        let source = catalog.table(&name).unwrap().unwrap();
        assert_eq!(source.heap().head_page_id(), head);
        assert_eq!(
            source.heap().get(ctid).unwrap(),
            Some(b"opaque row".to_vec())
        );
    }
}

#[test]
fn publication_failure_does_not_cache_table_and_retry_reopens() {
    use chilidb_storage::{Page, PageId};
    use std::{
        io,
        sync::atomic::{AtomicBool, Ordering::SeqCst},
    };
    struct Store {
        inner: MemoryPageStore,
        fail: AtomicBool,
    }
    impl PageStore for Store {
        fn allocate_page(&self) -> io::Result<PageId> {
            self.inner.allocate_page()
        }
        fn read_page(&self, id: PageId, out: &mut Page) -> io::Result<()> {
            if id == 0 && self.fail.swap(false, SeqCst) {
                return Err(io::Error::other("metadata read failure"));
            }
            self.inner.read_page(id, out)
        }
        fn write_page(&self, id: PageId, bytes: &Page) -> io::Result<()> {
            self.inner.write_page(id, bytes)
        }
        fn sync(&self) -> io::Result<()> {
            self.inner.sync()
        }
    }
    let store = Arc::new(Store {
        inner: MemoryPageStore::new(),
        fail: AtomicBool::new(false),
    });
    let root = {
        let pool = Arc::new(BufferPool::new(1, store.clone()).unwrap());
        let catalog = HeapCatalog::create(pool.clone()).unwrap();
        store.fail.store(true, SeqCst);
        assert!(matches!(
            catalog.create_table(&declaration()),
            Err(CatalogError::Heap(_))
        ));
        assert!(catalog.table("types").unwrap().is_none());
        let source = catalog.create_table(&declaration()).unwrap();
        assert_eq!(source.heap().head_page_id(), 2);
        pool.flush_all().unwrap();
        catalog.root_page_id()
    };
    let pool = Arc::new(BufferPool::new(1, store).unwrap());
    let catalog = HeapCatalog::open(pool, root).unwrap();
    assert_eq!(
        catalog
            .table("types")
            .unwrap()
            .unwrap()
            .heap()
            .head_page_id(),
        2
    );
}

fn declaration() -> bound::CreateTable<'static> {
    let types = vec![
        LogicalType::Boolean,
        LogicalType::Int32,
        LogicalType::Int64,
        LogicalType::Uint32,
        LogicalType::Float32,
        LogicalType::Float64,
        LogicalType::Text,
        LogicalType::Varchar(None),
        LogicalType::Varchar(NonZeroU32::new(u32::MAX)),
    ];
    bound::CreateTable {
        name: Cow::Borrowed("types"),
        columns: types
            .into_iter()
            .enumerate()
            .map(|(i, data_type)| bound::CreateColumn {
                name: Cow::Owned(format!("c{i}")),
                data_type,
                nullable: i != 0,
            })
            .collect(),
        constraints: vec![
            bound::TableConstraint::PrimaryKey(vec![0]),
            bound::TableConstraint::Unique(vec![1, 2]),
        ],
    }
}

#[test]
fn types_constraints_identity_and_file_reopen_single_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let root;
    let head;
    let ddl = declaration();
    {
        let store = Arc::new(FilePageStore::open(&path).unwrap());
        store.allocate_page().unwrap(); // The bootstrap root is not page zero.
        let pool = Arc::new(BufferPool::new(1, store).unwrap());
        let catalog = HeapCatalog::create(pool.clone()).unwrap();
        root = catalog.root_page_id();
        assert_ne!(root, 0);
        let source = catalog.create_table(&ddl).unwrap();
        head = source.heap().head_page_id();
        assert!(Arc::ptr_eq(
            &source,
            &catalog.table("types").unwrap().unwrap()
        ));
        let a = catalog.get_table("types").unwrap().unwrap();
        let b = catalog.get_table("types").unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(catalog.table("missing").unwrap().is_none());
        pool.flush_all().unwrap();
    }
    let pool = Arc::new(BufferPool::new(1, Arc::new(FilePageStore::open(&path).unwrap())).unwrap());
    let catalog = HeapCatalog::open(pool, root).unwrap();
    let source = catalog.table("types").unwrap().unwrap();
    assert_eq!(source.heap().head_page_id(), head);
    assert_eq!(source.constraints(), ddl.constraints);
    assert_eq!(
        source
            .schema()
            .columns
            .iter()
            .map(|c| (c.data_type.clone(), c.nullable))
            .collect::<Vec<_>>(),
        ddl.columns
            .iter()
            .map(|c| (c.data_type.clone(), c.nullable))
            .collect::<Vec<_>>()
    );
}

#[test]
fn binds_create_and_query() {
    let pool = Arc::new(BufferPool::new(2, Arc::new(MemoryPageStore::new())).unwrap());
    let catalog = HeapCatalog::create(pool).unwrap();
    let parsed =
        chilidb_parser::parse_sql("CREATE TABLE T (id INT PRIMARY KEY, name VARCHAR(25) UNIQUE)")
            .unwrap();
    let bound::Statement::CreateTable(ddl) = Binder::new(&catalog).bind(&parsed[0]).unwrap() else {
        panic!()
    };
    let source = catalog.create_table(&ddl).unwrap();
    assert!(!source.schema().columns[0].nullable);
    let parsed = chilidb_parser::parse_sql("SELECT id, name FROM t").unwrap();
    let bound::Statement::Select(query) = Binder::new(&catalog).bind(&parsed[0]).unwrap() else {
        panic!()
    };
    assert!(Arc::ptr_eq(
        &query.source.unwrap().source,
        &catalog.get_table("t").unwrap().unwrap()
    ));
}

#[test]
fn invalid_and_duplicate_declarations_do_not_allocate() {
    let store = Arc::new(MemoryPageStore::new());
    let pool = Arc::new(BufferPool::new(1, store.clone()).unwrap());
    let catalog = HeapCatalog::create(pool).unwrap();
    let good = declaration();
    catalog.create_table(&good).unwrap();
    let before = store.allocate_page().unwrap();
    assert!(matches!(
        catalog.create_table(&good),
        Err(CatalogError::TableExists(_))
    ));
    let mut invalid = good.clone();
    invalid.name = "bad".into();
    let mut cases = vec![];
    invalid.columns[0].nullable = true;
    cases.push(invalid.clone());
    invalid.columns[0].nullable = false;
    for indices in [vec![], vec![0, 0], vec![100]] {
        invalid.constraints = vec![bound::TableConstraint::Unique(indices)];
        cases.push(invalid.clone());
    }
    invalid.constraints = vec![bound::TableConstraint::PrimaryKey(vec![0]); 2];
    cases.push(invalid.clone());
    invalid.constraints.clear();
    invalid.columns[0].data_type = LogicalType::Null;
    cases.push(invalid.clone());
    invalid.columns[0].data_type = LogicalType::Int32;
    invalid.columns[1].name = invalid.columns[0].name.clone();
    cases.push(invalid.clone());
    invalid.columns[1].name = "x".repeat(20_000).into();
    cases.push(invalid);
    for ddl in cases {
        assert!(matches!(
            catalog.create_table(&ddl),
            Err(CatalogError::InvalidMetadata(_))
        ));
    }
    assert_eq!(store.allocate_page().unwrap(), before + 1);
}

#[test]
fn rejects_plain_heaps_and_corrupt_records() {
    let pool = Arc::new(BufferPool::new(1, Arc::new(MemoryPageStore::new())).unwrap());
    let plain = HeapTable::create(pool.clone()).unwrap();
    assert!(HeapCatalog::open(pool.clone(), plain.head_page_id()).is_err());
    for bytes in [
        b"garbage".as_slice(),
        b"CHILICAT\x02\x00",
        b"CHILICAT\x01\x00",
    ] {
        let catalog = HeapCatalog::create(pool.clone()).unwrap();
        let root = catalog.root_page_id();
        drop(catalog);
        HeapTable::open(pool.clone(), root)
            .unwrap()
            .insert(bytes)
            .unwrap();
        assert!(HeapCatalog::open(pool.clone(), root).is_err());
    }
}

#[test]
fn rejects_duplicate_metadata_and_self_roots() {
    for self_root in [false, true] {
        let pool = Arc::new(BufferPool::new(1, Arc::new(MemoryPageStore::new())).unwrap());
        let catalog = HeapCatalog::create(pool.clone()).unwrap();
        catalog.create_table(&declaration()).unwrap();
        let root = catalog.root_page_id();
        drop(catalog);
        let metadata = HeapTable::open(pool.clone(), root).unwrap();
        let records = metadata
            .scan()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let (ctid, mut bytes) = records
            .into_iter()
            .find(|(_, b)| b.starts_with(b"CHILICAT\x01\x01"))
            .unwrap();
        if self_root {
            metadata.delete(ctid).unwrap();
            bytes[10..14].copy_from_slice(&root.to_le_bytes());
        }
        metadata.insert(&bytes).unwrap();
        drop(metadata);
        assert!(HeapCatalog::open(pool, root).is_err());
    }
}
