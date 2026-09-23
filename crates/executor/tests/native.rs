use std::{convert::Infallible, sync::Arc};

use arrow_array::{Array, Int32Array, Int64Array, UInt64Array};
use chilidb_binder::{Binder, Catalog, TableSource};
use chilidb_executor::{ExecuteError, Executor, QueryResult};
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
