//! Storage-independent table sources and catalog lookup.

use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;

use crate::LogicalType;

/// Read-only access to named table sources.
///
/// Implementations provide a consistent catalog view for each query. Persistent
/// IDs and storage-specific lookup remain internal to the implementation.
pub trait Catalog {
    /// A failure to retrieve a table source.
    type Error;

    /// Find a table by its normalized SQL name; `None` means it is absent.
    ///
    /// # Errors
    /// Returns the catalog's error if lookup fails, rather than treating a
    /// failure to read metadata as a missing table.
    fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error>;
}

/// A table handle exposing logical metadata without scan or storage operations.
///
/// The source's schema and column ordering must remain stable while referenced
/// by a bound query. Column bindings index this schema. Catalog implementations
/// can pin a schema version or provide a snapshot-backed source.
///
/// Sources retain their concrete identity for downstream adapters through
/// `downcast_ref` on `dyn TableSource`; equal schemas do not imply equal tables.
pub trait TableSource: Any + Debug + Send + Sync {
    /// Return the immutable schema associated with this source.
    fn schema(&self) -> Arc<TableSchema>;
}

impl dyn TableSource {
    /// Whether this source has concrete type `T`.
    pub fn is<T: TableSource>(&self) -> bool {
        (self as &dyn Any).is::<T>()
    }

    /// Borrow the concrete source for a compatible downstream adapter.
    pub fn downcast_ref<T: TableSource>(&self) -> Option<&T> {
        (self as &dyn Any).downcast_ref()
    }
}

/// Immutable metadata describing a table source's visible columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableSchema {
    /// Catalog name with SQL identifier normalization already applied.
    pub name: String,
    /// Columns in schema order, including the order of `SELECT *`.
    pub columns: Vec<ColumnSchema>,
}

/// The name and declared type of a table column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnSchema {
    /// Catalog name with SQL identifier normalization already applied.
    pub name: String,
    /// Declared SQL type.
    pub data_type: LogicalType,
    /// Whether the column permits NULL values.
    pub nullable: bool,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{Binder, Catalog, ColumnSchema, LogicalType, TableSchema, TableSource, bound};

    #[derive(Debug)]
    struct TestSource {
        backend_id: u64,
        schema: Arc<TableSchema>,
    }

    impl TableSource for TestSource {
        fn schema(&self) -> Arc<TableSchema> {
            Arc::clone(&self.schema)
        }
    }

    // The catalog itself does not need Clone, Debug, Send, or Sync.
    struct TestCatalog {
        source: Arc<dyn TableSource>,
    }

    impl Catalog for TestCatalog {
        type Error = &'static str;

        fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
            match name {
                "unavailable" => Err("catalog unavailable"),
                "items" => Ok(Some(Arc::clone(&self.source))),
                _ => Ok(None),
            }
        }
    }

    #[test]
    fn binder_uses_a_source_handle_without_catalog_id_parameters() {
        let schema = Arc::new(TableSchema {
            name: "items".into(),
            columns: vec![ColumnSchema {
                name: "id".into(),
                data_type: LogicalType::Int32,
                nullable: false,
            }],
        });
        let catalog = TestCatalog {
            source: Arc::new(TestSource {
                backend_id: 7,
                schema: Arc::clone(&schema),
            }),
        };
        let binder = Binder { catalog: &catalog };
        let source = binder.catalog.get_table("items").unwrap().unwrap();
        assert!(Arc::ptr_eq(&source, &catalog.source));
        assert!(Arc::ptr_eq(&source.schema(), &schema));
        assert!(binder.catalog.get_table("missing").unwrap().is_none());
        assert!(binder.catalog.get_table("unavailable").is_err());

        let relation = bound::RelationId(0);
        let expr: bound::Expr<'_> = bound::Expr {
            data_type: LogicalType::Int32,
            nullable: false,
            kind: bound::ExprKind::Column(bound::ColumnBinding {
                relation,
                column_index: 0,
            }),
        };
        let statement: bound::Statement<'_> = bound::Statement::Select(bound::Select {
            source: Some(bound::Table { relation, source }),
            projection: vec![bound::NamedExpr {
                name: "id".into(),
                expr,
            }],
            filter: None,
            group_by: vec![],
            having: None,
            order_by: vec![],
            is_aggregate: false,
        });
        let bound::Statement::Select(query) = statement.clone() else {
            panic!("expected SELECT")
        };
        let table = query.source.unwrap();
        assert!(Arc::ptr_eq(&table.source, &catalog.source));
        assert!(table.source.is::<TestSource>());
        assert_eq!(
            table
                .source
                .downcast_ref::<TestSource>()
                .unwrap()
                .backend_id,
            7
        );
    }

    #[test]
    fn identical_metadata_does_not_erase_source_identity() {
        let schema = Arc::new(TableSchema {
            name: "items".into(),
            columns: vec![],
        });
        let source = |backend_id| -> Arc<dyn TableSource> {
            Arc::new(TestSource {
                backend_id,
                schema: Arc::clone(&schema),
            })
        };
        let a = source(1);
        let b = source(2);
        assert!(Arc::ptr_eq(&a.schema(), &b.schema()));
        assert!(!Arc::ptr_eq(&a, &b));
        assert_ne!(
            a.downcast_ref::<TestSource>().unwrap().backend_id,
            b.downcast_ref::<TestSource>().unwrap().backend_id,
        );
    }
}
