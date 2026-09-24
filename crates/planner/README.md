# chilidb-planner

Converts ChiliDB bound statements into native DataFusion logical plans. Parsing
and binding remain independent; planning does not execute SQL or select physical
algorithms. DataFusion 55.1.0 is pinned consistently across dependencies, requiring
Rust 1.94 or later.

```rust
use chilidb_binder::bound;
use chilidb_planner::{Command, PlannedStatement, Planner};

let plan = Planner::new().plan(&bound::Statement::Begin)?;
assert!(matches!(plan, PlannedStatement::Command(Command::Begin)));
# Ok::<(), chilidb_planner::PlanError>(())
```

From the workspace root:

```sh
cargo run --example plan -- 'SELECT SUM(id), COUNT(*) FROM items'
```

## Statement boundary

`Planner::plan(&bound::Statement<'sql>)` returns a `PlannedStatement<'sql>`:

| Variant | Contract |
| --- | --- |
| `Query { plan, output }` | Native DataFusion plan plus SQL-visible result metadata |
| `ModifyTable { plan }` | A root ModifyTable extension with relational input |
| `Command` | CREATE TABLE or transaction command, outside relational optimization |

DataFusion plans own their strings and expressions. Query output names and CREATE
TABLE declarations may borrow SQL, but no result borrows the bound AST or binder.
The source adapter retains the original table handle and logical catalog metadata.

`OutputSchema` preserves SQL-visible names, logical types, nullability, and direct
column lineage. Its fields correspond to root output columns in order. Duplicate
SQL output names are allowed. Optimizer-facing column names are generated and
unique; they are not display names or physical tuple slots.

The `table_source` module maps logical types to Arrow types. Text and VARCHAR share an
Arrow string representation; VARCHAR limits remain in logical metadata and modification
constraints. The source adapter has no storage or execution-provider dependency
and does not advertise predicate pushdown support. Generated relation identities
are query-local; native DataFusion plan equality is not a provider-identity check.
Cross-query caches must also account for catalog/source identity.

## Query planning

Queries use DataFusion TableScan, Filter, Aggregate, Window, Sort, Projection,
Values, and EmptyRelation nodes rather than ChiliDB copies of those operators.
SELECT without FROM starts with a single-row, zero-column relation.

Planning constructs the required phases in order:

```text
source → WHERE → grouping → HAVING → windows → ordering → final projection
```

Aggregate and window results are structurally deduplicated and referenced by
phase-local DataFusion columns. Grouped expressions reference group outputs,
not raw source columns. Window operands can reference plain aggregate results,
but cannot reference other window results in the same evaluation phase.

Sort precedes the final projection so ordering keys need not appear in the SQL
result. Generated aliases distinguish internal results, including repeated
SQL-visible output names.

Bound numeric casts and explicit frame/null-order semantics are retained.
DataFusion ranking functions return UInt64; their results are explicitly cast to
Int64 before ChiliDB expressions consume them. This is a checked cast, not a
wrapping conversion or TRY_CAST. COUNT(*) is represented using a non-null literal
argument. Function implementation handles belong to DataFusion's aggregate and
window libraries.

## ModifyTable extension

`ModifyTable` implements DataFusion's `UserDefinedLogicalNodeCore`. Its
`Modification` enum carries Insert, Update, or Delete payloads, accessible through
`operation()`. The node has private fields, validated `try_insert`, `try_update`,
and `try_delete` constructors, read-only accessors, and `into_plan()`.

All operations share one input and a fixed completion schema: non-nullable Int64
`affected_rows`. Execution must produce a completion even for an empty input;
no modification executor is provided here.

- Insert maps input columns to every target column in target-schema order.
- Update maps computed input columns to distinct target-column indices. A child
  Projection computes all assignments against original values simultaneously.
- Delete consumes a CTID rather than attempting to identify rows by their
  projected SQL values.

Modification scans add an internal `__ctid` column, invisible to SQL binding. Its
non-nullable Arrow `FixedSizeBinary(6)` values encode `chilidb_common::Ctid`:
a relation-local u32 page ID followed by a one-based u16 tuple slot, little-endian.
Required-column hooks retain it during projection rewriting. Query scans do not
request it. A CTID identifies a physical tuple version, not a stable logical row;
it carries no buffer pin or visibility guarantee. Execution must validate its
source, liveness, and concurrent modifications.

Values schemas describe incoming values, not target NOT NULL constraints.
Nullable input to a non-nullable target is permitted during planning; modification
execution must enforce constraints. Target metadata retains VARCHAR lengths and
other logical schema information.

Extension expressions expose value and CTID dependencies as named columns.
Rebuilding validates their arity, types, and target mappings. Relational inputs
reject nested ModifyTable nodes and native DataFusion DML, DDL, command, or COPY
nodes, including inside subqueries. Equality and hashing include the operation
and its payload, retaining target source identity across clones and optimizer
rewrites. Predicate and limit pushdown across ModifyTable are restricted; this
does not prevent optimizing its input.

## Optimizer integration

The planner does not install or run an optimizer pipeline. The separate
[`chilidb-optimizer`](../optimizer/src/lib.rs) crate runs explicit rule lists over
planned statements. `datafusion-optimizer` remains a development dependency of
this planner crate, used to test selected real rules against these plans,
including optimization beneath ModifyTable with an Insert operation.

`PlannedStatement::with_optimized_plan` is the statement-boundary check for
attaching an optimized result. It preserves query result metadata and checks
output-column/type contracts. Optimized queries cannot contain modification or
command nodes. For modifications it permits INSERT only, requiring the original
target, a ModifyTable root with the Insert operation, and the completion schema.
UPDATE and DELETE optimization are rejected until a CTID-preservation policy is
established.

These checks are not a proof that an optimizer rule preserves semantics. Use an
explicit rule allowlist, retain rule errors, and test each rule's effects. In
particular, an empty INSERT input must not erase the effectful root. Future
physical planning must support the output shapes of enabled rules, including
projected scans and empty relations.

See [the DataFusion planner boundary](../../docs/datafusion-planner.md) for the
extension and source contracts. The public DataFusion types allow construction
outside `Planner`; binding still owns SQL semantic validation. Planning guards
against unavailable columns, phase errors, malformed modification inputs, and excessive
bound-expression depth before structural comparisons.
