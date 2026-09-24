//! Explicit DataFusion logical rule pipelines over ChiliDB's planned statements.
//!
//! Rules are upstream or custom [`OptimizerRule`]s run in the supplied order;
//! there is no default rule set, analyzer pass, separate logical IR, cost model,
//! or physical optimization. Each call uses a fresh context: rule failures are
//! returned rather than skipped, time-dependent constant folding is disabled, and
//! panics in supplied rules are not caught. Reaching the pass limit returns the
//! current valid plan without proving that rewrites are exhausted.
//!
//! | Statement | Behavior |
//! | --- | --- |
//! | Query | Validate, optimize, reattach SQL output metadata |
//! | INSERT | Validate, optimize, check ModifyTable root/target/completion |
//! | UPDATE / DELETE | Error before invoking any rule; CTID preservation needs a policy |
//! | CREATE TABLE / transaction commands | Return unchanged without invoking rules |
//!
//! Queries and INSERT are validated with `with_optimized_plan` before and after
//! the pipeline. These checks preserve statement boundaries and SQL-visible output
//! metadata; they do not prove semantic equivalence, CTID provenance, or the
//! correctness of extension nodes, UDFs, or supplied rules. One rule can remove
//! the pattern another targets, so test rules individually and in order.
//!
//! ```
//! use chilidb_optimizer::Optimizer;
//! use chilidb_planner::{Command, PlannedStatement};
//!
//! let optimizer = Optimizer::new(vec![]);
//! let statement = optimizer.optimize(PlannedStatement::Command(Command::Begin))?;
//! assert!(matches!(statement, PlannedStatement::Command(Command::Begin)));
//! # Ok::<(), chilidb_optimizer::OptimizeError>(())
//! ```
//!
//! The workspace `optimize` example runs one explicit rule configuration.

use std::{num::NonZeroU8, sync::Arc};

use chilidb_planner::{PlanError, PlannedStatement};
use datafusion_common::DataFusionError;
use datafusion_expr::LogicalPlan;
pub use datafusion_optimizer::OptimizerRule;
use datafusion_optimizer::{Optimizer as RuleOptimizer, OptimizerContext};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OptimizeError {
    /// Statement-boundary validation failed.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// A rule failed; DataFusion's context is retained.
    #[error(transparent)]
    DataFusion(#[from] DataFusionError),
}

#[derive(Clone, Debug)]
pub struct Optimizer {
    rules: RuleOptimizer,
    max_passes: NonZeroU8,
}

impl Optimizer {
    /// Rules run in the supplied order, for at most three passes by default.
    /// An empty list validates statement contracts without applying rewrites.
    pub fn new(rules: Vec<Arc<dyn OptimizerRule + Send + Sync>>) -> Self {
        Self {
            rules: RuleOptimizer::with_rules(rules),
            max_passes: NonZeroU8::new(3).unwrap(),
        }
    }

    /// DataFusion may stop earlier when it detects an unchanged or repeated plan.
    pub fn with_max_passes(mut self, max_passes: NonZeroU8) -> Self {
        self.max_passes = max_passes;
        self
    }

    pub fn optimize<'sql>(
        &self,
        statement: PlannedStatement<'sql>,
    ) -> Result<PlannedStatement<'sql>, OptimizeError> {
        self.optimize_with_observer(statement, |_, _| {})
    }

    /// Observe successful rule results before final statement-boundary validation.
    /// Observations can include candidates that the boundary subsequently rejects.
    pub fn optimize_with_observer<'sql, F>(
        &self,
        statement: PlannedStatement<'sql>,
        observer: F,
    ) -> Result<PlannedStatement<'sql>, OptimizeError>
    where
        F: FnMut(&LogicalPlan, &dyn OptimizerRule),
    {
        let plan = match &statement {
            PlannedStatement::Query { plan, .. } | PlannedStatement::ModifyTable { plan } => plan,
            PlannedStatement::Command(_) => return Ok(statement),
        };
        // Reject unsupported or malformed statements before any supplied rule runs.
        statement.clone().with_optimized_plan(plan.clone())?;
        let context = OptimizerContext::new()
            .without_query_execution_start_time()
            .with_skip_failing_rules(false)
            .with_max_passes(self.max_passes.get());
        let optimized = self.rules.optimize(plan.clone(), &context, observer)?;
        Ok(statement.with_optimized_plan(optimized)?)
    }
}
