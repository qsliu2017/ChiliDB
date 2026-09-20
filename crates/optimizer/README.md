# chilidb-optimizer

An explicit logical rewrite pipeline over `chilidb_planner::PlannedStatement`.
The optimizer uses DataFusion 55.1.0 rules directly; it does not introduce another
logical IR or build physical execution plans. Upstream rules can use DataFusion's
physical expression kernels for planning-time constant folding; that does not
select an executor for ChiliDB.

## Interface

```rust,ignore
use std::sync::Arc;
use chilidb_optimizer::Optimizer;
use datafusion_optimizer::simplify_expressions::SimplifyExpressions;

let optimizer = Optimizer::new(vec![Arc::new(SimplifyExpressions::new())]);
let optimized = optimizer.optimize(planned_statement)?;
```

`new` requires an ordered rule list. There is no default rule set, analyzer pass,
or implicit registration of other rules. An empty list still checks statement
contracts. The pipeline runs for at most three passes by default, stopping earlier
when DataFusion detects an unchanged or repeated plan. `with_max_passes` accepts a
`NonZeroU8` to change that limit. Reaching the limit returns the current valid plan;
it does not prove that every possible rewrite has been exhausted.

`optimize_with_observer` accepts a callback receiving the plan and rule after each
successful rule application, including unchanged results. Observations precede
final statement-boundary validation and can include rejected candidates.

Each optimization gets a fresh context. Rule failures are returned, never skipped.
Time-dependent constant folding is disabled because this interface has no query
execution timestamp. The pipeline does not catch panics in supplied rules.

## Statement contracts

| Statement | Behavior |
| --- | --- |
| Query | Validate, optimize, reattach SQL output metadata |
| INSERT | Validate, optimize the plan, check ModifyTable root/target/completion |
| UPDATE / DELETE | Return an error before invoking any rule |
| CREATE TABLE / transaction commands | Return unchanged without invoking rules |

Queries and INSERT are checked through `with_optimized_plan` both before any rule
runs and after the pipeline finishes. These checks reject malformed statement
boundaries, preserve query output columns/types/nullability and SQL-visible
metadata, and reject known modification/command nodes inside relational inputs.
They do not prove semantic equivalence, CTID provenance, or the correctness of
arbitrary extension nodes, UDFs, or supplied rules.

UPDATE and DELETE are deliberately not sent through a generic rule pipeline.
Their optimization requires a CTID-preservation policy beyond schema equality.

`OptimizeError::Plan` retains statement-boundary failures;
`OptimizeError::DataFusion` retains rule-engine failures and their context.

## Rule selection

Reuse upstream rules by passing their `Arc<dyn OptimizerRule + Send + Sync>`
implementations. Custom rules implement the same re-exported `OptimizerRule`
trait. Test rules individually and in the intended order: one rule can eliminate
the pattern another was meant to handle. Full-pipeline success alone does not
establish that an individual rule works.

The example is one explicit configuration, not a built-in default:

```sh
cargo run --example optimize -- 'SELECT id + (1 + 2) AS total FROM items WHERE TRUE'
cargo run --example optimize -- 'INSERT INTO items VALUES (1 + 2)'
```

Physical planning must support the shapes produced by the enabled rules. There
is no cost model, access-path selection, physical optimizer, or execution here.
