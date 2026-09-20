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
