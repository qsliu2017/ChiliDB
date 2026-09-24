# ChiliDB Agent Guide

ChiliDB is a small OLTP database for teaching and learning. Favor code that exposes database ideas over production-scale features or clever abstractions.

## Choose one role

Use exactly one role before substantial work:

- **Teacher**: load `.agents/skills/teacher/SKILL.md`. Design the course variant, starter code, assignments, reference implementations, and tests.
- **Learner**: load `.agents/skills/learner/SKILL.md`. Explain and discuss freely, but obey its strict limits on writing implementation code.

If the user has not selected a role and the request could affect coursework, ask: **“Should I work as the teacher or learner?”** Do not infer the role from access to solution code. Never use the teacher role to bypass learner restrictions.

In pi, these skills are invoked as `/skill:teacher` and `/skill:learner`. A client may expose them as `/teacher` and `/learner`.

## Product direction

Use PostgreSQL and DuckDB as design references, not compatibility requirements.

- Prefer PostgreSQL-like SQL behavior, catalog-driven design, transactions, MVCC, WAL, and clear subsystem ownership.
- Prefer DuckDB-like embeddability, readable modern implementation, explicit data flow, and clean logical-to-physical planning.
- Optimize first for OLTP: short transactions, point/range access, predictable latency, and correctness under concurrency.
- Keep room for both tuple-at-a-time and vectorized execution so students can measure their trade-offs.
- Build the database as a library first. A CLI or wire server should be a thin adapter.

## Architectural boundaries

Keep these layers independently replaceable:

1. **SQL frontend** — parser, binder, types, and catalog-facing names.
2. **Planning** — logical plans, rewrite/optimization rules, physical plans, and cost model.
3. **Execution** — executor operators and expression evaluation over an abstract data source.
4. **Transactions** — transaction state, concurrency control, lock/deadlock policy, and visibility.
5. **Storage** — records/pages, indexes, buffer management, disk I/O, WAL, and recovery.

Cross boundaries through small Rust traits and stable data types. Depend on interfaces, not concrete implementations. Pass logical identifiers rather than internal pointers. Keep policy separate from mechanism—for example, replacement policy from buffer management and lock policy from transaction state.

Do not create a generic abstraction until there are two plausible implementations or an assignment explicitly compares alternatives. Document invariants and ownership at each boundary. Avoid global mutable state.

## Assignment shape

Use the CMU 15-445/645 Fall 2025 progression as a reference, adapted to Rust and this architecture:

1. Rust/database primer and SQL exercises
2. Storage, buffer pool, and replacement policy
3. Indexes and access paths
4. Query planning and execution
5. Concurrency control, then WAL/recovery

Each assignment must identify:

- learning objectives and prerequisites;
- provided components versus student-owned TODOs;
- public API and invariants that must not change;
- visible tests, instructor-only tests, and grading weights;
- correctness, concurrency, persistence, and performance expectations;
- a small required milestone before the full implementation.

The course may reorder or split this progression after the teacher gathers institutional constraints.

## Engineering rules

- Write code comments, crate READMEs, and technical design docs as project documentation: describe behavior, interfaces, invariants, and limitations. Keep teacher/learner roles, assignment ownership, and student TODO policy in agent guidance or dedicated course materials, not implementation documentation. Document current behavior rather than conversation history or immediate decisions; keep architecture in the root README and API details in crate READMEs or rustdoc. Omit comments that merely repeat field names or types; retain non-obvious semantics, defaults, and invariants. Describe the implementation directly and affirmatively. Avoid contrastive explanations such as “this is not X” or “X, not Y”; state behavior, scope, and assumptions explicitly.

- Treat buffer management and the whole storage layer as performance-critical. Favor compact/cache-line-aware layouts, contiguous aligned data, and fine-grained synchronization. Data may be separate from locks; do not require RwLock<Page> merely to fit a Rust wrapper. Small, audited unsafe blocks are permitted, but Rust aliasing, lifetimes, and memory-ordering rules still apply: undefined behavior and data races are not performance optimizations. Document safety invariants, minimize unsafe scope, and measure trade-offs.
- Use formal methods (Stateright or TLA+) to check storage concurrency protocols. Record finite bounds, atomic steps, failure/cancellation assumptions, safety properties, and any fairness required for progress. Check the modeled protocol's safety properties and include reachability witnesses for the relevant scenarios. Distinguish an abstract protocol proof from implementation refinement and Rust memory safety; retain Miri, fault-injection, and runtime tests.

- Use stable Rust unless an assignment explicitly teaches an unstable feature.
- Keep `cargo fmt`, `cargo clippy --all-targets --all-features`, and `cargo test --all` passing.
- Unit-test local invariants; use contract tests for interchangeable components; use integration tests for SQL behavior.
- Make concurrency and recovery tests deterministic with barriers, seeded schedules, fault injection, and restart checks. Do not rely on sleeps for correctness.
- Keep public starter tests diagnostic. Keep adversarial edge cases and grading thresholds in an instructor-only suite when the institution supports private tests.
- Record intentional PostgreSQL or DuckDB semantic differences in project documentation.

## Current state

The workspace contains the root library and `crates/common`, `crates/peg`, `crates/parser`, `crates/binder`, `crates/planner`, `crates/optimizer`, `crates/executor`, `crates/storage`, `crates/heapam`, and `crates/catalog`. The root exports `chilidb::common`, `chilidb::parser`, `chilidb::binder`, `chilidb::planner`, `chilidb::optimizer`, `chilidb::executor`, `chilidb::storage`, `chilidb::heapam`, and `chilidb::catalog`. These crates are instructor-provided infrastructure, not student TODOs; see the root and crate READMEs. The parser provides SQL grammar and borrowed AST conversion. The binder handles supported statements over a generic catalog, including grouping, built-in aggregates, and ranking/aggregate windows, with table-source handles, explicit casts, and validated DDL/DML representations. Binding does not mutate the catalog or execute transaction commands. `crates/planner` converts bound statements into DataFusion logical query plans and a unified ModifyTable extension for Insert/Update/Delete, retaining SQL-visible output metadata and CTID-based modification-row identity. `crates/common` defines `Ctid` as a relation-local u32 page ID plus a one-based u16 tuple slot, encoded into the internal non-nullable FixedSizeBinary(6) `__ctid` column for UPDATE/DELETE. DataFusion 55.1.0 is pinned; Rust 1.94 or later is required. `crates/optimizer` runs explicit ordered DataFusion rule lists, with no implicit defaults, no skipped rule errors, bounded passes, and optional rule observation. Query and INSERT statements are checked before and after optimization; commands pass through, and UPDATE/DELETE optimization is rejected pending CTID-preservation policy. Optimizer plumbing is provided; assignment rule ownership remains undecided, and rule lists must not mask the rule under assessment. The execution baseline is native DataFusion: `crates/executor` validates query boundaries, calls DefaultPhysicalPlanner directly without implicit logical optimization, and collects native Arrow streams. Constant queries (including supported aggregates/windows) execute; table-scan adapters, DML execution, and transaction commands are not enabled.

`crates/storage` provides 8 KiB store-local pages, FilePageStore/MemoryPageStore, and a fixed-capacity BufferPool with second-chance clock replacement. A contiguous 8 KiB-aligned page allocation is separate from a contiguous, 64-byte-aligned descriptor array. Descriptor content locks are RwLock<()>; page UnsafeCells are accessed only through audited private latch-scoped helpers. BufferPool::pin(PageId) returns a residency-only PinnedBuffer; read/write callbacks acquire and promptly release content locks. References cannot escape callbacks. Write access marks dirty while exclusively latched, before the callback, not on unpin. BufferId is a reusable resident slot identity, distinct from PageId. PinnedBuffer<P: AsRef<BufferPool>> retains a pool handle plus u32 buffer/page IDs. Access and Drop resolve the handle through AsRef, and private construction preserves pool identity. Pool methods return PinnedBuffer<&BufferPool> (16 bytes on 64-bit), with the borrow retaining the pool's lifetime. VictimStrategy is a public independent replacement-policy interface; ClockStrategy is the default and BufferPool::with_strategy injects alternatives. Policies receive access notifications and an eligibility callback; pool-side validation rejects out-of-range or pinned candidates before I/O or unsafe access. Policy callbacks run under metadata synchronization and must not reenter the pool. Dirty victims flush before reuse; failed I/O retains old resident state. Explicit flush_all syncs the backing store but is not a transaction commit. Pin and pool Drop do not flush dirty pages. One pool per store and one store instance per file are required for coherence. `crates/heapam` implements pre-MVCC linked slotted pages containing opaque byte records. CTIDs are checked against cached heap membership; compaction preserves slots, deleted slots are never reused, and replacement cannot relocate across pages. Scans capture page/slot bounds, copy one page's records at a time, and retain no pins or latches across iterator calls; they are not transaction snapshots. Heap handles must be opened once and shared with Arc. `crates/catalog` stores versioned table definitions in a metadata heap and implements binder Catalog/TableSource with stable cached source identities. It preserves declared types/nullability/constraints and table heap roots; constraints are not enforced on raw heap mutations. Catalog bootstrap requires the caller to retain its root page ID. Clean-close persistence requires explicit successful flushing; failed publication can orphan pages. SQL row encoding, native heap-scan adapters, ModifyTable execution, transaction state/snapshots, MVCC, conflict handling, WAL, and recovery remain unimplemented. `crates/storage/models/buffer_pool.rs` is a Stateright specification of a proposed asynchronous victim/loading protocol, not the production buffer pool. Development-only model checks explore finite frame/page/backend bounds, and check nine safety properties and non-vacuity witnesses. See `crates/storage/models/README.md` for atomicity, fairness, cancellation, fingerprint, and refinement limitations. The current runtime still uses synchronous scratch-buffer loading under the metadata mutex. The source-selected pipeline document is an exploratory alternative, not the active executor. Establish course scope before assigning components to students.