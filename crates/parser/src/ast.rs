//! SQL syntax without name binding, type checking, or source spans.
//!
//! Text borrows the SQL input unless identifier folding or quote unescaping is needed.
use std::borrow::Cow;

/// An unbound SQL statement whose text borrows the input, not the parse tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Statement<'sql> {
    /// A projection with an optional source table and predicate.
    Select {
        /// Expressions in source order, or a single [`Expr::Wildcard`].
        projection: Vec<Expr<'sql>>,
        /// The optional, unqualified source table name.
        from: Option<Cow<'sql, str>>,
        /// The optional `WHERE` expression.
        filter: Option<Expr<'sql>>,
        /// Grouping expressions in source order.
        group_by: Vec<Expr<'sql>>,
        /// Predicate applied to grouped results.
        having: Option<Expr<'sql>>,
        /// Result ordering in source order.
        order_by: Vec<OrderByExpr<'sql>>,
    },
    /// A table declaration with column-level constraints.
    CreateTable {
        /// The unqualified table name.
        name: Cow<'sql, str>,
        /// Column definitions in source order.
        columns: Vec<ColumnDef<'sql>>,
    },
    /// An `INSERT INTO ... VALUES` statement.
    Insert {
        /// The unqualified target table name.
        table: Cow<'sql, str>,
        /// Target column names; empty when the column list is omitted.
        columns: Vec<Cow<'sql, str>>,
        /// Value expressions grouped by row, without row-width validation.
        rows: Vec<Vec<Expr<'sql>>>,
    },
    /// An `UPDATE ... SET` statement.
    Update {
        /// The unqualified target table name.
        table: Cow<'sql, str>,
        /// Assignments in source order.
        assignments: Vec<Assignment<'sql>>,
        /// The optional `WHERE` expression.
        filter: Option<Expr<'sql>>,
    },
    /// A `DELETE FROM` statement.
    Delete {
        /// The unqualified target table name.
        table: Cow<'sql, str>,
        /// The optional `WHERE` expression.
        filter: Option<Expr<'sql>>,
    },
    Begin,
    Commit,
    Rollback,
}

/// A column declaration in a `CREATE TABLE` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef<'sql> {
    /// The column name, folded if bare or unescaped if quoted.
    pub name: Cow<'sql, str>,
    pub data_type: DataType<'sql>,
    /// Constraints in source order, without consistency or duplicate checks.
    pub constraints: Vec<ColumnConstraint>,
}

/// A SQL type declaration, without runtime representation or validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataType<'sql> {
    /// `INT` or `INTEGER`.
    Integer,
    BigInt,
    Text,
    /// `BOOL` or `BOOLEAN`.
    Boolean,
    Real,
    Double,
    /// `VARCHAR` with an optional decimal length, preserved without range checks.
    Varchar(
        /// The original length digits, or `None` when omitted.
        Option<&'sql str>,
    ),
}

/// A column-level constraint declaration; parsing does not enforce constraints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnConstraint {
    NotNull,
    PrimaryKey,
    Unique,
}

/// A column assignment in an `UPDATE` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment<'sql> {
    /// The unqualified target column name.
    pub column: Cow<'sql, str>,
    pub value: Expr<'sql>,
}

/// An untyped expression with operator grouping represented by the tree.
///
/// Precedence, from low to high: `OR`, `AND`, prefix `NOT`, comparison or
/// `IS [NOT] NULL`, additive, multiplicative, prefix signs, and primary expressions.
/// Binary arithmetic and boolean operators associate left; prefix operators
/// associate right. Comparisons do not chain. Parentheses affect grouping but
/// have no separate AST node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr<'sql> {
    /// An unqualified name, ASCII-folded if bare and case-preserving if quoted.
    Identifier(Cow<'sql, str>),
    /// A two-part `table.column` name, without name resolution.
    QualifiedIdentifier {
        table: Cow<'sql, str>,
        column: Cow<'sql, str>,
    },
    /// An explicit projection alias.
    Alias {
        expr: Box<Expr<'sql>>,
        alias: Cow<'sql, str>,
    },
    /// An unresolved function call; argument validity is checked by binding.
    FunctionCall {
        /// Unqualified function name.
        name: Cow<'sql, str>,
        /// Arguments, including a standalone wildcard when written.
        args: Vec<Expr<'sql>>,
        distinct: bool,
        /// Optional aggregate input predicate.
        filter: Option<Box<Expr<'sql>>>,
        /// Optional inline window specification.
        over: Option<Box<WindowSpec<'sql>>>,
    },
    /// `*`, accepted as the complete projection or standalone call argument.
    Wildcard,
    Literal(Literal<'sql>),
    Unary {
        op: UnaryOp,
        expr: Box<Expr<'sql>>,
    },
    Binary {
        left: Box<Expr<'sql>>,
        op: BinaryOp,
        right: Box<Expr<'sql>>,
    },
    /// An `IS NULL` or `IS NOT NULL` predicate.
    IsNull {
        expr: Box<Expr<'sql>>,
        /// Whether the predicate contains `NOT`.
        negated: bool,
    },
}

/// A SQL literal; numeric spelling is preserved rather than converted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal<'sql> {
    /// A decimal or exponent-form number; a leading sign is a unary operator.
    Number(&'sql str),
    /// A single-quoted string with delimiters removed and doubled quotes unescaped.
    String(
        /// Borrowed text unless unescaping requires allocation.
        Cow<'sql, str>,
    ),
    Null,
    Boolean(bool),
}

/// A prefix operator; see [`Expr`] for precedence and associativity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Plus,
    Minus,
}

/// A binary operator; see [`Expr`] for precedence and associativity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    /// Inequality (`<>` or `!=`).
    NotEq,
    Less,
    LessEq,
    Greater,
    GreaterEq,
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}

/// One ordering expression; omitted direction defaults to ascending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderByExpr<'sql> {
    pub expr: Expr<'sql>,
    pub direction: SortDirection,
    pub nulls: Option<NullOrder>,
}
/// Sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}
/// Explicit null placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NullOrder {
    First,
    Last,
}
/// An inline window specification, without implicit frame interpretation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowSpec<'sql> {
    pub partition_by: Vec<Expr<'sql>>,
    pub order_by: Vec<OrderByExpr<'sql>>,
    pub frame: Option<WindowFrame<'sql>>,
}
/// An explicit frame; short forms end at CURRENT ROW.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowFrame<'sql> {
    pub units: FrameUnits,
    /// Inclusive start bound.
    pub start: FrameBound<'sql>,
    /// Inclusive end bound.
    pub end: FrameBound<'sql>,
}
/// Frame measurement units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameUnits {
    /// Physical rows.
    Rows,
    /// Ordering range.
    Range,
}
/// Frame bound; offsets retain unsigned decimal spelling without range checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameBound<'sql> {
    UnboundedPreceding,
    Preceding(&'sql str),
    CurrentRow,
    Following(&'sql str),
    UnboundedFollowing,
}
