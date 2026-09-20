use std::{
    num::NonZeroU8,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use chilidb::{
    binder::Binder,
    optimizer::{OptimizeError, Optimizer},
    parser,
    planner::{Command, Modification, ModifyTable, PlannedStatement, Planner},
};
use datafusion_common::{DataFusionError, tree_node::Transformed};
use datafusion_expr::{
    LogicalPlan, lit,
    logical_plan::{EmptyRelation, Filter},
};
use datafusion_optimizer::{
    OptimizerConfig, OptimizerRule, optimize_projections::OptimizeProjections,
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

fn insert<'a>(statement: &'a PlannedStatement<'_>) -> &'a ModifyTable {
    let LogicalPlan::Extension(extension) = relational(statement) else {
        panic!("expected extension")
    };
    extension.node.as_any().downcast_ref().unwrap()
}

fn rules() -> Optimizer {
    Optimizer::new(vec![
        Arc::new(SimplifyExpressions::new()),
        Arc::new(OptimizeProjections::new()),
    ])
}

#[derive(Debug)]
struct FailingRule(Arc<AtomicUsize>);
impl OptimizerRule for FailingRule {
    fn name(&self) -> &str {
        "deliberate_failure"
    }
    fn rewrite(
        &self,
        _: LogicalPlan,
        _: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>, DataFusionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(DataFusionError::Plan("optimization-test-marker".into()))
    }
}

#[test]
fn real_rules_fold_constants_and_preserve_duplicate_sql_metadata() {
    let original = plan("SELECT id AS same, id AS same, 1 + 2 AS folded FROM items");
    let PlannedStatement::Query {
        output: expected, ..
    } = &original
    else {
        panic!()
    };
    let expected = expected.clone();
    let before = relational(&original).display_indent().to_string();
    let mut observed = Vec::new();
    let optimized = rules()
        .optimize_with_observer(original, |_, rule| observed.push(rule.name().to_owned()))
        .unwrap();
    let PlannedStatement::Query { plan, output } = optimized else {
        panic!()
    };
    assert_eq!(output, expected);
    assert_eq!(output.fields[0].name, "same");
    assert_eq!(output.fields[1].name, "same");
    assert_ne!(plan.schema().field(0).name(), plan.schema().field(1).name());
    let after = plan.display_indent().to_string();
    assert_ne!(before, after);
    assert!(!after.contains(" + "), "{after}");
    assert!(after.contains("Int32(3)"), "{after}");
    assert!(!observed.is_empty());
    assert_eq!(observed.len() % 2, 0);
    for pass in observed.chunks_exact(2) {
        assert_eq!(pass, ["simplify_expressions", "optimize_projections"]);
    }
}

#[test]
fn scalar_aggregate_and_window_queries_keep_output_contracts() {
    for sql in [
        "SELECT id + 0 FROM items",
        "SELECT SUM(id), COUNT(*) FROM items",
        "SELECT ROW_NUMBER() OVER (ORDER BY id), SUM(id) OVER () FROM items",
    ] {
        let original = plan(sql);
        let PlannedStatement::Query {
            output: expected, ..
        } = &original
        else {
            panic!()
        };
        let expected = expected.clone();
        let PlannedStatement::Query { output, .. } = rules().optimize(original).unwrap() else {
            panic!()
        };
        assert_eq!(output, expected, "{sql}");
    }
}

#[test]
fn insert_folds_values_without_changing_target_or_effectful_root() {
    let original = plan("INSERT INTO items VALUES (1 + 2)");
    let target = insert(&original).target().clone();
    let schema = relational(&original).schema().clone();
    let optimized = rules().optimize(original).unwrap();
    let node = insert(&optimized);
    assert!(matches!(node.operation(), Modification::Insert { .. }));
    assert_eq!(node.target(), &target);
    assert_eq!(relational(&optimized).schema(), &schema);
    let input = node.input().display_indent().to_string();
    // Simplification preserves the original expression name in an alias.
    let LogicalPlan::Values(values) = node.input() else {
        panic!("expected values: {input}")
    };
    let mut value = &values.values[0][0];
    while let datafusion_expr::Expr::Alias(alias) = value {
        value = &alias.expr;
    }
    assert!(
        matches!(
            value,
            datafusion_expr::Expr::Literal(datafusion_common::ScalarValue::Int32(Some(3)), _)
        ),
        "{input}"
    );
}

#[test]
fn commands_bypass_rules_and_observers() {
    let calls = Arc::new(AtomicUsize::new(0));
    let optimizer = Optimizer::new(vec![Arc::new(FailingRule(calls.clone()))]);
    for sql in [
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "CREATE TABLE new_items (id INTEGER)",
    ] {
        let original = plan(sql);
        let expected = format!("{original:?}");
        let optimized = optimizer
            .optimize_with_observer(original, |_, _| panic!("command observed"))
            .unwrap();
        assert!(matches!(optimized, PlannedStatement::Command(_)));
        assert_eq!(format!("{optimized:?}"), expected);
    }
    assert!(matches!(
        optimizer
            .optimize(PlannedStatement::Command(Command::Begin))
            .unwrap(),
        PlannedStatement::Command(Command::Begin)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn empty_rule_list_does_not_install_implicit_rewrites() {
    for sql in ["SELECT 1 + 2 AS value", "INSERT INTO items VALUES (1 + 2)"] {
        let original = plan(sql);
        let expected = relational(&original).clone();
        let optimized = Optimizer::new(vec![])
            .optimize_with_observer(original, |_, _| panic!("empty pipeline observed"))
            .unwrap();
        assert_eq!(relational(&optimized), &expected);
        assert!(
            relational(&optimized)
                .display_indent()
                .to_string()
                .contains(" + ")
        );
    }
}

#[test]
fn rule_errors_are_propagated_not_skipped() {
    let calls = Arc::new(AtomicUsize::new(0));
    let error = Optimizer::new(vec![Arc::new(FailingRule(calls.clone()))])
        .optimize(plan("SELECT id FROM items"))
        .unwrap_err();
    assert!(matches!(error, OptimizeError::DataFusion(_)));
    assert!(
        error.to_string().contains("optimization-test-marker"),
        "{error}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn update_and_delete_are_rejected_before_rules_run() {
    let calls = Arc::new(AtomicUsize::new(0));
    let optimizer = Optimizer::new(vec![Arc::new(FailingRule(calls.clone()))]);
    for sql in ["UPDATE items SET id = id + 1", "DELETE FROM items"] {
        let error = optimizer
            .optimize_with_observer(plan(sql), |_, _| {
                panic!("unsupported modification observed")
            })
            .unwrap_err();
        assert!(matches!(error, OptimizeError::Plan(_)), "{error}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn invalid_query_metadata_is_rejected_before_rules_run() {
    let calls = Arc::new(AtomicUsize::new(0));
    let optimizer = Optimizer::new(vec![Arc::new(FailingRule(calls.clone()))]);
    let mut original = plan("SELECT 1");
    let PlannedStatement::Query { plan: input, .. } = &mut original else {
        panic!()
    };
    *input = relational(&plan("SELECT TRUE")).clone();
    let error = optimizer.optimize(original).unwrap_err();
    assert!(matches!(error, OptimizeError::Plan(_)), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[derive(Debug)]
struct CheckContext;
impl OptimizerRule for CheckContext {
    fn name(&self) -> &str {
        "check_context"
    }
    fn rewrite(
        &self,
        plan: LogicalPlan,
        config: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>, DataFusionError> {
        assert!(config.query_execution_start_time().is_none());
        assert!(!config.options().optimizer.skip_failed_rules);
        Ok(Transformed::no(plan))
    }
}

#[test]
fn context_disables_clock_folding_and_unchanged_plans_stop_early() {
    let mut observations = 0;
    Optimizer::new(vec![Arc::new(CheckContext)])
        .optimize_with_observer(plan("SELECT 1"), |_, _| observations += 1)
        .unwrap();
    assert_eq!(observations, 1);
}

#[derive(Debug)]
struct RemoveRoot;
impl OptimizerRule for RemoveRoot {
    fn name(&self) -> &str {
        "remove_root"
    }
    fn rewrite(
        &self,
        plan: LogicalPlan,
        _: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>, DataFusionError> {
        Ok(Transformed::yes(LogicalPlan::EmptyRelation(
            EmptyRelation {
                produce_one_row: false,
                schema: plan.schema().clone(),
            },
        )))
    }
}

#[test]
fn final_validation_rejects_same_schema_insert_root_replacement() {
    let mut observed = false;
    let error = Optimizer::new(vec![Arc::new(RemoveRoot)])
        .optimize_with_observer(plan("INSERT INTO items VALUES (1)"), |plan, rule| {
            observed = true;
            assert_eq!(rule.name(), "remove_root");
            assert!(matches!(plan, LogicalPlan::EmptyRelation(_)));
        })
        .unwrap_err();
    assert!(observed);
    assert!(matches!(error, OptimizeError::Plan(_)), "{error}");
    assert!(error.to_string().contains("INSERT root"), "{error}");
}

#[derive(Debug)]
struct AddTrueFilter(Arc<AtomicUsize>);
impl OptimizerRule for AddTrueFilter {
    fn name(&self) -> &str {
        "add_true_filter"
    }
    // Default apply_order(None) invokes this once per pass, not once per node.
    fn rewrite(
        &self,
        plan: LogicalPlan,
        _: &dyn OptimizerConfig,
    ) -> Result<Transformed<LogicalPlan>, DataFusionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Transformed::yes(LogicalPlan::Filter(Filter::try_new(
            lit(true),
            Arc::new(plan),
        )?)))
    }
}

#[test]
fn default_and_custom_pass_limits_bound_non_converging_rules() {
    for limit in [None, Some(1), Some(5)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut optimizer = Optimizer::new(vec![Arc::new(AddTrueFilter(calls.clone()))]);
        if let Some(limit) = limit {
            optimizer = optimizer.with_max_passes(NonZeroU8::new(limit).unwrap());
        }
        let mut observations = 0;
        let optimized = optimizer
            .optimize_with_observer(plan("SELECT id FROM items"), |_, rule| {
                assert_eq!(rule.name(), "add_true_filter");
                observations += 1;
            })
            .unwrap();
        let expected = usize::from(limit.unwrap_or(3));
        assert_eq!(calls.load(Ordering::SeqCst), expected);
        assert_eq!(observations, expected);
        assert_eq!(
            relational(&optimized)
                .display_indent()
                .to_string()
                .matches("Filter:")
                .count(),
            expected
        );
    }
}
