# chilidb-executor

`Executor::new().execute(statement).await` accepts a planner `PlannedStatement`
and returns `QueryResult { output, batches }`. `output` retains the SQL-visible
names (including duplicates), types, and lineage in positional order; Arrow
batch names are internal planner names. Results are fully collected in memory.
Run execution inside a Tokio runtime.

The executor validates the original query boundary with `with_optimized_plan`
before native physical planning. Commands and ModifyTable statements are
rejected before execution; nested modifications are rejected by boundary
validation. Errors retain planner or DataFusion error details.

A native DataFusion session uses `SessionStateBuilder::with_default_features`.
`DefaultPhysicalPlanner` creates the physical plan directly, then DataFusion
`collect` executes it. Native physical optimization remains enabled. No default
logical optimizer is run here: callers choose logical rules explicitly through
`chilidb-optimizer` before execution. There is no custom operator pipeline or
source registry.

Constant queries support scalar expressions, filtering, aggregation, windows,
and ORDER BY through the existing binder and planner. ChiliDB table scans,
storage access, DML, DDL, and transaction execution are not implemented. The
planner's `TableAdapter` exposes logical metadata, not a native physical scan
provider; table queries fail physical planning until a future storage adapter
provides that bridge. This crate does not add storage or transaction semantics.
