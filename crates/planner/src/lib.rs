#![doc = include_str!("../README.md")]

pub mod modify_table;
pub mod plan;
pub mod planning;
pub mod table_source;

pub use chilidb_common::Ctid;
pub use datafusion_expr::{Expr, LogicalPlan};
pub use modify_table::{Modification, ModifyTable, Target, UpdateAssignment};
pub use plan::{Command, PlannedStatement};
pub use planning::Planner;

pub use chilidb_binder::{LogicalType, Scalar};

use std::borrow::Cow;

use chilidb_binder::bound::ColumnBinding;
use datafusion_common::DataFusionError;

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("invalid bound statement: {0}")]
    InvalidBound(&'static str),
    #[error("column is unavailable in this evaluation phase: {0:?}")]
    UnknownColumn(ColumnBinding),
    #[error("expression nesting exceeds 256 nodes")]
    ExpressionTooDeep,
    #[error(transparent)]
    DataFusion(#[from] DataFusionError),
}

/// SQL-visible root query output, in DataFusion output-column order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputSchema<'sql> {
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
