//! Errors produced by SQL name resolution and type checking.

use crate::LogicalType;

/// A catalog failure or semantic error in an SQL statement.
#[derive(Debug, PartialEq, thiserror::Error)]
pub enum BindError<E> {
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
