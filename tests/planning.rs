//! Parse -> bind -> native DataFusion contracts (no execution).
use arrow_schema::DataType;
use chilidb::{
    binder::{Binder, Catalog, ColumnSchema, LogicalType, TableSchema, TableSource, bound},
    parser,
    planner::{
        Command, LogicalPlan, Modification, ModifyTable, OutputSchema, PlannedStatement, Planner,
    },
};
use datafusion_common::ExprSchema;
use datafusion_expr::{Expr, logical_plan::InvariantLevel};
use std::sync::Arc;

#[derive(Debug)]
struct Source;
impl TableSource for Source {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::new(TableSchema {
            name: "t".into(),
            columns: [
                ("a", LogicalType::Int32, false),
                ("b", LogicalType::Int32, true),
                ("label", LogicalType::Text, true),
                ("flag", LogicalType::Boolean, false),
            ]
            .into_iter()
            .map(|(name, data_type, nullable)| ColumnSchema {
                name: name.into(),
                data_type,
                nullable,
            })
            .collect(),
        })
    }
}
struct FakeCatalog;
impl Catalog for FakeCatalog {
    type Error = std::convert::Infallible;
    fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
        Ok((name == "t").then(|| Arc::new(Source) as Arc<dyn TableSource>))
    }
}
fn bind(sql: &str) -> bound::Statement<'_> {
    let parsed = parser::parse_sql(sql).unwrap();
    assert_eq!(parsed.len(), 1);
    Binder::new(&FakeCatalog).bind(&parsed[0]).unwrap()
}
fn plan(sql: &str) -> PlannedStatement<'_> {
    Planner::new()
        .plan(&bind(sql))
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}
fn query(sql: &str) -> (LogicalPlan, OutputSchema<'_>) {
    let PlannedStatement::Query { plan, output } = plan(sql) else {
        panic!("expected query")
    };
    validate(&plan);
    assert_eq!(plan.schema().fields().len(), output.fields.len());
    (plan, output)
}
fn validate(p: &LogicalPlan) {
    p.check_invariants(InvariantLevel::Executable)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display_indent_schema()));
    for input in p.inputs() {
        validate(input);
    }
}
fn projection(p: &LogicalPlan) -> &datafusion_expr::logical_plan::Projection {
    let LogicalPlan::Projection(p) = p else {
        panic!("expected projection: {p:?}")
    };
    p
}
fn unalias(mut e: &Expr) -> &Expr {
    loop {
        match e {
            Expr::Alias(a) => e = &a.expr,
            Expr::Cast(c) => e = &c.expr,
            _ => return e,
        }
    }
}
fn nodes<'a>(p: &'a LogicalPlan, out: &mut Vec<&'a LogicalPlan>) {
    out.push(p);
    for input in p.inputs() {
        nodes(input, out);
    }
}
fn tree(p: &LogicalPlan) -> Vec<&LogicalPlan> {
    let mut out = vec![];
    nodes(p, &mut out);
    out
}
fn modify_table(sql: &str) -> LogicalPlan {
    let PlannedStatement::ModifyTable { plan } = plan(sql) else {
        panic!("expected modification")
    };
    validate(&plan);
    assert_eq!(plan.schema().fields().len(), 1);
    let f = plan.schema().field(0);
    assert_eq!(f.name(), "affected_rows");
    assert_eq!(f.data_type(), &DataType::Int64);
    assert!(!f.is_nullable());
    plan
}
fn extension<T: 'static>(p: &LogicalPlan) -> &T {
    let LogicalPlan::Extension(e) = p else {
        panic!("expected extension")
    };
    e.node.as_any().downcast_ref().unwrap()
}

#[test]
fn constant_select_uses_single_empty_row() {
    let (p, o) = query("SELECT 1 AS one, 'hello' AS greeting");
    assert_eq!(
        o.fields.iter().map(|f| f.name.as_ref()).collect::<Vec<_>>(),
        ["one", "greeting"]
    );
    let input = &projection(&p).input;
    assert!(input.schema().fields().is_empty());
    assert!(matches!(input.as_ref(), LogicalPlan::EmptyRelation(e) if e.produce_one_row));
}
#[test]
fn where_and_reordered_projection_preserve_source_lineage() {
    let (p, o) = query("SELECT label AS name, a, b FROM t WHERE flag AND b > 1");
    for (f, i) in o.fields.iter().zip([2, 0, 1]) {
        assert_eq!(f.origin.unwrap().column_index, i);
    }
    assert_eq!(
        o.fields.iter().map(|f| f.nullable).collect::<Vec<_>>(),
        [true, false, true]
    );
    let LogicalPlan::Filter(f) = projection(&p).input.as_ref() else {
        panic!()
    };
    let LogicalPlan::TableScan(scan) = f.input.as_ref() else {
        panic!()
    };
    let source = scan
        .source
        .downcast_ref::<chilidb::planner::table_source::TableAdapter>()
        .unwrap();
    assert!(!source.with_ctid());
    assert!(
        scan.projected_schema
            .fields()
            .iter()
            .all(|f| f.name() != "__ctid")
    );
    assert_eq!(source.table().source.schema().columns[2].name, "label");
    assert_eq!(scan.projected_schema.field(0).name(), "__c0");
    for (e, i) in projection(&p).expr.iter().zip([2, 0, 1]) {
        let Expr::Column(c) = unalias(e) else {
            panic!()
        };
        assert_eq!(c.name, format!("__c{i}"));
        assert_eq!(c.relation.as_ref().unwrap().to_string(), "__r0");
    }
}
#[test]
fn hidden_ctid_is_not_a_sql_column() {
    for sql in ["SELECT ctid FROM t", "SELECT __ctid FROM t"] {
        let parsed = parser::parse_sql(sql).unwrap();
        assert!(matches!(
            Binder::new(&FakeCatalog).bind(&parsed[0]),
            Err(chilidb::binder::BindError::UnknownColumn { .. })
        ));
    }
}

#[test]
fn duplicate_sql_names_are_external_metadata_only() {
    let (p, o) = query("SELECT a AS same, b AS same, a AS same FROM t");
    assert!(o.fields.iter().all(|f| f.name == "same"));
    let names: std::collections::HashSet<_> =
        p.schema().fields().iter().map(|f| f.name()).collect();
    assert_eq!(names.len(), 3);
}
#[test]
fn global_aggregate_is_shared_across_having_and_order() {
    let (p, _) =
        query("SELECT SUM(a) AS total, SUM(a) FROM t WHERE flag HAVING SUM(a) > 0 ORDER BY total");
    let pr = projection(&p);
    assert_eq!(unalias(&pr.expr[0]), unalias(&pr.expr[1]));
    let LogicalPlan::Sort(s) = pr.input.as_ref() else {
        panic!()
    };
    let LogicalPlan::Filter(h) = s.input.as_ref() else {
        panic!()
    };
    let LogicalPlan::Aggregate(a) = h.input.as_ref() else {
        panic!()
    };
    assert!(a.group_expr.is_empty());
    assert_eq!(a.aggr_expr.len(), 1);
    assert!(matches!(a.input.as_ref(), LogicalPlan::Filter(_)));
}
#[test]
fn compound_group_key_reused_inside_larger_expression() {
    let (p, _) = query("SELECT a + b AS k, (a + b) * 2 FROM t GROUP BY a + b ORDER BY k");
    let pr = projection(&p);
    let Expr::BinaryExpr(e) = unalias(&pr.expr[1]) else {
        panic!()
    };
    assert_eq!(unalias(&pr.expr[0]), unalias(&e.left));
    let all = tree(&p);
    let aggregates: Vec<_> = all
        .iter()
        .filter_map(|p| {
            if let LogicalPlan::Aggregate(a) = p {
                Some(a)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(aggregates.len(), 1);
    assert_eq!(aggregates[0].group_expr.len(), 1);
    assert!(aggregates[0].aggr_expr.is_empty());
}
#[test]
fn hidden_order_keys_do_not_leak_into_output() {
    for sql in [
        "SELECT a FROM t ORDER BY label",
        "SELECT 1 AS one FROM t ORDER BY COUNT(*)",
        "SELECT a FROM t ORDER BY ROW_NUMBER() OVER ()",
    ] {
        let (p, o) = query(sql);
        assert_eq!(o.fields.len(), 1);
        assert!(matches!(
            projection(&p).input.as_ref(),
            LogicalPlan::Sort(_)
        ));
    }
}
#[test]
fn nested_aggregate_window_reads_grouped_output() {
    let (p, _) = query("SELECT SUM(SUM(a)) OVER () FROM t");
    let LogicalPlan::Window(w) = projection(&p).input.as_ref() else {
        panic!()
    };
    let LogicalPlan::Aggregate(a) = w.input.as_ref() else {
        panic!()
    };
    assert_eq!(w.window_expr.len(), 1);
    assert_eq!(a.aggr_expr.len(), 1);
    assert!(a.group_expr.is_empty());
    let Expr::WindowFunction(f) = unalias(&w.window_expr[0]) else {
        panic!()
    };
    let Expr::Column(c) = unalias(&f.params.args[0]) else {
        panic!()
    };
    assert!(a.schema.index_of_column(c).is_ok());
}
#[test]
fn window_alias_and_repeated_order_call_share_one_result() {
    let (p, o) = query(
        "SELECT ROW_NUMBER() OVER (ORDER BY b) AS n, ROW_NUMBER() OVER (ORDER BY b) FROM t ORDER BY n, ROW_NUMBER() OVER (ORDER BY b)",
    );
    let pr = projection(&p);
    assert_eq!(unalias(&pr.expr[0]), unalias(&pr.expr[1]));
    let count: usize = tree(&p)
        .iter()
        .map(|p| {
            if let LogicalPlan::Window(w) = p {
                w.window_expr.len()
            } else {
                0
            }
        })
        .sum();
    assert_eq!(count, 1);
    assert!(o.fields.iter().all(|f| f.data_type == LogicalType::Int64));
    assert!(
        p.schema()
            .fields()
            .iter()
            .all(|f| f.data_type() == &DataType::Int64)
    );
}
#[test]
fn windows_partition_order_and_filter_can_read_aggregates() {
    for sql in [
        "SELECT SUM(SUM(b)) OVER (PARTITION BY a ORDER BY COUNT(*)) FROM t GROUP BY a",
        "SELECT COUNT(*) OVER (PARTITION BY SUM(a) ORDER BY COUNT(*)) FROM t",
        "SELECT SUM(SUM(a)) FILTER (WHERE COUNT(*) > 0) OVER () FROM t",
    ] {
        let (p, _) = query(sql);
        let LogicalPlan::Window(w) = projection(&p).input.as_ref() else {
            panic!()
        };
        assert_eq!(w.window_expr.len(), 1);
        assert!(matches!(w.input.as_ref(), LogicalPlan::Aggregate(_)));
    }
}
#[test]
fn count_distinct_filter_retains_types_and_modifiers() {
    let (p, o) = query("SELECT COUNT(DISTINCT b) FILTER (WHERE flag) FROM t");
    assert_eq!(
        (&o.fields[0].data_type, o.fields[0].nullable),
        (&LogicalType::Int64, false)
    );
    let LogicalPlan::Aggregate(a) = projection(&p).input.as_ref() else {
        panic!()
    };
    let Expr::AggregateFunction(f) = unalias(&a.aggr_expr[0]) else {
        panic!()
    };
    assert!(f.params.distinct);
    assert!(f.params.filter.is_some());
    assert_eq!(f.func.name(), "count");
}
#[test]
fn empty_input_global_aggregate_remains_global() {
    let (p, o) = query("SELECT COUNT(*), SUM(a) FROM t WHERE FALSE");
    let LogicalPlan::Aggregate(a) = projection(&p).input.as_ref() else {
        panic!()
    };
    assert!(a.group_expr.is_empty());
    assert_eq!(a.aggr_expr.len(), 2);
    assert!(matches!(a.input.as_ref(), LogicalPlan::Filter(_)));
    assert!(!o.fields[0].nullable);
    assert!(o.fields[1].nullable);
}
#[test]
fn modifications_keep_filtered_identity_and_read_original_row() {
    let p = modify_table("UPDATE t SET b = a, a = a + 1 WHERE flag");
    let u = extension::<ModifyTable>(&p);
    let Modification::Update { ctid, assignments } = u.operation() else {
        panic!("expected update")
    };
    assert_eq!(
        assignments
            .iter()
            .map(|a| a.target_column_index)
            .collect::<Vec<_>>(),
        [1, 0]
    );
    let input = u.input();
    let pr = projection(input);
    assert!(matches!(pr.input.as_ref(), LogicalPlan::Filter(_)));
    for a in assignments {
        assert!(input.schema().index_of_column(&a.value).is_ok());
    }
    let rhs: Vec<_> = pr
        .expr
        .iter()
        .filter(|e| matches!(e, Expr::Alias(a) if a.name.starts_with("__new")))
        .collect();
    assert_eq!(rhs.len(), 2);
    let Expr::Column(original) = unalias(rhs[0]) else {
        panic!()
    };
    let Expr::BinaryExpr(increment) = unalias(rhs[1]) else {
        panic!()
    };
    assert_eq!(unalias(&increment.left), &Expr::Column(original.clone()));
    let field = input.schema().field_from_column(ctid).unwrap();
    assert_eq!(field.data_type(), &DataType::FixedSizeBinary(6));
    assert!(!field.is_nullable());
    let p = modify_table("DELETE FROM t WHERE b IS NULL");
    let d = extension::<ModifyTable>(&p);
    let Modification::Delete { ctid } = d.operation() else {
        panic!("expected delete")
    };
    assert!(
        tree(d.input())
            .iter()
            .any(|p| matches!(p, LogicalPlan::Filter(_)))
    );
    assert!(d.input().schema().index_of_column(ctid).is_ok());
}
#[test]
fn insert_normalizes_full_width_typed_nulls() {
    let p = modify_table("INSERT INTO t (flag, a) VALUES (TRUE, 1), (FALSE, 2)");
    let i = extension::<ModifyTable>(&p);
    let Modification::Insert { values } = i.operation() else {
        panic!("expected insert")
    };
    assert_eq!(values.len(), 4);
    assert_eq!(i.input().schema().fields().len(), 4);
    let LogicalPlan::Values(v) = i.input() else {
        panic!()
    };
    assert_eq!(v.values.len(), 2);
    for (idx, ty) in [(1, DataType::Int32), (2, DataType::Utf8)] {
        assert_eq!(v.schema.field(idx).data_type(), &ty);
        assert!(v.schema.field(idx).is_nullable());
    }
}
#[test]
fn insert_values_describe_inputs_not_target_constraints_or_lineage() {
    let p = modify_table("INSERT INTO t (a, flag) VALUES (NULL, TRUE), (1, FALSE)");
    let i = extension::<ModifyTable>(&p);
    let Modification::Insert { values } = i.operation() else {
        panic!("expected insert")
    };
    assert_eq!(values.len(), 4);
    assert!(i.input().schema().field(0).is_nullable());
    assert!(
        i.input()
            .schema()
            .fields()
            .iter()
            .all(|f| f.metadata().is_empty())
    );
    assert!(
        !tree(i.input())
            .iter()
            .any(|p| matches!(p, LogicalPlan::TableScan(_)))
    );
}
#[test]
fn schema_and_transaction_commands_are_preserved() {
    let sql = "CREATE TABLE new_table (id INTEGER PRIMARY KEY, value TEXT)";
    let bound::Statement::CreateTable(expected) = bind(sql) else {
        panic!()
    };
    let PlannedStatement::Command(Command::CreateTable(actual)) = plan(sql) else {
        panic!()
    };
    assert_eq!(actual, expected);
    assert!(matches!(
        plan("BEGIN"),
        PlannedStatement::Command(Command::Begin)
    ));
    assert!(matches!(
        plan("COMMIT"),
        PlannedStatement::Command(Command::Commit)
    ));
    assert!(matches!(
        plan("ROLLBACK"),
        PlannedStatement::Command(Command::Rollback)
    ));
}
#[test]
fn invalid_bound_column_and_assignment_indices_return_errors() {
    for bad in [
        bound::ColumnBinding {
            relation: bound::RelationId(u32::MAX),
            column_index: 0,
        },
        bound::ColumnBinding {
            relation: bound::RelationId(0),
            column_index: usize::MAX,
        },
    ] {
        let mut s = bind("SELECT a FROM t");
        let bound::Statement::Select(q) = &mut s else {
            panic!()
        };
        q.projection[0].expr.kind = bound::ExprKind::Column(bad);
        assert!(Planner::new().plan(&s).is_err());
    }
    let mut s = bind("UPDATE t SET a = 1");
    let bound::Statement::Update(u) = &mut s else {
        panic!()
    };
    u.assignments[0].column_index = usize::MAX;
    assert!(Planner::new().plan(&s).is_err());
}
#[test]
fn native_plan_owns_sql_text_after_statement_and_sql_are_dropped() {
    let native = {
        let sql = String::from("SELECT 'owned' AS alias");
        let (p, _) = query(&sql);
        p.clone()
    };
    validate(&native);
    assert!(native.display_indent().to_string().contains("owned"));
}
