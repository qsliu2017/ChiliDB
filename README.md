# ChiliDB

A small OLTP database in Rust with independently replaceable layers.

## Extensibility

ChiliDB uses [Apache DataFusion](https://datafusion.apache.org/)'s native logical
plans and expression types at the optimizer boundary. ChiliDB retains its parser,
binder, catalog abstraction, and ModifyTable extension; physical planning, execution,
and storage remain independent. DataFusion's optimizer can supply selected rules
without adopting its execution engine.

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
logical extension with operation-specific payloads. These examples do not execute SQL.

To inspect an explicit optimizer rule pipeline:

```sh
cargo run --example optimize -- 'INSERT INTO items VALUES (1 + 2)'
```

The optimizer accepts an ordered rule list, preserves query/INSERT statement
contracts, and propagates rule failures. There is no implicit default rule set.
Commands pass through unchanged; UPDATE/DELETE optimization remains disabled.
Physical planning and execution are not implemented.

| Crate | Responsibility |
| --- | --- |
| [`chilidb-common`](crates/common/README.md) | Shared identifiers, including relation-local tuple-version CTIDs |
| [`chilidb-peg`](crates/peg/README.md) | `peg::grammar!`: compile PEG grammars into Rust matchers and borrowed parse trees |
| [`chilidb-parser`](crates/parser/README.md) | SQL grammar, AST types, and fallible tree-to-AST conversion |
| [`chilidb-binder`](crates/binder/README.md) | Catalog-backed name resolution, typed expressions, and bound SQL commands |
| [`chilidb-planner`](crates/planner/README.md) | Bound-AST to DataFusion logical plans, SQL output metadata, and ModifyTable operations |
| [`chilidb-optimizer`](crates/optimizer/README.md) | Explicit rewrite pipelines, rule observation, and statement-boundary checks |
| `chilidb` | Library entry point; re-exports `common`, `parser`, `binder`, `planner`, and `optimizer` |

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

## Execution design draft

[Source-selected pipelines](docs/pipeline-execution-draft.md) describe tuple
execution for heap-backed sources, columnar execution for other sources, and
explicit materialization/state-transfer boundaries. This is a design draft;
physical execution is not implemented.

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
statement-boundary checks. No execution or storage engine is required.
Agent workflows are documented in [AGENT.md](AGENT.md).

## References

- [Apache DataFusion architecture](https://datafusion.apache.org/library-user-guide/architecture.html): extensible query-engine interfaces and component boundaries.
- [DuckDB v2.0: Your Database Deserves a Better Parser](https://duckdb.org/2026/08/20/duckdb-20-peg-parser)
- [PEG Parser in DuckDB 2.0](https://qsliu.dev/post/duckdb-peg/)
- [DuckDB PEG parser source](https://github.com/duckdb/duckdb/tree/main/src/parser/peg): grammar matching, parse-result transformation, and selective memoization.
