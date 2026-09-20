//! ChiliDB: a small OLTP database with independently replaceable layers.
//!
//! The SQL frontend produces an unbound AST without catalog lookup, type
//! checking, planning, or execution.
//!
//! ```
//! let statements = chilidb::parser::parse_sql("SELECT 1 + 2")?;
//! assert_eq!(statements.len(), 1);
//! # Ok::<(), chilidb::parser::ParseError>(())
//! ```

/// Shared physical tuple identifiers.
pub use chilidb_common as common;

/// Catalog metadata and bound SQL representations.
pub use chilidb_binder as binder;

/// SQL grammar, borrowed AST types, and parsing entry points.
pub use chilidb_parser as parser;

/// Logical planning, output schemas, and command plans.
pub use chilidb_planner as planner;
