//! Native DataFusion execution of planned queries.
//!
//! [`Executor::execute`] runs inside a Tokio runtime and fully collects results in
//! memory. [`QueryResult::output`] keeps SQL-visible names (including duplicates),
//! types, and lineage in positional order; Arrow batch names are internal planner
//! names. The query boundary is validated with `with_optimized_plan`; commands,
//! ModifyTable roots, and nested modifications are rejected.
//!
//! `DefaultPhysicalPlanner` plans directly, so no logical optimizer runs here
//! (callers choose rules through `chilidb-optimizer`); DataFusion's physical
//! optimization remains enabled. Constant queries execute, including filters,
//! aggregates, windows, and ORDER BY. The planner's `TableAdapter` has no physical
//! scan provider, so table queries fail physical planning; storage access, DML,
//! DDL, and transactions are not implemented.

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

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, sync::Arc};

    use crate::{ExecuteError, Executor, QueryResult};
    use arrow_array::{Array, Int32Array, Int64Array, UInt64Array};
    use chilidb_binder::{Binder, Catalog, TableSource};
    use chilidb_planner::{Command, PlannedStatement, Planner};

    struct EmptyCatalog;
    impl Catalog for EmptyCatalog {
        type Error = Infallible;

        fn get_table(&self, _: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
            Ok(None)
        }
    }

    fn plan(sql: &str) -> PlannedStatement<'_> {
        let statements = chilidb_parser::parse_sql(sql).unwrap();
        let bound = Binder::new(&EmptyCatalog).bind(&statements[0]).unwrap();
        Planner::new().plan(&bound).unwrap()
    }

    async fn query(sql: &str) -> QueryResult<'_> {
        Executor::new().execute(plan(sql)).await.unwrap()
    }

    fn integers(result: &QueryResult<'_>, column: usize) -> Vec<Option<i64>> {
        result
            .batches
            .iter()
            .flat_map(|batch| {
                let array = batch.column(column);
                (0..array.len()).map(move |row| {
                    if array.is_null(row) {
                        None
                    } else if let Some(array) = array.as_any().downcast_ref::<Int32Array>() {
                        Some(i64::from(array.value(row)))
                    } else if let Some(array) = array.as_any().downcast_ref::<Int64Array>() {
                        Some(array.value(row))
                    } else if let Some(array) = array.as_any().downcast_ref::<UInt64Array>() {
                        Some(i64::try_from(array.value(row)).unwrap())
                    } else {
                        panic!("unexpected integer array: {:?}", array.data_type())
                    }
                })
            })
            .collect()
    }

    #[tokio::test]
    async fn scalar_and_duplicate_sql_names_preserve_metadata() {
        let statement = plan("SELECT 1 + 2 AS same, 4 AS same");
        let PlannedStatement::Query { output, .. } = &statement else {
            panic!("expected query")
        };
        let expected = output.clone();
        let result = Executor::default().execute(statement).await.unwrap();
        assert_eq!(result.output, expected);
        assert_eq!(result.output.fields[0].name, "same");
        assert_eq!(result.output.fields[1].name, "same");
        assert_eq!(integers(&result, 0), vec![Some(3)]);
        assert_eq!(integers(&result, 1), vec![Some(4)]);
    }

    #[tokio::test]
    async fn false_filter_has_no_rows() {
        let result = query("SELECT 1 WHERE FALSE").await;
        assert_eq!(result.output.fields.len(), 1);
        assert!(integers(&result, 0).is_empty());
    }

    #[tokio::test]
    async fn global_aggregates_include_empty_input() {
        let result = query("SELECT COUNT(*), SUM(7)").await;
        assert_eq!(integers(&result, 0), vec![Some(1)]);
        assert_eq!(integers(&result, 1), vec![Some(7)]);
        let result = query("SELECT COUNT(*), SUM(7) WHERE FALSE").await;
        assert_eq!(integers(&result, 0), vec![Some(0)]);
        assert_eq!(integers(&result, 1), vec![None]);
    }

    #[tokio::test]
    async fn native_window_and_order() {
        let result = query("SELECT ROW_NUMBER() OVER (ORDER BY 7) AS n ORDER BY n DESC").await;
        assert_eq!(integers(&result, 0), vec![Some(1)]);
    }

    #[tokio::test]
    async fn commands_and_modification_statements_are_rejected() {
        let executor = Executor::new();
        for command in [Command::Begin, Command::Commit, Command::Rollback] {
            assert!(matches!(
                executor.execute(PlannedStatement::Command(command)).await,
                Err(ExecuteError::UnsupportedStatement)
            ));
        }
        let PlannedStatement::Query { plan, .. } = plan("SELECT 1") else {
            panic!("expected query")
        };
        // Even a malformed modification is rejected at the statement boundary.
        assert!(matches!(
            executor
                .execute(PlannedStatement::ModifyTable { plan })
                .await,
            Err(ExecuteError::UnsupportedStatement)
        ));
    }

    #[tokio::test]
    async fn invalid_original_output_is_rejected() {
        let mut statement = plan("SELECT 1");
        let PlannedStatement::Query { output, .. } = &mut statement else {
            panic!("expected query")
        };
        output.fields.clear();
        assert!(matches!(
            Executor::new().execute(statement).await,
            Err(ExecuteError::InvalidPlan(_))
        ));
    }

    #[tokio::test]
    async fn real_modification_cannot_be_disguised_as_query() {
        use chilidb_binder::{ColumnSchema, LogicalType, TableSchema, bound};
        use chilidb_planner::{ModifyTable, Target};

        #[derive(Debug)]
        struct MetadataOnly;
        impl TableSource for MetadataOnly {
            fn schema(&self) -> Arc<TableSchema> {
                Arc::new(TableSchema {
                    name: "items".into(),
                    columns: vec![ColumnSchema {
                        name: "id".into(),
                        data_type: LogicalType::Int32,
                        nullable: false,
                    }],
                })
            }
        }
        let PlannedStatement::Query {
            plan: input,
            output,
        } = plan("SELECT 1")
        else {
            panic!("expected query")
        };
        let target = Target::new(bound::Table {
            relation: bound::RelationId(0),
            source: Arc::new(MetadataOnly),
        });
        let values = input.schema().columns();
        let modification = ModifyTable::try_insert(target, input, values)
            .unwrap()
            .into_plan();
        let executor = Executor::new();
        assert!(matches!(
            executor
                .execute(PlannedStatement::ModifyTable {
                    plan: modification.clone()
                })
                .await,
            Err(ExecuteError::UnsupportedStatement)
        ));
        assert!(matches!(
            executor
                .execute(PlannedStatement::Query {
                    plan: modification,
                    output
                })
                .await,
            Err(ExecuteError::InvalidPlan(_))
        ));
    }
}
