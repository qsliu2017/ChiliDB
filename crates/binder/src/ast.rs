//! Resolved query structure, without logical or physical plan operators.

use std::borrow::Cow;
use std::sync::Arc;

use crate::{LogicalType, Scalar, TableSource};

/// Operator symbols shared with the SQL syntax tree.
pub use chilidb_parser::{BinaryOp, UnaryOp};

/// Identity of one relation occurrence within a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RelationId(pub u32);

/// A resolved column reference, independent of execution-slot assignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ColumnBinding {
    /// The query-local relation containing the column.
    pub relation: RelationId,
    /// Zero-based position in the relation source's schema, not a physical slot.
    pub column_index: usize,
}

/// A table occurrence and its shared source handle.
#[derive(Clone, Debug)]
pub struct Table {
    /// Query-local identity of this table occurrence.
    pub relation: RelationId,
    /// The source exposing the table schema.
    pub source: Arc<dyn TableSource>,
}

/// A semantically resolved statement.
#[derive(Clone, Debug)]
pub enum Statement<'sql> {
    /// A projection with an optional source and filter.
    Select(Select<'sql>),
    /// A table declaration, without catalog mutation.
    CreateTable(CreateTable<'sql>),
    /// Rows converted to the target table's schema order.
    Insert(Insert<'sql>),
    /// Assignments evaluated against the original target row.
    Update(Update<'sql>),
    /// Rows selected for deletion.
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
    /// The declared logical type.
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
    /// The target table handle.
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
    /// The optional source table occurrence.
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
    /// The typed expression producing this column.
    pub expr: Expr<'sql>,
}

/// A typed expression with resolved column references and explicit casts.
#[derive(Clone, Debug, PartialEq)]
pub struct Expr<'sql> {
    /// The expression's result type.
    pub data_type: LogicalType,
    /// Whether the expression may produce NULL.
    pub nullable: bool,
    /// The operation or value represented by the expression.
    pub kind: ExprKind<'sql>,
}

/// The operation or value represented by a bound expression.
#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind<'sql> {
    /// A literal value.
    Literal(Scalar<'sql>),
    /// A column resolved within the query's relation scope.
    Column(ColumnBinding),
    /// A prefix operation.
    Unary {
        /// The operator symbol.
        op: UnaryOp,
        /// The typed operand.
        expr: Box<Expr<'sql>>,
    },
    /// A binary operation.
    Binary {
        /// The operator symbol.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr<'sql>>,
        /// The right operand.
        right: Box<Expr<'sql>>,
    },
    /// A conversion to the enclosing expression's result type.
    Cast {
        /// The expression before conversion.
        expr: Box<Expr<'sql>>,
    },
    /// An IS NULL or IS NOT NULL predicate.
    IsNull {
        /// The tested expression.
        expr: Box<Expr<'sql>>,
        /// Whether the predicate contains NOT.
        negated: bool,
    },
}
