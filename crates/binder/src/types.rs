//! SQL types and literal values, independent of storage representation.

use std::borrow::Cow;
use std::num::NonZeroU32;

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
