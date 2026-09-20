#![doc = include_str!("../README.md")]

pub mod ast;
pub mod grammar;
mod transform;

pub use ast::*;
use grammar::Node;
pub use grammar::ParseError;

/// Conversion from a named PEG node to an AST value borrowing SQL, not the tree.
///
/// Implemented for statements, expressions, column definitions, assignments,
/// data types, constraints, and literals. Import this trait to call `parse`.
/// Statement and expression conversion accept either their grammar wrapper or
/// a concrete node of the corresponding kind.
///
/// ```
/// use chilidb_parser::{grammar::Node, Expr, Literal, ParseNode};
/// let sql = String::from("001");
/// let node = Node::new("Number", &sql, 0..sql.len(), vec![])?;
/// let expr = Expr::parse(&node)?;
/// drop(node);
/// assert_eq!(expr, Expr::Literal(Literal::Number("001")));
/// # Ok::<(), chilidb_parser::ParseError>(())
/// ```
///
/// The SQL input must outlive the result:
///
/// ```compile_fail
/// use chilidb_parser::{grammar::Node, Expr, ParseNode};
/// let expr = {
///     let sql = String::from("001");
///     let node = Node::new("Number", &sql, 0..sql.len(), vec![]).unwrap();
///     Expr::parse(&node).unwrap()
/// };
/// println!("{expr:?}");
/// ```
pub trait ParseNode<'sql>: Sized {
    /// Converts a node without reparsing its source text.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] for an unexpected rule, malformed tree shape, or
    /// invalid leaf token. Silent punctuation and trivia in composite nodes
    /// are not generally revalidated; use [`parse_sql`] to validate SQL text.
    fn parse(node: &Node<'sql>) -> Result<Self, ParseError>;
}

/// Parses zero or more semicolon-separated statements, consuming the entire input.
///
/// One final semicolon is optional; empty or comment-only input yields an empty
/// vector. AST text borrows `input`, except when name folding or quote unescaping
/// requires allocation. Names remain unbound and numbers retain their spelling.
/// No statements are executed.
///
/// # Errors
///
/// Returns [`ParseError`] with a UTF-8 byte offset and expected grammar items for
/// invalid syntax or exceeded PEG recursion/work limits.
pub fn parse_sql(input: &str) -> Result<Vec<Statement<'_>>, ParseError> {
    let root = grammar::parse(input)?;
    transform::statements(&root)
}
