# ChiliDB

A small OLTP database in Rust with independently replaceable layers.

## SQL frontend

```sh
cargo run --example parse -- 'SELECT 1 + 2 * 3'
```

The example prints an unbound AST. It does not execute the query.

| Crate | Responsibility |
| --- | --- |
| [`chilidb-peg`](crates/peg/README.md) | `peg::grammar!`: compile PEG grammars into Rust matchers and borrowed parse trees |
| [`chilidb-parser`](crates/parser/README.md) | SQL grammar, AST types, and fallible tree-to-AST conversion |
| `chilidb` | Library entry point; re-exports `chilidb::parser` |

```text
PEG grammar ──compile time──> Rust matchers
                                  │
SQL text ─────────────────────────┘
                                  ↓
                            Node<'sql> tree
                                  │ ParseNode::parse
                                  ↓
                            Statement<'sql> AST
```

Matching operates directly on UTF-8 text, with explicit whitespace and keyword
boundaries. AST conversion runs after the complete input matches, outside
speculative parsing. The frontend performs no catalog lookup, type checking,
planning, or execution; downstream layers consume AST types rather than PEG
rule names.

Nodes borrow matched text and retain byte spans for diagnostics. AST text is
borrowed unless identifier normalization or quote unescaping requires a copy.
Tree and AST structures use owned vectors and boxes. The SQL input must outlive both structures; the tree can be dropped
before the AST.

See the crate documentation for grammar syntax, supported SQL, and resource
limits. The PEG matcher has no memoization or runtime rule registration.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
```

Tests cover PEG matching and compile-time validation, SQL syntax and precedence,
borrowing, malformed nodes, and frontend integration without a catalog or storage.
Agent workflows are documented in [AGENT.md](AGENT.md).

## References

- [DuckDB v2.0: Your Database Deserves a Better Parser](https://duckdb.org/2026/08/20/duckdb-20-peg-parser)
- [PEG Parser in DuckDB 2.0](https://qsliu.dev/post/duckdb-peg/)
- [DuckDB PEG parser source](https://github.com/duckdb/duckdb/tree/main/src/parser/peg): grammar matching, parse-result transformation, and selective memoization.
