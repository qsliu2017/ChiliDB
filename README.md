# ChiliDB

A small OLTP database in Rust with independently replaceable layers.

## Extensibility

ChiliDB uses [Apache DataFusion](https://datafusion.apache.org/)'s native logical
plans and expression types at the optimizer boundary. ChiliDB retains its parser,
binder, catalog abstraction, and ModifyTable extension. Execution uses DataFusion's
standard physical planner and pull-based Arrow executor. Logical rule selection
remains explicit. Page storage and buffer management are separate from the SQL
frontend and executor.

## SQL frontend

```sh
cargo run --example parse -- 'SELECT 1 + 2 * 3'
```

The example prints an unbound AST. To resolve names and inspect types against a
demo catalog:

```sh
cargo run --example bind -- 'SELECT id + 1 FROM items WHERE id > 0'
```

Aggregates and windows also produce typed bound expressions:

```sh
cargo run --example bind -- 'SELECT SUM(id), COUNT(*) FROM items'
cargo run --example bind -- 'SELECT id, SUM(id) OVER (ORDER BY id ROWS 2 PRECEDING) FROM items'
```

To inspect logical operators and their output schemas:

```sh
cargo run --example plan -- 'SELECT SUM(id), COUNT(*) FROM items HAVING COUNT(*) > 0'
```

Logical planning handles queries and commands, including grouping, windowing,
ordering, and CTID-based modification-row identity. INSERT, UPDATE, and DELETE share a ModifyTable
logical extension with operation-specific payloads. The parse/bind/plan examples
do not execute SQL.

To inspect an explicit optimizer rule pipeline:

```sh
cargo run --example optimize -- 'INSERT INTO items VALUES (1 + 2)'
```

The optimizer accepts an ordered rule list, preserves query/INSERT statement
contracts, and propagates rule failures. There is no implicit default rule set.
Commands pass through unchanged; UPDATE/DELETE optimization remains disabled.
The query executor uses native DataFusion physical planning and collection:

```sh
cargo run --example execute -- 'SELECT 1 + 2 AS answer'
```

Table-scan adapters, DML execution, and transaction command handling are not
implemented yet. Constant queries, including supported aggregates/windows, can run.

| Crate | Responsibility |
| --- | --- |
| [`chilidb-common`](crates/common/src/lib.rs) | Shared identifiers, including relation-local tuple-version CTIDs |
| [`chilidb-peg`](crates/peg/README.md) | `peg::grammar!`: compile PEG grammars into Rust matchers and borrowed parse trees |
| [`chilidb-parser`](crates/parser/README.md) | SQL grammar, AST types, and fallible tree-to-AST conversion |
| [`chilidb-binder`](crates/binder/README.md) | Catalog-backed name resolution, typed expressions, and bound SQL commands |
| [`chilidb-planner`](crates/planner/README.md) | Bound-AST to DataFusion logical plans, SQL output metadata, and ModifyTable operations |
| [`chilidb-optimizer`](crates/optimizer/src/lib.rs) | Explicit rewrite pipelines, rule observation, and statement-boundary checks |
| [`chilidb-executor`](crates/executor/src/lib.rs) | Native DataFusion query execution with SQL output metadata |
| [`chilidb-storage`](crates/storage/README.md) | Fixed-size page stores, buffer pins/latches, dirty flushing, and clock replacement |
| [`chilidb-heapam`](crates/heapam/README.md) | Slotted-page byte records, CTID access, compaction, and page-bounded scans |
| [`chilidb-catalog`](crates/catalog/src/lib.rs) | Heap-backed table metadata and stable binder-compatible table handles |
| `chilidb` | Re-exports `common`, `parser`, `binder`, `planner`, `optimizer`, `executor`, `storage`, `heapam`, and `catalog` |

```text
PEG grammar ──compile time──> Rust matchers
                                  │
SQL text ─────────────────────────┘
                                  ↓
                            Node<'sql> tree
                                  │ ParseNode::parse
                                  ↓
                            Statement<'sql> AST
                                  │ Binder::bind
                                  ↓
                            bound::Statement<'sql>
                                  │ Planner::plan
                                  ↓
                            DataFusion query / modification plan
                            or nonrelational command
                                  │ Optimizer::optimize (query / INSERT)
                                  ↓
                            Validated optimized statement
                                  │ Executor::execute (queries)
                                  ↓
                            Native DataFusion physical plan → RecordBatches
```

Matching operates directly on UTF-8 text, with explicit whitespace and keyword
boundaries. AST conversion runs after the complete input matches, outside
speculative parsing. The parser performs no catalog lookup or type checking. The binder validates
queries and commands against a catalog and inserts explicit casts. Planning
consumes this bound AST and builds DataFusion operators, not PEG rule names.
Execution is a separate layer.

Nodes borrow matched text and retain byte spans for diagnostics. AST text is
borrowed unless identifier normalization or quote unescaping requires a copy.
Tree and AST structures use owned vectors and boxes. The SQL input must outlive both structures; the tree can be dropped
before the AST.

See the crate documentation for grammar syntax, supported SQL, and resource
limits. The PEG matcher has no memoization or runtime rule registration.

## Storage foundation

The buffer pool owns contiguous, 8 KiB-aligned pages and a separate aligned
descriptor array. `pin(PageId)` protects residency without retaining a content
lock; `PinnedBuffer::read/write` scope latches to callbacks. Replacement uses a
pluggable `VictimStrategy`, defaulting to a second-chance clock. The pool validates
victim eligibility independently of policy. File and memory page stores share one interface. Dirty pages
are flushed explicitly; pin Drop never performs writeback.

`heapam` stores opaque byte records in linked slotted pages. CTIDs survive
compaction, deleted slots are not reused, and scans retain no page pins across
iterator calls. Same-page replacement is supported; growing records that cannot
fit return NoSpace rather than silently relocating.

`catalog` stores versioned table definitions in its own heap: names, column types,
nullability, constraints, and data-heap roots. Its cached table handles implement
the binder's Catalog/TableSource interfaces. Definitions and heap chains reopen
after an explicit successful flush; callers retain the catalog root page ID.

```sh
cargo run --example heap_catalog
```

This creates and reopens a temporary catalog file, then binds a query against its
stored schema; it does not execute a heap scan.

SQL row encoding, native heap-scan adapters, ModifyTable execution, transaction
state/snapshots, MVCC, and WAL/recovery remain next layers. Constraints are stored
metadata, not enforced by raw heap access. Page pins are not row locks, and flush
is not commit.

## Storage protocol model checking

Storage performance work may use compact layouts, separate data/locks, and small
unsafe boundaries without relaxing Rust's memory-safety requirements. The first
[Stateright model](crates/storage/src/models/buffer_pool.rs) checks a proposed asynchronous
buffer-loading protocol: victim reservation, duplicate requests, content access,
and I/O failure recovery. It does not replace the current synchronous runtime.

```sh
cargo test -p chilidb-storage --test buffer_model -- --nocapture
```

The checks cover safety properties and reachability witnesses within finite
configurations of the abstract protocol.

## Alternative execution design

[Source-selected pipelines](docs/pipeline-execution-draft.md) remain an exploratory
alternative, not the active executor. The current baseline is native DataFusion
execution.

## Development

Rust 1.94 or later is required by the pinned DataFusion 55.1.0 dependencies.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

Tests cover PEG matching and compile-time validation, SQL syntax and precedence,
borrowing, malformed nodes, catalog-backed binding, aggregate/window scope rules,
logical-plan construction, and explicit optimizer pipelines with failure and
statement-boundary checks. Native query execution and page-store/buffer-pool
contracts are tested independently, including I/O failures and concurrent access.
Heap and catalog tests cover slotted-page corruption, stable CTIDs, scan bounds,
metadata validation, binder integration, and clean-close file reopening.
Agent workflows are documented in [AGENT.md](AGENT.md).

## References

- [Apache DataFusion architecture](https://datafusion.apache.org/library-user-guide/architecture.html): extensible query-engine interfaces and component boundaries.
- [DuckDB v2.0: Your Database Deserves a Better Parser](https://duckdb.org/2026/08/20/duckdb-20-peg-parser)
- [PEG Parser in DuckDB 2.0](https://qsliu.dev/post/duckdb-peg/)
- [DuckDB PEG parser source](https://github.com/duckdb/duckdb/tree/main/src/parser/peg): grammar matching, parse-result transformation, and selective memoization.
