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
    /// The `BEGIN` transaction command.
    Begin,
    /// The `COMMIT` transaction command.
    Commit,
    /// The `ROLLBACK` transaction command.
    Rollback,
}

/// A column declaration in a `CREATE TABLE` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDef<'sql> {
    /// The column name, folded if bare or unescaped if quoted.
    pub name: Cow<'sql, str>,
    /// The declared SQL type.
    pub data_type: DataType<'sql>,
    /// Constraints in source order, without consistency or duplicate checks.
    pub constraints: Vec<ColumnConstraint>,
}

/// A SQL type declaration, without runtime representation or validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataType<'sql> {
    /// `INT` or `INTEGER`.
    Integer,
    /// `BIGINT`.
    BigInt,
    /// `TEXT`.
    Text,
    /// `BOOL` or `BOOLEAN`.
    Boolean,
    /// `REAL`.
    Real,
    /// `DOUBLE`.
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
    /// `NOT NULL`.
    NotNull,
    /// `PRIMARY KEY`.
    PrimaryKey,
    /// `UNIQUE`.
    Unique,
}

/// A column assignment in an `UPDATE` statement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment<'sql> {
    /// The unqualified target column name.
    pub column: Cow<'sql, str>,
    /// The assigned expression.
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
    Identifier(
        /// The folded or unescaped name.
        Cow<'sql, str>,
    ),
    /// A two-part `table.column` name, without name resolution.
    QualifiedIdentifier {
        /// The table qualifier.
        table: Cow<'sql, str>,
        /// The column name.
        column: Cow<'sql, str>,
    },
    /// `*`, accepted only as the complete `SELECT` projection.
    Wildcard,
    /// A literal value without type inference.
    Literal(
        /// The literal's syntax value.
        Literal<'sql>,
    ),
    /// A prefix operator and its operand.
    Unary {
        /// The prefix operator.
        op: UnaryOp,
        /// The operand.
        expr: Box<Expr<'sql>>,
    },
    /// A binary operator and its operands.
    Binary {
        /// The left operand.
        left: Box<Expr<'sql>>,
        /// The binary operator.
        op: BinaryOp,
        /// The right operand.
        right: Box<Expr<'sql>>,
    },
    /// An `IS NULL` or `IS NOT NULL` predicate.
    IsNull {
        /// The tested expression.
        expr: Box<Expr<'sql>>,
        /// Whether the predicate contains `NOT`.
        negated: bool,
    },
}

/// A SQL literal; numeric spelling is preserved rather than converted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Literal<'sql> {
    /// A decimal or exponent-form number; a leading sign is a unary operator.
    Number(
        /// The original numeric token.
        &'sql str,
    ),
    /// A single-quoted string with delimiters removed and doubled quotes unescaped.
    String(
        /// Borrowed text unless unescaping requires allocation.
        Cow<'sql, str>,
    ),
    /// `NULL`.
    Null,
    /// `TRUE` or `FALSE`.
    Boolean(
        /// Whether the token is `TRUE`.
        bool,
    ),
}

/// A prefix operator; see [`Expr`] for precedence and associativity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// Logical `NOT`.
    Not,
    /// Unary `+`.
    Plus,
    /// Unary `-`.
    Minus,
}

/// A binary operator; see [`Expr`] for precedence and associativity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    /// Logical `OR`.
    Or,
    /// Logical `AND`.
    And,
    /// Equality (`=`).
    Eq,
    /// Inequality (`<>` or `!=`).
    NotEq,
    /// Less than (`<`).
    Less,
    /// Less than or equal (`<=`).
    LessEq,
    /// Greater than (`>`).
    Greater,
    /// Greater than or equal (`>=`).
    GreaterEq,
    /// Addition (`+`).
    Add,
    /// Subtraction (`-`).
    Subtract,
    /// Multiplication (`*`).
    Multiply,
    /// Division (`/`).
    Divide,
    /// Modulo (`%`).
    Modulo,
}
