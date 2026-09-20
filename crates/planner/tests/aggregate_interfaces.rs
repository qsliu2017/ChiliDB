//! DataFusion's aggregate replaces its input schema; window appends to it.
use chilidb_planner::{Expr, LogicalPlan};
use datafusion_expr::{LogicalPlanBuilder, lit, logical_plan::InvariantLevel};
use datafusion_functions_aggregate::expr_fn::count;
use datafusion_functions_window::expr_fn::rank;
use std::sync::Arc;

#[test]
fn aggregation_and_windows_have_distinct_output_layouts() {
    let input = LogicalPlanBuilder::values(vec![vec![lit(1i32)]])
        .unwrap()
        .build()
        .unwrap();
    let key = Expr::Column(input.schema().qualified_field(0).0.map_or_else(
        || datafusion_common::Column::from_name(input.schema().field(0).name()),
        |q| datafusion_common::Column::new(Some(q.clone()), input.schema().field(0).name()),
    ));
    let grouped = LogicalPlanBuilder::from(input)
        .aggregate(vec![key.clone()], vec![count(key).alias("count")])
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(grouped.schema().fields().len(), 2);
    let window = LogicalPlanBuilder::from(grouped.clone())
        .window(vec![rank().alias("rank")])
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(window.schema().fields().len(), 3);
    assert_eq!(
        &window.schema().fields()[..2],
        grouped.schema().fields().as_ref()
    );
    let sorted = LogicalPlanBuilder::from(window)
        .sort(vec![datafusion_expr::col("rank").sort(true, false)])
        .unwrap()
        .build()
        .unwrap();
    sorted.check_invariants(InvariantLevel::Executable).unwrap();
    let LogicalPlan::Sort(sort) = sorted else {
        panic!()
    };
    let LogicalPlan::Window(window) = Arc::unwrap_or_clone(sort.input) else {
        panic!()
    };
    assert_eq!(window.window_expr.len(), 1);
    let LogicalPlan::Aggregate(aggregate) = window.input.as_ref() else {
        panic!()
    };
    assert_eq!(aggregate.group_expr.len(), 1);
    assert_eq!(aggregate.aggr_expr.len(), 1);
}
