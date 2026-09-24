//! Heap-backed catalog of versioned table definitions.
//!
//! `HeapCatalog::create` allocates a metadata heap whose root holds a versioned
//! manifest; the root is not necessarily page zero, so callers retain
//! `root_page_id()` and pass it to `open`. Records store names, column types and
//! nullability, constraint column indices, and table heap heads. They are
//! metadata records, not SQL-queryable system tables.
//!
//! Share one catalog per root with `Arc`. Creation and lookup serialize through
//! a mutex, and cached `HeapTableSource` handles keep their `Arc` identity. `open`
//! validates all metadata, rejecting missing manifests, malformed records,
//! duplicate names, and duplicate or self-referencing heap roots.
//!
//! Names are stored as supplied; the binder normalizes identifiers. Declarations,
//! metadata size, and name uniqueness are checked before allocating a table heap,
//! but a failed publication can orphan a heap page. Reopening requires a
//! successful pool flush with mutations quiesced and old handles dropped.
//! Constraints are metadata only. There is no WAL, crash-atomic publication,
//! MVCC, DROP, or ALTER.

mod codec;

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex},
};

use chilidb_binder::{Catalog, ColumnSchema, TableSchema, TableSource, bound};
use chilidb_heapam::{HeapError, HeapTable};
use chilidb_storage::{BufferPool, PageId};

/// Failures reading or publishing heap-backed metadata.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error(transparent)]
    Heap(#[from] HeapError),
    #[error("table already exists: {0}")]
    TableExists(String),
    #[error("invalid table metadata: {0}")]
    InvalidMetadata(String),
    #[error("corrupt catalog: {0}")]
    Corruption(String),
    #[error("catalog lock is poisoned")]
    Poisoned,
}

/// Stable storage identity and immutable metadata for a table.
pub struct HeapTableSource {
    heap: Arc<HeapTable>,
    schema: Arc<TableSchema>,
    constraints: Vec<bound::TableConstraint>,
}

impl fmt::Debug for HeapTableSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeapTableSource")
            .field("head", &self.heap.head_page_id())
            .field("schema", &self.schema)
            .field("constraints", &self.constraints)
            .finish()
    }
}

impl HeapTableSource {
    pub fn heap(&self) -> &Arc<HeapTable> {
        &self.heap
    }
    pub fn constraints(&self) -> &[bound::TableConstraint] {
        &self.constraints
    }
}

impl TableSource for HeapTableSource {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.schema)
    }
}

/// A catalog stored as versioned records in a dedicated heap.
///
/// Share one instance per root through `Arc`; independently opened instances do
/// not coordinate their caches. The caller retains the explicit root page ID.
pub struct HeapCatalog {
    pool: Arc<BufferPool>,
    metadata: HeapTable,
    entries: Mutex<HashMap<String, Arc<HeapTableSource>>>,
}

impl HeapCatalog {
    pub fn create(pool: Arc<BufferPool>) -> Result<Self, CatalogError> {
        let metadata = HeapTable::create(Arc::clone(&pool))?;
        metadata.insert(codec::MANIFEST)?;
        Ok(Self {
            pool,
            metadata,
            entries: Mutex::new(HashMap::new()),
        })
    }

    pub fn open(pool: Arc<BufferPool>, root: PageId) -> Result<Self, CatalogError> {
        let metadata = HeapTable::open(Arc::clone(&pool), root)?;
        let mut records = metadata.scan()?;
        let first = records.next().transpose()?;
        if !first.is_some_and(|(_, bytes)| bytes == codec::MANIFEST) {
            return Err(CatalogError::Corruption("missing catalog manifest".into()));
        }
        let mut entries = HashMap::new();
        let mut roots = HashSet::new();
        roots.insert(root);
        for record in records {
            let (_, bytes) = record?;
            if bytes == codec::MANIFEST {
                return Err(CatalogError::Corruption("duplicate manifest".into()));
            }
            let m = codec::decode(&bytes).map_err(CatalogError::Corruption)?;
            if entries.contains_key(&m.schema.name) || !roots.insert(m.head) {
                return Err(CatalogError::Corruption(
                    "duplicate table name or heap root".into(),
                ));
            }
            let heap = HeapTable::open(Arc::clone(&pool), m.head)?;
            entries.insert(
                m.schema.name.clone(),
                Arc::new(HeapTableSource {
                    heap: Arc::new(heap),
                    schema: Arc::new(m.schema),
                    constraints: m.constraints,
                }),
            );
        }
        Ok(Self {
            pool,
            metadata,
            entries: Mutex::new(entries),
        })
    }

    pub fn root_page_id(&self) -> PageId {
        self.metadata.head_page_id()
    }

    pub fn create_table(
        &self,
        declaration: &bound::CreateTable<'_>,
    ) -> Result<Arc<HeapTableSource>, CatalogError> {
        let mut entries = self.entries.lock().map_err(|_| CatalogError::Poisoned)?;
        if entries.contains_key(declaration.name.as_ref()) {
            return Err(CatalogError::TableExists(declaration.name.to_string()));
        }
        let mut m = codec::Metadata {
            head: 0, // Fixed-width placeholder used only for preallocation validation.
            schema: TableSchema {
                name: declaration.name.to_string(),
                columns: declaration
                    .columns
                    .iter()
                    .map(|c| ColumnSchema {
                        name: c.name.to_string(),
                        data_type: c.data_type.clone(),
                        nullable: c.nullable,
                    })
                    .collect(),
            },
            constraints: declaration.constraints.clone(),
        };
        codec::encode(&m).map_err(CatalogError::InvalidMetadata)?;
        let heap = Arc::new(HeapTable::create(Arc::clone(&self.pool))?);
        m.head = heap.head_page_id();
        let bytes = codec::encode(&m).map_err(CatalogError::InvalidMetadata)?;
        self.metadata.insert(&bytes)?;
        let source = Arc::new(HeapTableSource {
            heap,
            schema: Arc::new(m.schema),
            constraints: m.constraints,
        });
        entries.insert(source.schema.name.clone(), Arc::clone(&source));
        Ok(source)
    }

    pub fn table(&self, name: &str) -> Result<Option<Arc<HeapTableSource>>, CatalogError> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| CatalogError::Poisoned)?
            .get(name)
            .cloned())
    }
}

impl Catalog for HeapCatalog {
    type Error = CatalogError;
    fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
        Ok(self
            .table(name)?
            .map(|source| source as Arc<dyn TableSource>))
    }
}
