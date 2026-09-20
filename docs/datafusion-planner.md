# DataFusion planner boundary

ChiliDB keeps its parser and typed bound AST, then lowers relational work into
DataFusion's `LogicalPlan` and `Expr`. ChiliDB's `ModifyTable` extension carries
Insert, Update, and Delete operations; ordinary relational operators use
DataFusion's native variants.

The implementation pins the published DataFusion **55.1.0** crates. Research used
commit [`b4a8c824`](https://github.com/apache/datafusion/tree/b4a8c824b44bffacf681f74d064107a0c1f71fff);
the release dependency is not assumed to be identical to that commit. Physical
planning, storage, transactions, and modification execution remain separate components.

The public API is documented in [chilidb-planner](../crates/planner/README.md).

## Statement and output ownership

`PlannedStatement` separates queries, relational modifications, and nonrelational
commands. CREATE TABLE and transaction commands never enter the relational
optimizer.

A query contains a native DataFusion plan and an `OutputSchema`. The native plan
owns its strings. External output metadata retains SQL-visible names, logical
types, nullability, and optional source lineage, and may borrow SQL text. Neither
representation borrows the bound AST or binder.

DataFusion schemas use generated, unique column names. SQL-visible names may
repeat and are not suitable as optimizer identities. External output fields
correspond to root output columns in order; rewriting must preserve this
correspondence. Stronger inferred non-nullability is acceptable, but changed
result types or weaker nullability are not.

A modification has one root ModifyTable extension and no nested ModifyTable nodes. Its schema
is one non-nullable Int64 field, `affected_rows`. Execution must return one
completion row, including for zero affected rows. This is command completion,
not a SELECT result set.

## Source and type adaptation

The logical `TableAdapter` retains a ChiliDB table source and exposes an Arrow
schema with generated field names. It is not a DataFusion execution provider.
A later physical planner can recover the original source through downcasting.
Provider predicate pushdown is not advertised as supported.

Original logical metadata remains available: mapping Text and VARCHAR to Arrow
strings does not implement VARCHAR length enforcement. The modification target retains
its original schema, while query output metadata retains declared SQL types.

Bound numeric casts are preserved, including Uint32-to-Int64 before SUM.
Aggregate and window operations use DataFusion function handles. Ranking results
are explicitly cast from DataFusion UInt64 to ChiliDB Int64 before consumption;
overflow is an error, not wrapping or NULL. Window bounds and NULL ordering are
translated explicitly rather than reconstructed from DataFusion defaults.

## Modification input contracts

| Modification | Input dependencies |
| --- | --- |
| Insert | One named value column per target column, in target-schema order |
| Update | CTID plus named computed values, each mapped to a distinct target index |
| Delete | CTID for every selected row |

`ModifyTable` has private fields, validated `try_insert`, `try_update`, and
`try_delete` constructors, accessors, and `into_plan()`. Its `operation()` accessor
returns a `Modification` enum; each variant contains only its required payload.
Target indices address the original logical source schema. Named input references
are resolved against the current child schema rather than assuming input positions
survive optimization.

UPDATE computes all assignments in a child Projection against original input
values. Its executor applies a patch, preserving unchanged stored values; one
assignment does not observe another assignment's new value.

Input nullability describes incoming values, not target NOT NULL constraints.
Execution still enforces nullability, VARCHAR limits, uniqueness, ranges, and other
modification constraints. Planning does not silently truncate values or enforce them by
removing incoming rows.

### CTID

`chilidb_common::Ctid` is a relation-local physical tuple-version address:
`page_id: u32` and `slot_id: NonZeroU16`. Pages are zero-based; slots are one-based
tuple-directory entries rather than byte offsets. All u32 page IDs are
representable; actual page/slot existence is a storage check.

Modification-capable scans expose one additional generated column:

```text
Name:       __ctid
Arrow type: FixedSizeBinary(6)
Nullable:   false
Visibility: internal, absent from the binder's SQL-visible schema
```

The payload contains four little-endian page-ID bytes followed by two little-endian
slot-ID bytes. `Ctid::from_le_bytes` rejects slot zero; successful decoding does
not prove liveness. This execution-interchange encoding is not Rust's padded
struct layout, a PostgreSQL ABI, or a heap-page format.

The target relation comes from ModifyTable. CTIDs contain no buffer pin or lock;
execution acquires the appropriate guards and validates visibility/concurrent
modification. UPDATE may create a version at a different CTID, and reclamation
may reuse addresses. Query scans do not request CTIDs, and SQL binding does not
expose a `ctid` system column.

```text
ModifyTable(Update: a <- new_a, ctid <- __ctid)
└─ Projection(__ctid, new_a = old_b + 1)
   └─ Filter(old_a > 0)
      └─ TableScan(target, including __ctid)
```

A CTID's schema is not proof of its provenance. The planner constructs it from
the correct source; optimizer rules must preserve its association with the row.

## Extension hooks

| Hook | Contract |
| --- | --- |
| `inputs()` | Exactly one relational input; no nested ChiliDB modification extensions |
| `expressions()` | Insert values; Update CTID followed by values; Delete CTID. All are named column expressions. |
| `with_exprs_and_inputs(...)` | Rebuild mappings in that order; preserve target and assignment destinations; validate arity, references, and types |
| `check_invariants(...)` | Validate local modification contracts and fixed completion schema |
| `necessary_children_exprs(...)` | Retain every modification dependency, even when no completion output is requested |
| Predicate/limit pushdown hooks | Do not move predicates or limits across the modification operation |

Equality and hashing include the operation discriminant and payload, retaining
target source identity across clones and rebuilding. Providers are not compared structurally, and equal table names alone
do not imply equal targets.

## Why ModifyTable rather than native DML

DataFusion's native `DmlStatement` can represent INSERT, UPDATE, and DELETE, but
it does not expose a CTID or a changed-column mapping. Its documented input
contract matches the target schema, and its projection optimizer treats all DML
input columns as required. DataFusion's default physical planner delegates
modification to table-provider methods rather than consuming our CTID/patch
protocol.

ChiliDB retains a custom ModifyTable so UPDATE consumes CTID plus changed values,
and DELETE consumes CTID only. Required-column hooks preserve exactly those
inputs. INSERT uses full target-column mappings, sharing the same extension and
validation machinery. Full-row UPDATE inputs are possible, but would be a
different contract; borrowing tuple views can reduce their cost but does not make
them equivalent to a patch protocol.

This choice is independent of row-versus-column execution. Either a native DML
node or an extension can be lowered into a custom executor. Heap CREATE TABLE
remains a ChiliDB command rather than being labeled as DataFusion
CreateMemoryTable.

## Optimization boundary

An extension is not an automatic barrier around its input. Optimizing INSERT's
source is useful: expression simplification and projection cleanup can apply
without an INSERT-specific rewrite. `ModifyTable(Insert, empty input)` must retain
the ModifyTable root and Insert operation; it still has completion semantics.

`PlannedStatement::with_optimized_plan` accepts optimized queries and INSERTs.
It checks DataFusion invariants, query output contracts, and preservation of the
ModifyTable root, Insert operation, target, and completion schema. UPDATE/DELETE replacement is rejected
until their selected rules have a CTID-preservation policy. Nonrelational
commands cannot accept a replacement relational plan.

These are structural safeguards, not a proof that arbitrary rules preserve modification
effects. Relational query and modification inputs reject both nested ModifyTable nodes
and native DataFusion DML/DDL/statement/COPY nodes, including within subqueries.
[`chilidb-optimizer`](../crates/optimizer/README.md) accepts an explicit ordered
rule list, propagates rule errors, and applies statement-boundary checks before
and after optimization. It has no implicit default rules. Commands pass through
without running rules; UPDATE/DELETE optimization returns an error. Directly
applying arbitrary rules to the public native plan bypasses this policy.
Integration tests exercise selected real DataFusion rules and rejection paths.

DataFusion's optimizer does not automatically run its analyzer. Lowered inputs
must satisfy its analyzed-plan requirements. The future physical planner must
handle the output shapes of enabled rules, including projected TableScans and
EmptyRelation, not just the shapes initially emitted by the planner.
