# chilidb-parser

A SQL parser and borrowed abstract syntax tree.
`parse_sql(&str) -> Result<Vec<Statement<'_>>, ParseError>` consumes all input. The
public `ast` types are independent of PEG. `grammar::parse` exposes a named
`Node<'sql>` tree with borrowed text and UTF-8 byte spans. A separate transformer
converts that tree to the AST.

```rust
use chilidb_parser::parse_sql;
let statements = parse_sql("BEGIN; INSERT INTO items (id) VALUES (1); COMMIT;")?;
# Ok::<(), chilidb_parser::ParseError>(())
```

## Lifetimes and allocations

`Statement<'sql>` and its nested AST types borrow the SQL input, not the temporary
PEG tree. Keep the input alive while using the AST; the tree is dropped before
`parse_sql` returns. Numbers and VARCHAR lengths are `&'sql str` slices retaining
exact spelling. Identifiers and SQL string values are `Cow<'sql, str>`: lowercase
bare names and quoted text without escapes borrow input (without quote delimiters).
Uppercase bare names allocate for ASCII lowercase folding; doubled quotes allocate
for unescaping. Quoted identifiers preserve case and UTF-8. This avoids text copies,
not all allocations: tree nodes, AST vectors, and boxed expressions still allocate.

```rust
use std::borrow::Cow;
use chilidb_parser::{parse_sql, Expr, Literal, Statement};
let sql = String::from("SELECT item, '世界', 001 FROM Items");
let ast = parse_sql(&sql)?;
let Statement::Select { projection, from, .. } = &ast[0] else { unreachable!() };
assert!(matches!(&projection[0], Expr::Identifier(Cow::Borrowed("item"))));
assert!(matches!(&projection[1], Expr::Literal(Literal::String(Cow::Borrowed("世界")))));
assert!(matches!(&projection[2], Expr::Literal(Literal::Number("001"))));
assert!(matches!(from, Some(Cow::Owned(name)) if name == "items"));
# Ok::<(), chilidb_parser::ParseError>(())
```

## Supported SQL

- `SELECT expr [AS alias], ... [FROM table] [WHERE expr] [GROUP BY expr, ...]
  [HAVING expr] [ORDER BY expr [ASC|DESC] [NULLS FIRST|LAST], ...]`, or `SELECT * ...`.
- `CREATE TABLE name (column type [constraint ...], ...)`.
  Column constraints: NOT NULL, PRIMARY KEY, UNIQUE, in any order, including repeats.
  Types: INT/INTEGER, BIGINT, TEXT, BOOL/BOOLEAN, REAL, DOUBLE, VARCHAR[(length)].
- `INSERT INTO table [(column, ...)] VALUES (expr, ...), ...`.
- `UPDATE table SET column = expr, ... [WHERE expr]`.
- `DELETE FROM table [WHERE expr]`.
- `BEGIN`, `COMMIT`, `ROLLBACK`.

Expressions, from low to high precedence: OR, AND, prefix NOT, one comparison
(`=`, `<>`, `!=`, `<`, `<=`, `>`, `>=`, `IS [NOT] NULL`), `+ -`, `* / %`,
prefix `+ -`, parentheses/literals/names. Arithmetic and boolean binary operators
associate left; prefix operators associate right. Names can be `table.column`.
Generic function calls accept empty arguments, expression lists, or a standalone
`*`, optionally preceded by DISTINCT. Calls may have `FILTER (WHERE expr)` followed
by `OVER ([PARTITION BY expr, ...] [ORDER BY ...] [frame])`. Function names are
ordinary identifiers; binding validates function names and arguments. Wildcard is
otherwise allowed only as the complete SELECT projection. Aliases require explicit
AS and are restricted to projection items.

Frames use ROWS or RANGE with a bound or BETWEEN bound AND bound. Bounds are
UNBOUNDED PRECEDING/FOLLOWING, CURRENT ROW, or unsigned decimal offsets followed
by PRECEDING/FOLLOWING. Offset spelling borrows input. Short frames normalize the
end to CURRENT ROW; omitted frames remain absent for binding to interpret. Ordering
defaults to ascending, while omitted null placement remains absent.

Numbers retain exact spelling (including decimal/exponent forms); signs are
unary operators, not part of numeric literals. Strings use single quotes with
`''` escapes. Quoted identifiers use double quotes with `""` escapes, preserve
case, and accept Unicode. Bare identifiers are ASCII `[a-zA-Z_][a-zA-Z_0-9]*`,
fold to lowercase, and reject reserved syntax keywords. Keywords are ASCII
case insensitive with identifier boundaries. NULL, TRUE, and FALSE are literals.
Backslash has no special meaning in SQL strings or identifiers.

Statements require separating semicolons, with one optional final semicolon.
Empty/comment-only input is valid; empty statements are not. Spaces, tabs,
newlines, `--` line comments and non-nested `/* ... */` comments are trivia.

## Limits

The grammar is not PostgreSQL-compatible: no joins, subqueries, implicit or table aliases,
LIMIT, named windows, GROUPS/EXCLUDE frames, set operations, schema-qualified table names, table
constraints, defaults, parameter placeholders, casts, or nested block comments.
Parsing performs no execution, binding, catalog lookup, type checking, row-width
validation, duplicate-name checking, or constraint consistency checking. VARCHAR
lengths are preserved as strings rather than validated. The PEG engine bounds
recursion depth and parsing work; exceeding either limit returns a parse error.
Errors expose a byte offset and expected grammar items; the AST does not retain
spans.

## Converting an inspected tree

The public `ParseNode<'sql>` trait converts a borrowed `&Node<'sql>` into an AST
value with `parse`, returning `ParseError` on invalid nodes. `Statement::parse`
accepts either a `Statement` wrapper or a concrete statement node. `Expr::parse`
similarly accepts an `Expr` wrapper or a concrete expression node. Column
definitions, assignments, data types, constraints, literals, ordering types, window specifications, and frame types also implement
`ParseNode`. Import the trait to call these methods.

```rust
use chilidb_parser::{grammar, ParseNode, Statement};
let sql = String::from("SELECT item FROM items");
let statement = {
    let tree = grammar::parse(&sql)?;
    let statement = Statement::parse(&tree.children()[0])?;
    drop(tree);
    statement
}; // The AST continues borrowing sql.
assert!(matches!(statement, Statement::Select { .. }));
# Ok::<(), chilidb_parser::ParseError>(())
```

Nodes expose inherent `rule()`, `text()`, `span()`, and `children()` accessors.
`text()` borrows the matched slice of the original input. Their private
fields and checked `Node::new` constructor enforce valid UTF-8 spans, ordered
children contained within their parent, and matching source slices. Both nodes and
ASTs borrow SQL, not each other; conversion needs no separate input argument.
Conversion checks tree shapes and leaf tokens and returns `ParseError` for malformed
nodes. It does not reparse SQL: silent punctuation and trivia in composite nodes
are not generally revalidated. Use `parse_sql` to validate SQL text.
