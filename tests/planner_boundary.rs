use std::sync::Arc;

use chilidb::{
    binder::Binder,
    parser,
    planner::{Modification, ModifyTable, PlannedStatement, Planner},
};
use datafusion_expr::{LogicalPlan, logical_plan::EmptyRelation};
use datafusion_optimizer::{
    Optimizer, OptimizerContext, optimize_projections::OptimizeProjections,
    simplify_expressions::SimplifyExpressions,
};

#[path = "../examples/support/mod.rs"]
mod support;

fn plan(sql: &str) -> PlannedStatement<'_> {
    let catalog = support::catalog();
    let parsed = parser::parse_sql(sql).unwrap();
    let bound = Binder::new(&catalog).bind(&parsed[0]).unwrap();
    Planner::new().plan(&bound).unwrap()
}

fn relational<'a>(statement: &'a PlannedStatement<'_>) -> &'a LogicalPlan {
    match statement {
        PlannedStatement::Query { plan, .. } | PlannedStatement::ModifyTable { plan } => plan,
        _ => panic!("expected relational statement"),
    }
}

fn optimize(plan: LogicalPlan) -> LogicalPlan {
    Optimizer::with_rules(vec![
        Arc::new(SimplifyExpressions::new()),
        Arc::new(OptimizeProjections::new()),
    ])
    .optimize(
        plan,
        &OptimizerContext::new().with_skip_failing_rules(false),
        |_, _| {},
    )
    .unwrap()
}

#[test]
fn optimizer_boundary_preserves_duplicate_sql_names() {
    let original = plan("SELECT id AS same, id AS same FROM items WHERE id > 0");
    let optimized = optimize(relational(&original).clone());
    let PlannedStatement::Query { plan, output } = original.with_optimized_plan(optimized).unwrap()
    else {
        panic!()
    };
    assert_eq!(output.fields[0].name, "same");
    assert_eq!(output.fields[1].name, "same");
    assert_ne!(plan.schema().field(0).name(), plan.schema().field(1).name());
}

#[test]
fn optimized_insert_preserves_the_effectful_root() {
    let original = plan("INSERT INTO items VALUES (1 + 2)");
    let optimized = optimize(relational(&original).clone());
    let PlannedStatement::ModifyTable { plan } = original.with_optimized_plan(optimized).unwrap()
    else {
        panic!()
    };
    let LogicalPlan::Extension(extension) = plan else {
        panic!()
    };
    let node = extension
        .node
        .as_any()
        .downcast_ref::<ModifyTable>()
        .unwrap();
    assert!(matches!(node.operation(), Modification::Insert { .. }));

    let original = self::plan("INSERT INTO items VALUES (1)");
    let empty = LogicalPlan::EmptyRelation(EmptyRelation {
        produce_one_row: false,
        schema: relational(&original).schema().clone(),
    });
    assert!(original.with_optimized_plan(empty).is_err());
}

#[test]
fn optimizer_cannot_replace_the_insert_target() {
    // Each fixture owns a different source allocation, even with the same SQL name.
    let first = plan("INSERT INTO items VALUES (1)");
    let other = plan("INSERT INTO items VALUES (1)");
    assert!(
        first
            .with_optimized_plan(relational(&other).clone())
            .is_err()
    );
}

#[test]
fn updates_and_deletes_do_not_enable_generic_optimization_implicitly() {
    for sql in ["UPDATE items SET id = id + 1", "DELETE FROM items"] {
        let original = plan(sql);
        let unchanged = relational(&original).clone();
        assert!(original.with_optimized_plan(unchanged).is_err());
    }
}

#[test]
fn optimizer_cannot_change_public_query_types_or_column_order() {
    let original = plan("SELECT id FROM items");
    let replacement = plan("SELECT TRUE FROM items");
    assert!(
        original
            .with_optimized_plan(relational(&replacement).clone())
            .is_err()
    );
}

#[test]
fn optimizer_cannot_change_modification_with_same_target_and_completion_schema() {
    for replace_with_update in [false, true] {
        let original = plan("INSERT INTO items VALUES (1)");
        let LogicalPlan::Extension(extension) = relational(&original) else {
            panic!()
        };
        let insert = extension
            .node
            .as_any()
            .downcast_ref::<ModifyTable>()
            .unwrap();
        let table = insert.target().table();
        let input = chilidb::planner::table_source::scan(table, true).unwrap();
        let ctid = chilidb::planner::table_source::ctid(table);
        let replacement = if replace_with_update {
            ModifyTable::try_update(
                insert.target().clone(),
                input,
                ctid,
                vec![chilidb::planner::UpdateAssignment {
                    target_column_index: 0,
                    value: chilidb::planner::table_source::column(
                        chilidb::binder::bound::ColumnBinding {
                            relation: table.relation,
                            column_index: 0,
                        },
                    ),
                }],
            )
        } else {
            ModifyTable::try_delete(insert.target().clone(), input, ctid)
        }
        .unwrap();
        assert_eq!(insert.target(), replacement.target());
        let replacement = replacement.into_plan();
        assert_eq!(relational(&original).schema(), replacement.schema());
        assert!(original.with_optimized_plan(replacement).is_err());
    }
}
