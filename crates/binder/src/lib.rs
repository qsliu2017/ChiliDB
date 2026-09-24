#![doc = include_str!("../README.md")]

pub mod ast;
pub mod catalog;
pub mod context;

mod aggregate;
mod binding;
mod ddl;
mod dml;
mod expression;

pub use ast as bound;
pub use catalog::*;
pub use context::*;

use std::borrow::Cow;
use std::num::NonZeroU32;

/// A binder borrowing a read-only catalog.
///
/// Query-local state lives separately in [`BindContext`]. The catalog
/// implementation is generic; contexts and bound ASTs retain table-source handles.
pub struct Binder<'catalog, C: Catalog + ?Sized> {
    /// The schema view used for name resolution.
    pub catalog: &'catalog C,
}

/// The logical type of an expression or catalog column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogicalType {
    /// A NULL literal whose concrete type is not determined by context.
    Null,
    /// A boolean value.
    Boolean,
    /// A signed 32-bit integer.
    Int32,
    /// A signed 64-bit integer.
    Int64,
    /// An unsigned 32-bit integer.
    Uint32,
    /// A 32-bit floating-point value.
    Float32,
    /// A 64-bit floating-point value.
    Float64,
    /// A string without a declared length bound.
    Text,
    /// A string with an optional positive character limit.
    Varchar(Option<NonZeroU32>),
}

/// A literal value; string text can borrow the SQL input.
#[derive(Clone, Debug, PartialEq)]
pub enum Scalar<'sql> {
    /// SQL NULL; its contextual type belongs to the enclosing expression.
    Null,
    /// A boolean value.
    Boolean(bool),
    /// A signed 32-bit integer.
    Int32(i32),
    /// A signed 64-bit integer.
    Int64(i64),
    /// An unsigned 32-bit integer.
    Uint32(u32),
    /// A 32-bit floating-point value.
    Float32(f32),
    /// A 64-bit floating-point value.
    Float64(f64),
    /// String contents without SQL quoting.
    String(Cow<'sql, str>),
}

/// A catalog failure or semantic error in an SQL statement.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum BindError<E> {
    /// No supported built-in has this exact normalized name.
    #[error("unknown function: {0}")]
    UnknownFunction(String),
    /// Invalid aggregate or window context or call signature.
    #[error("invalid function call: {0}")]
    InvalidFunction(&'static str),
    /// A column is not determined by grouping keys.
    #[error("expression references a column outside GROUP BY")]
    UngroupedColumn,
    /// An invalid or unsupported frame specification.
    #[error("invalid or unsupported window frame: {0}")]
    InvalidWindowFrame(&'static str),

    /// The catalog could not perform the lookup.
    #[error("catalog lookup failed: {0}")]
    Catalog(#[source] E),
    /// A table declaration conflicts with an existing catalog entry.
    #[error("table already exists: {0}")]
    TableAlreadyExists(String),
    /// A declaration or target list repeats a column name.
    #[error("duplicate column: {0}")]
    DuplicateColumn(String),
    /// A constraint declaration is duplicated or conflicts with another declaration.
    #[error("invalid constraint: {0}")]
    InvalidConstraint(String),
    /// A declared type parameter is invalid.
    #[error("invalid type declaration: {0}")]
    InvalidType(String),
    /// A statement lacks a required list or name in a manually constructed AST.
    #[error("invalid statement: {0}")]
    InvalidStatement(&'static str),
    /// An INSERT row has a different width than its target column list.
    #[error("INSERT row {row} has {actual} values; expected {expected}")]
    RowWidth {
        /// One-based row number.
        row: usize,
        /// Required number of values.
        expected: usize,
        /// Supplied number of values.
        actual: usize,
    },
    /// No table has this normalized name.
    #[error("unknown table: {0}")]
    UnknownTable(String),
    /// No visible column matches the requested name.
    #[error("unknown column {name:?} (qualifier: {qualifier:?})")]
    UnknownColumn {
        /// Optional relation qualifier.
        qualifier: Option<String>,
        /// Requested column name.
        name: String,
    },
    /// More than one visible column matches the requested name.
    #[error("ambiguous column {name:?} (qualifier: {qualifier:?})")]
    AmbiguousColumn {
        /// Optional relation qualifier.
        qualifier: Option<String>,
        /// Requested column name.
        name: String,
    },
    /// An operand has a type unsuitable for its expression context.
    #[error("{context} requires {expected}, got {actual:?}")]
    TypeMismatch {
        /// The clause or operation requiring a type.
        context: &'static str,
        /// A description of the acceptable types.
        expected: &'static str,
        /// The supplied type.
        actual: LogicalType,
    },
    /// Operands have no supported common type.
    #[error("incompatible types for {context}: {left:?} and {right:?}")]
    IncompatibleTypes {
        /// The operation requiring compatible operands.
        context: &'static str,
        /// Left operand type.
        left: LogicalType,
        /// Right operand type.
        right: LogicalType,
    },
    /// A numeric token is malformed or outside the supported finite range.
    #[error("invalid or out-of-range number: {0}")]
    InvalidNumber(String),
    /// A wildcard occurs outside a complete SELECT projection or without a source.
    #[error("wildcard requires a source and must be the complete projection")]
    InvalidWildcard,
    /// A manually constructed SELECT contains no output expressions.
    #[error("SELECT requires at least one output expression")]
    EmptyProjection,
    /// Expression nesting exceeds the binder's recursion limit.
    #[error("expression nesting exceeds 256 levels")]
    ExpressionTooDeep,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn catalog_error_preserves_its_source() {
        let error = BindError::Catalog(std::io::Error::other("metadata unavailable"));
        assert_eq!(
            error.to_string(),
            "catalog lookup failed: metadata unavailable"
        );
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .is_some()
        );
        let semantic = BindError::<std::io::Error>::UnknownTable("missing".into());
        assert!(semantic.source().is_none());
    }
}
