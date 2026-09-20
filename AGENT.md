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

- Write code comments, crate READMEs, and technical design docs as project documentation: describe behavior, interfaces, invariants, and limitations. Keep teacher/learner roles, assignment ownership, and student TODO policy in agent guidance or dedicated course materials, not implementation documentation. Document current behavior rather than conversation history or immediate decisions; keep architecture in the root README and API details in crate READMEs or rustdoc.

- Use stable Rust unless an assignment explicitly teaches an unstable feature.
- Keep `cargo fmt`, `cargo clippy --all-targets --all-features`, and `cargo test --all` passing.
- Unit-test local invariants; use contract tests for interchangeable components; use integration tests for SQL behavior.
- Make concurrency and recovery tests deterministic with barriers, seeded schedules, fault injection, and restart checks. Do not rely on sleeps for correctness.
- Keep public starter tests diagnostic. Keep adversarial edge cases and grading thresholds in an instructor-only suite when the institution supports private tests.
- Record intentional PostgreSQL or DuckDB semantic differences in project documentation.

## Current state

The workspace contains the root library, `crates/peg` (general PEG procedural macro), and `crates/parser` (SQL grammar and AST transformer). The root exports the frontend as `chilidb::parser`. These crates are instructor-provided infrastructure, not student TODOs; see the root and crate READMEs. Binding, planning, execution, storage, and transactions remain unimplemented. Establish course scope before assigning their components to students.