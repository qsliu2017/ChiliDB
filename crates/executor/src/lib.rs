#![doc = include_str!("../README.md")]

use arrow_array::RecordBatch;
use chilidb_planner::{OutputSchema, PlanError, PlannedStatement};
use datafusion::{
    common::DataFusionError,
    execution::{SessionState, session_state::SessionStateBuilder},
    physical_plan::collect,
    physical_planner::{DefaultPhysicalPlanner, PhysicalPlanner},
};

/// Collected Arrow data with SQL-visible, positional output metadata.
#[derive(Debug)]
pub struct QueryResult<'sql> {
    pub output: OutputSchema<'sql>,
    pub batches: Vec<RecordBatch>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    #[error("commands and table modifications are not supported by the query executor")]
    UnsupportedStatement,
    #[error(transparent)]
    InvalidPlan(#[from] PlanError),
    #[error(transparent)]
    DataFusion(#[from] DataFusionError),
}

/// Native DataFusion physical planning and execution, without logical optimization.
pub struct Executor {
    session: SessionState,
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor {
    pub fn new() -> Self {
        Self {
            session: SessionStateBuilder::new().with_default_features().build(),
        }
    }

    /// Validate the statement boundary, physically plan, and collect all batches.
    /// Logical optimization is the caller's explicit, separate responsibility.
    pub async fn execute<'sql>(
        &self,
        statement: PlannedStatement<'sql>,
    ) -> Result<QueryResult<'sql>, ExecuteError> {
        let plan = match &statement {
            PlannedStatement::Query { plan, .. } => plan.clone(),
            _ => return Err(ExecuteError::UnsupportedStatement),
        };
        let PlannedStatement::Query { plan, output } = statement.with_optimized_plan(plan)? else {
            unreachable!("query boundary validation preserves the statement kind")
        };
        // SessionState::create_physical_plan would also run its logical optimizer.
        let physical = DefaultPhysicalPlanner::default()
            .create_physical_plan(&plan, &self.session)
            .await?;
        let batches = collect(physical, self.session.task_ctx()).await?;
        Ok(QueryResult { output, batches })
    }
}
