#![doc = include_str!("../README.md")]

pub mod ast;
pub mod catalog;
pub mod context;
pub mod error;
pub mod types;

mod aggregate;
mod binding;
mod ddl;
mod dml;
mod expression;

pub use ast as bound;
pub use catalog::*;
pub use context::*;
pub use error::BindError;
pub use types::*;

/// A binder borrowing a read-only catalog.
///
/// Query-local state lives separately in [`BindContext`]. The catalog
/// implementation is generic; contexts and bound ASTs retain table-source handles.
pub struct Binder<'catalog, C: Catalog + ?Sized> {
    /// The schema view used for name resolution.
    pub catalog: &'catalog C,
}
