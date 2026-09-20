# chilidb-binder

SQL name resolution and type checking over a storage-independent catalog.
The output is a bound AST, not a logical plan or an executed result.

```rust
use std::{convert::Infallible, sync::Arc};
use chilidb_binder::{Binder, Catalog, TableSource};

struct EmptyCatalog;
impl Catalog for EmptyCatalog {
    type Error = Infallible;
    fn get_table(&self, _: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
        Ok(None)
    }
}

let parsed = chilidb_parser::parse_sql("SELECT 1 + 2.5 WHERE true")?;
let result = Binder::new(&EmptyCatalog).bind(&parsed[0])?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

For a catalog-backed example, run from the workspace root:

```sh
cargo run --example bind -- 'SELECT id + 1 FROM items WHERE id > 0'
```

## Statements

`Binder::bind` accepts every statement in the parser's SQL subset and creates a
fresh scope per call. It performs no catalog mutations or transaction actions.

| Statement | Bound representation |
| --- | --- |
| SELECT | Resolved columns, expanded `*`, typed projections and WHERE |
| CREATE TABLE | Typed column declarations and indexed key constraints |
| INSERT | Typed rows in full target-schema order |
| UPDATE | Target-column indices, typed assignments, and optional WHERE |
| DELETE | Target source and optional boolean WHERE |
| BEGIN / COMMIT / ROLLBACK | Transaction command, without state validation |

SELECT column outputs retain their names; other expressions use `?column?`.
Wildcards require a source and must be the complete SELECT projection.

CREATE TABLE rejects existing tables, duplicate names or constraints, multiple
primary-key declarations, and invalid VARCHAR lengths. Primary-key columns are
non-nullable. INTEGER/BIGINT/REAL/DOUBLE declarations map to Int32/Int64/Float32/
Float64. VARCHAR lengths must be positive and fit `u32`.

INSERT validates target-column names and each row's width. VALUES expressions
have no target-table scope. Explicit column lists are reordered into full schema
order; omitted columns receive typed NULLs. UPDATE expressions all read the
original target row, not preceding assignments. Duplicate assignment targets
are rejected. UPDATE and DELETE predicates require Boolean.

Missing names, ambiguity, invalid operands or declarations, and catalog failures
produce structured `BindError` values. Errors use `thiserror` and preserve the
catalog error as their source when it implements `std::error::Error`. Expression
nesting is limited to 256 levels; exceeding it returns `ExpressionTooDeep`.

Binding does not enforce NOT NULL, UNIQUE, PRIMARY KEY, runtime conversion ranges,
or VARCHAR value lengths. These remain execution/storage checks, including NULLs
introduced by omitted INSERT columns. Catalog-dependent statements must be bound
against the appropriate schema view: binding CREATE TABLE does not make the table
visible to a subsequent INSERT. Executing commands in order is a caller concern.

## Types and coercion

Numeric types use explicit widths: `Int32`, `Int64`, `Uint32`, `Float32`, and
`Float64`. Integer literals infer `Int32` when representable, otherwise `Int64`.
Decimal/exponent literals infer finite `Float64`. Signed minima are supported;
malformed or out-of-range literals are errors. `Uint32` and `Float32` can come
from catalog columns.

| Operands | Common numeric type |
| --- | --- |
| Same numeric type | That type |
| `Int32` with `Uint32`, or integer pairs involving `Int64` | `Int64` |
| `Float32` with an integer, or any pair involving `Float64` | `Float64` |
| NULL with a numeric type | The numeric type |
| NULL with NULL in arithmetic | `Int32` |

Assignment coercion permits numeric-to-numeric and string-to-string conversions,
including explicit narrowing casts; NULL can take the target type. Runtime
conversion and range checks are not performed by the binder. Boolean/string/
numeric cross-family assignments are rejected.

Arithmetic requires numbers; modulo requires integers. Unary minus promotes
`Uint32` to `Int64`. Numeric comparisons use the same common-type rules; boolean
pairs compare as booleans and string pairs as `Text`. No implicit string-to-number
or boolean-to-number conversions are performed. Numeric-to-float coercion can
lose precision; it is not an exact-decimal arithmetic guarantee.

`AND`, `OR`, `NOT`, and WHERE require Boolean, with NULL cast to Boolean when
needed. Other NULL operands are cast to their contextual type; comparing two
NULLs uses Text. A standalone NULL retains `LogicalType::Null`. Type changes
appear as explicit `Cast` expressions. Binary nullability conservatively combines
operand nullability; `IS [NOT] NULL` is non-nullable Boolean. Binding does not
evaluate arithmetic, detect division by zero, or enforce runtime overflow rules.

## Catalog and scope interfaces

`Binder<'catalog, C>` borrows a `Catalog`. Its `get_table(name)` returns
`Result<Option<Arc<dyn TableSource>>, C::Error>`, distinguishing absence from
lookup failure. Sources expose immutable schemas without storage operations and
retain concrete backend identity for downstream adapters through `downcast_ref`.
Their schema and column ordering must remain stable for the bound query.

`BindContext` holds scopes outermost to innermost. Unqualified column lookup
uses the nearest scope with a match; qualified lookup stops at the nearest scope
containing the qualifier. Duplicate matches are ambiguous. Each relation has a
query-local `RelationId`; columns use zero-based source-schema indices, not
physical tuple offsets. The SQL parser exposes one source table and no aliases
or nested queries; scope lookup also supports manually assembled nested contexts.

## Ownership

`bound::Statement<'sql>` and `bound::Expr<'sql>` have no catalog type parameters.
They retain table-source handles, so the catalog and binder can be dropped.
The AST borrows SQL text, not the parsed AST. Borrowed strings remain borrowed;
already-owned parser strings are cloned when retained in the bound AST. Wildcard
output names are copied from catalog metadata. Tree structures allocate vectors
and boxes. Table-containing structures do not define equality for opaque sources.
