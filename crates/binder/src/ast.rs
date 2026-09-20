//! Resolved query structure, without logical or physical plan operators.

use std::borrow::Cow;
use std::sync::Arc;

use crate::{LogicalType, Scalar, TableSource};

/// Operator symbols shared with the SQL syntax tree.
pub use chilidb_parser::{BinaryOp, FrameUnits, NullOrder, SortDirection, UnaryOp};

/// Identity of one relation occurrence within a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RelationId(pub u32);

/// A resolved column reference, independent of execution-slot assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ColumnBinding {
    pub relation: RelationId,
    /// Zero-based position in the relation source's schema, not a physical slot.
    pub column_index: usize,
}

/// A table occurrence and its shared source handle.
#[derive(Clone, Debug)]
pub struct Table {
    pub relation: RelationId,
    pub source: Arc<dyn TableSource>,
}

/// A semantically resolved statement.
#[derive(Clone, Debug)]
pub enum Statement<'sql> {
    Select(Select<'sql>),
    /// A table declaration, without catalog mutation.
    CreateTable(CreateTable<'sql>),
    /// Rows converted to the target table's schema order.
    Insert(Insert<'sql>),
    /// Assignments evaluated against the original target row.
    Update(Update<'sql>),
    Delete(Delete<'sql>),
    /// Begin a transaction; state changes belong to execution.
    Begin,
    /// Commit a transaction; state changes belong to execution.
    Commit,
    /// Roll back a transaction; state changes belong to execution.
    Rollback,
}

/// A validated table declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateTable<'sql> {
    /// The normalized table name.
    pub name: Cow<'sql, str>,
    /// Declared columns in schema order.
    pub columns: Vec<CreateColumn<'sql>>,
    /// Constraints expressed in zero-based column indices.
    pub constraints: Vec<TableConstraint>,
}

/// A typed column declaration, independent of a storage format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateColumn<'sql> {
    /// The normalized column name.
    pub name: Cow<'sql, str>,
    pub data_type: LogicalType,
    /// Whether NULL is permitted; primary-key columns are non-nullable.
    pub nullable: bool,
}

/// A table constraint to be enforced by execution and storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TableConstraint {
    /// Primary-key column indices in key order.
    PrimaryKey(Vec<usize>),
    /// Unique-key column indices in key order.
    Unique(Vec<usize>),
}

/// An INSERT with rows normalized to the target schema.
#[derive(Clone, Debug)]
pub struct Insert<'sql> {
    pub table: Table,
    /// Values in full schema order; omitted columns contain typed NULLs.
    pub rows: Vec<Vec<Expr<'sql>>>,
}

/// An UPDATE with target-column indices and typed assignments.
#[derive(Clone, Debug)]
pub struct Update<'sql> {
    /// The target table occurrence visible to expressions.
    pub table: Table,
    /// Assignments in source order, all reading the original row.
    pub assignments: Vec<Assignment<'sql>>,
    /// Optional boolean WHERE predicate.
    pub filter: Option<Expr<'sql>>,
}

/// An assignment to a column in the target schema.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment<'sql> {
    /// Zero-based target-schema index.
    pub column_index: usize,
    /// Expression coerced to the target type.
    pub value: Expr<'sql>,
}

/// A DELETE with an optional typed predicate.
#[derive(Clone, Debug)]
pub struct Delete<'sql> {
    /// The target table occurrence visible to the predicate.
    pub table: Table,
    /// Optional boolean WHERE predicate.
    pub filter: Option<Expr<'sql>>,
}

/// A resolved SELECT query.
#[derive(Clone, Debug)]
pub struct Select<'sql> {
    /// Grouping keys, resolved before aggregate validation.
    pub group_by: Vec<Expr<'sql>>,
    /// Predicate evaluated on groups.
    pub having: Option<Expr<'sql>>,
    pub order_by: Vec<OrderByExpr<'sql>>,
    /// Whether grouping is required (windows alone do not require it).
    pub is_aggregate: bool,
    pub source: Option<Table>,
    /// Output expressions in result-column order; wildcards are expanded.
    pub projection: Vec<NamedExpr<'sql>>,
    /// The optional boolean WHERE predicate.
    pub filter: Option<Expr<'sql>>,
}

/// An output expression and its result-column name.
#[derive(Clone, Debug, PartialEq)]
pub struct NamedExpr<'sql> {
    /// The output name, borrowed or synthesized.
    pub name: Cow<'sql, str>,
    pub expr: Expr<'sql>,
}

/// A typed expression with resolved column references and explicit casts.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr<'sql> {
    pub data_type: LogicalType,
    pub nullable: bool,
    pub kind: ExprKind<'sql>,
}

/// The operation or value represented by a bound expression.
#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind<'sql> {
    Aggregate(Box<AggregateExpr<'sql>>),
    Window(Box<WindowExpr<'sql>>),
    Literal(Scalar<'sql>),
    Column(ColumnBinding),
    Unary {
        op: UnaryOp,
        expr: Box<Expr<'sql>>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr<'sql>>,
        right: Box<Expr<'sql>>,
    },
    /// A conversion to the enclosing expression's result type.
    Cast {
        expr: Box<Expr<'sql>>,
    },
    /// An IS NULL or IS NOT NULL predicate.
    IsNull {
        expr: Box<Expr<'sql>>,
        /// Whether the predicate contains NOT.
        negated: bool,
    },
}

/// Built-in group aggregate operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateFunction {
    /// Count rows for COUNT(*), or non-NULL argument values for COUNT(expr).
    Count,
    /// Sum non-NULL numeric values, widening integers to Int64 and floats to Float64.
    Sum,
    /// Average non-NULL numeric values using Float64.
    Avg,
    /// Minimum non-NULL value, retaining the operand's logical type.
    Min,
    /// Maximum non-NULL value, retaining the operand's logical type.
    Max,
}

/// Built-in window operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowFunction {
    /// Apply an aggregate to each row's window frame.
    Aggregate(AggregateFunction),
    /// One-based row position within a partition.
    RowNumber,
    /// One-based peer rank, leaving gaps after tied rows.
    Rank,
    /// One-based peer rank without gaps.
    DenseRank,
}

/// A group aggregate; an empty argument list denotes COUNT(*).
///
/// Result type and nullability belong to the enclosing [`Expr`].
#[derive(Clone, Debug, PartialEq)]
pub struct AggregateExpr<'sql> {
    pub function: AggregateFunction,
    /// Typed input expressions, with signature coercions applied.
    pub args: Vec<Expr<'sql>>,
    /// Whether duplicate argument values are eliminated before aggregation.
    pub distinct: bool,
    /// Optional Boolean predicate; only TRUE contributes a row.
    pub filter: Option<Box<Expr<'sql>>>,
}

/// A window computation. Ranking functions retain but do not use the frame.
///
/// Result type and nullability belong to the enclosing [`Expr`]. Window inputs
/// can reference plain aggregate results when the SELECT performs grouping.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowExpr<'sql> {
    pub function: WindowFunction,
    /// Typed operands; ranking functions and COUNT(*) have no arguments.
    pub args: Vec<Expr<'sql>>,
    /// Argument deduplication flag; window DISTINCT is rejected by the binder.
    pub distinct: bool,
    /// Optional Boolean predicate for an aggregate window function.
    pub filter: Option<Box<Expr<'sql>>>,
    pub partition_by: Vec<Expr<'sql>>,
    /// Within-partition order, independent of query output order.
    pub order_by: Vec<OrderByExpr<'sql>>,
    /// Validated boundaries, including resolved defaults.
    pub frame: WindowFrame,
}

/// Resolved sort key, including default null placement.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderByExpr<'sql> {
    pub expr: Expr<'sql>,
    pub direction: SortDirection,
    /// Explicit NULL placement: by default LAST for ASC and FIRST for DESC.
    pub nulls: NullOrder,
}

/// Validated frame boundaries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowFrame {
    pub units: FrameUnits,
    /// Inclusive starting boundary.
    pub start: FrameBound,
    /// Inclusive ending boundary.
    pub end: FrameBound,
}

/// Frame boundary; offsets are nonnegative integral row counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameBound {
    /// Beginning of the partition; not a valid frame end.
    UnboundedPreceding,
    /// The given number of rows before the current row, for ROWS frames.
    Preceding(u64),
    /// The current row for ROWS, or its peer boundary for RANGE.
    CurrentRow,
    /// The given number of rows after the current row, for ROWS frames.
    Following(u64),
    /// End of the partition; not a valid frame start.
    UnboundedFollowing,
}
