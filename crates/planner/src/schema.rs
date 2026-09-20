//! SQL-visible query outputs, separate from DataFusion's internal column names.

use std::borrow::Cow;

use chilidb_binder::{LogicalType, bound::ColumnBinding};

/// Root query output metadata, in DataFusion output-column order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Schema<'sql> {
    /// Visible output columns in positional order; duplicate names are allowed.
    pub fields: Vec<Field<'sql>>,
}

/// The name, type, and optional source identity of an output column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field<'sql> {
    pub name: Cow<'sql, str>,
    pub data_type: LogicalType,
    pub nullable: bool,
    /// The bound column for a direct pass-through, or None for computed values.
    ///
    /// This describes lineage, not the column's position in this output schema.
    pub origin: Option<ColumnBinding>,
}
