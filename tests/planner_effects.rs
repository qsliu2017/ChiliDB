use std::sync::Arc;

use arrow_schema::DataType;
use chilidb::{
    binder::Binder,
    parser,
    planner::{Modification, ModifyTable, PlannedStatement, Planner, table_source::TableAdapter},
};
use datafusion_common::{Column, Constraints, TableReference};
use datafusion_expr::{
    Expr, LogicalPlan, LogicalPlanBuilder,
    expr::Cast,
    logical_plan::{
        CreateMemoryTable, DdlStatement, DmlStatement, WriteOp as DmlOperation, dml::InsertOp,
    },
};

#[path = "../examples/support/mod.rs"]
mod support;

fn plan(sql: &str) -> PlannedStatement<'_> {
    let catalog = support::catalog();
    let parsed = parser::parse_sql(sql).unwrap();
    let bound = Binder::new(&catalog).bind(&parsed[0]).unwrap();
    Planner::new().plan(&bound).unwrap()
}

fn modify_table(plan: &LogicalPlan) -> &ModifyTable {
    let LogicalPlan::Extension(extension) = plan else {
        panic!()
    };
    extension
        .node
        .as_any()
        .downcast_ref::<ModifyTable>()
        .unwrap()
}

fn native_insert() -> LogicalPlan {
    let PlannedStatement::ModifyTable { plan } = plan("INSERT INTO items VALUES (1)") else {
        panic!()
    };
    let node = modify_table(&plan);
    let Modification::Insert { values } = node.operation() else {
        panic!()
    };
    let input = LogicalPlanBuilder::from(node.input().clone())
        .project(vec![Expr::Column(values[0].clone()).alias("__c0")])
        .unwrap()
        .build()
        .unwrap();
    LogicalPlan::Dml(DmlStatement::new(
        TableReference::bare("items"),
        Arc::new(TableAdapter::new(node.target().table().clone(), false)),
        DmlOperation::Insert(InsertOp::Append),
        Arc::new(input),
    ))
}

fn count_projection(input: LogicalPlan, alias: &str) -> LogicalPlan {
    LogicalPlanBuilder::from(input)
        .project(vec![
            Expr::Cast(Cast::new(
                Box::new(Expr::Column(Column::from_name("count"))),
                DataType::Int32,
            ))
            .alias(alias),
        ])
        .unwrap()
        .build()
        .unwrap()
}

#[test]
fn optimizer_cannot_hide_native_dml_inside_a_query() {
    let original = plan("SELECT 1");
    let replacement = count_projection(native_insert(), "__output0");
    let PlannedStatement::Query { plan: before, .. } = &original else {
        panic!()
    };
    assert_eq!(before.schema(), replacement.schema());
    assert!(original.with_optimized_plan(replacement).is_err());
}

#[test]
fn modify_table_cannot_consume_native_dml_results() {
    let PlannedStatement::ModifyTable { plan } = plan("INSERT INTO items VALUES (1)") else {
        panic!()
    };
    let node = modify_table(&plan);
    let input = count_projection(native_insert(), "value");
    assert!(
        ModifyTable::try_insert(
            node.target().clone(),
            input,
            vec![Column::from_name("value")],
        )
        .is_err()
    );
}

#[test]
fn optimizer_cannot_replace_a_query_with_schema_compatible_ddl() {
    let original = plan("SELECT 1");
    let PlannedStatement::Query { plan: before, .. } = &original else {
        panic!()
    };
    let replacement = LogicalPlan::Ddl(DdlStatement::CreateMemoryTable(CreateMemoryTable {
        name: TableReference::bare("unexpected_table"),
        constraints: Constraints::default(),
        input: Arc::new(before.clone()),
        if_not_exists: false,
        or_replace: false,
        column_defaults: vec![],
        temporary: false,
    }));
    assert_eq!(before.schema(), replacement.schema());
    assert!(original.with_optimized_plan(replacement).is_err());
}
