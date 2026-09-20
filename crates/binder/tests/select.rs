//! End-to-end semantic checks: parse SQL, then bind against immutable metadata.
use std::{borrow::Cow, sync::Arc};

use chilidb_binder::{
    BindError, Binder, Catalog, ColumnSchema, LogicalType, Scalar, TableSchema, TableSource, bound,
};
use chilidb_parser::{BinaryOp, UnaryOp, parse_sql};

#[derive(Debug)]
struct Source(Arc<TableSchema>);

impl TableSource for Source {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.0)
    }
}

struct TestCatalog {
    table: Arc<dyn TableSource>,
    duplicate: Arc<dyn TableSource>,
}

impl Default for TestCatalog {
    fn default() -> Self {
        use LogicalType::*;
        let columns: Vec<_> = [
            ("id", Int32, false),
            ("qty", Uint32, true),
            ("price", Float64, true),
            ("name", Text, false),
            ("flag", Boolean, true),
            ("small", Float32, false),
        ]
        .into_iter()
        .map(|(name, data_type, nullable)| ColumnSchema {
            name: name.into(),
            data_type,
            nullable,
        })
        .collect();
        Self {
            duplicate: Arc::new(Source(Arc::new(TableSchema {
                name: "duplicate".into(),
                columns: vec![columns[0].clone(), columns[0].clone()],
            }))),
            table: Arc::new(Source(Arc::new(TableSchema {
                name: "t".into(),
                columns,
            }))),
        }
    }
}

impl Catalog for TestCatalog {
    type Error = &'static str;

    fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
        match name {
            "t" => Ok(Some(Arc::clone(&self.table))),
            "duplicate" => Ok(Some(Arc::clone(&self.duplicate))),
            "broken" => Err("metadata unavailable"),
            _ => Ok(None),
        }
    }
}

fn select<'sql>(binder: &Binder<'_, TestCatalog>, sql: &'sql str) -> bound::Select<'sql> {
    let parsed = parse_sql(sql).unwrap();
    let bound::Statement::Select(result) = binder.bind(&parsed[0]).unwrap() else {
        panic!("expected SELECT")
    };
    result
}

fn error(binder: &Binder<'_, TestCatalog>, sql: &str) -> BindError<&'static str> {
    binder.bind(&parse_sql(sql).unwrap()[0]).unwrap_err()
}

fn assert_column(expr: &bound::Expr<'_>, index: usize) {
    assert_eq!(
        expr.kind,
        bound::ExprKind::Column(bound::ColumnBinding {
            relation: bound::RelationId(0),
            column_index: index,
        })
    );
}

#[test]
fn columns_resolve_by_schema_index_and_keep_actual_output_names() {
    let catalog = TestCatalog::default();
    let binder = Binder { catalog: &catalog };
    let query = select(&binder, "SELECT t.name, ID, qty, id + 1 FROM T");
    let source = query.source.unwrap();
    assert_eq!(source.relation, bound::RelationId(0));
    assert!(Arc::ptr_eq(&source.source, &catalog.table));
    for (output, index) in query.projection.iter().zip([3, 0, 1]) {
        let schema = catalog.table.schema();
        assert_column(&output.expr, index);
        assert_eq!(output.name, schema.columns[index].name);
        assert_eq!(output.expr.data_type, schema.columns[index].data_type);
        assert_eq!(output.expr.nullable, schema.columns[index].nullable);
    }
    assert_eq!(query.projection[3].name, "?column?");
    assert!(query.filter.is_none());
}

#[test]
fn wildcard_expands_in_schema_order_and_retains_source_identity() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    let query = select(&binder, "SELECT * FROM t");
    assert!(Arc::ptr_eq(&query.source.unwrap().source, &catalog.table));
    let schema = catalog.table.schema();
    assert_eq!(query.projection.len(), schema.columns.len());
    for (index, (output, column)) in query.projection.iter().zip(&schema.columns).enumerate() {
        assert_eq!(output.name, column.name);
        assert_eq!(output.expr.data_type, column.data_type);
        assert_eq!(output.expr.nullable, column.nullable);
        assert_column(&output.expr, index);
    }
    assert_eq!(error(&binder, "SELECT *"), BindError::InvalidWildcard);
}

#[test]
fn resolution_errors_distinguish_missing_names_ambiguity_and_catalog_failure() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    for (sql, qualifier, name) in [
        ("SELECT absent FROM t", None, "absent"),
        ("SELECT t.absent FROM t", Some("t"), "absent"),
        ("SELECT other.id FROM t", Some("other"), "id"),
        ("SELECT id", None, "id"),
        ("SELECT t.id", Some("t"), "id"),
    ] {
        assert_eq!(
            error(&binder, sql),
            BindError::UnknownColumn {
                qualifier: qualifier.map(str::to_owned),
                name: name.into(),
            },
            "{sql}"
        );
    }
    for (sql, qualifier) in [
        ("SELECT id FROM duplicate", None),
        ("SELECT duplicate.id FROM duplicate", Some("duplicate")),
    ] {
        assert_eq!(
            error(&binder, sql),
            BindError::AmbiguousColumn {
                qualifier: qualifier.map(str::to_owned),
                name: "id".into(),
            }
        );
    }
    assert_eq!(
        error(&binder, "SELECT * FROM missing"),
        BindError::UnknownTable("missing".into())
    );
    assert_eq!(
        error(&binder, "SELECT * FROM broken"),
        BindError::Catalog("metadata unavailable")
    );
}

#[test]
fn binder_reuse_starts_a_fresh_query_context_even_after_errors() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    for _ in 0..3 {
        let query = select(&binder, "SELECT id FROM t");
        assert_eq!(query.source.unwrap().relation, bound::RelationId(0));
        assert_column(&query.projection[0].expr, 0);
        assert!(matches!(
            error(&binder, "SELECT id"),
            BindError::UnknownColumn { .. }
        ));
        assert!(select(&binder, "SELECT 1").source.is_none());
    }
}

#[test]
fn where_accepts_boolean_and_explicitly_casts_null_but_rejects_other_types() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    for sql in ["SELECT 1 WHERE TRUE", "SELECT id FROM t WHERE flag"] {
        let predicate = select(&binder, sql).filter.unwrap();
        assert_eq!(predicate.data_type, LogicalType::Boolean);
    }
    let predicate = select(&binder, "SELECT 1 WHERE NULL").filter.unwrap();
    assert_eq!(predicate.data_type, LogicalType::Boolean);
    assert!(predicate.nullable);
    let bound::ExprKind::Cast { expr } = predicate.kind else {
        panic!("NULL needs a contextual cast")
    };
    assert_eq!(expr.data_type, LogicalType::Null);
    assert_eq!(expr.kind, bound::ExprKind::Literal(Scalar::Null));
    for (sql, expected_type) in [
        ("SELECT 1 WHERE 1", LogicalType::Int32),
        ("SELECT 1 WHERE 'yes'", LogicalType::Text),
    ] {
        assert!(
            matches!(error(&binder, sql), BindError::TypeMismatch { actual, .. } if actual == expected_type)
        );
    }
}

#[test]
fn transaction_commands_require_no_catalog_lookup_or_transaction_state() {
    struct NoLookup;
    impl Catalog for NoLookup {
        type Error = std::convert::Infallible;
        fn get_table(&self, _: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
            panic!("transaction binding must not query the catalog")
        }
    }
    let binder = Binder::new(&NoLookup);
    let parsed = parse_sql("BEGIN; COMMIT; ROLLBACK; COMMIT;").unwrap();
    assert!(matches!(
        binder.bind(&parsed[0]).unwrap(),
        bound::Statement::Begin
    ));
    assert!(matches!(
        binder.bind(&parsed[1]).unwrap(),
        bound::Statement::Commit
    ));
    assert!(matches!(
        binder.bind(&parsed[2]).unwrap(),
        bound::Statement::Rollback
    ));
    assert!(matches!(
        binder.bind(&parsed[3]).unwrap(),
        bound::Statement::Commit
    ));
}

#[test]
fn bound_literals_outlive_parsed_ast_and_borrow_unescaped_sql_text() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    let sql = String::from("SELECT 'hello', 'it''s', 42, 2147483648, 1.25, TRUE, NULL");
    let parsed = parse_sql(&sql).unwrap();
    let bound::Statement::Select(query) = binder.bind(&parsed[0]).unwrap() else {
        panic!("expected SELECT")
    };
    drop(parsed);
    let bound::ExprKind::Literal(Scalar::String(Cow::Borrowed(text))) =
        &query.projection[0].expr.kind
    else {
        panic!("unescaped SQL text should remain borrowed")
    };
    assert_eq!(*text, "hello");
    assert_eq!(text.as_ptr(), sql[8..13].as_ptr());
    assert_eq!(
        query.projection[1].expr.kind,
        bound::ExprKind::Literal(Scalar::String(Cow::Owned("it's".into())))
    );
    for (index, data_type, value) in [
        (2, LogicalType::Int32, Scalar::Int32(42)),
        (3, LogicalType::Int64, Scalar::Int64(2147483648)),
        (4, LogicalType::Float64, Scalar::Float64(1.25)),
        (5, LogicalType::Boolean, Scalar::Boolean(true)),
        (6, LogicalType::Null, Scalar::Null),
    ] {
        assert_eq!(query.projection[index].expr.data_type, data_type);
        assert_eq!(
            query.projection[index].expr.kind,
            bound::ExprKind::Literal(value)
        );
        assert_eq!(query.projection[index].expr.nullable, index == 6);
    }
}

#[test]
fn numeric_common_types_insert_casts_without_losing_column_bindings() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    for (sql, common, left_type, right_type, left_index, right_index) in [
        (
            "SELECT id + qty FROM t",
            LogicalType::Int64,
            LogicalType::Int32,
            LogicalType::Uint32,
            0,
            1,
        ),
        (
            "SELECT small + id FROM t",
            LogicalType::Float64,
            LogicalType::Float32,
            LogicalType::Int32,
            5,
            0,
        ),
        (
            "SELECT small + price FROM t",
            LogicalType::Float64,
            LogicalType::Float32,
            LogicalType::Float64,
            5,
            2,
        ),
    ] {
        let query = select(&binder, sql);
        let expr = &query.projection[0].expr;
        assert_eq!(expr.data_type, common);
        assert_eq!(expr.nullable, right_index != 0);
        let bound::ExprKind::Binary { op, left, right } = &expr.kind else {
            panic!("expected binary")
        };
        assert_eq!(*op, BinaryOp::Add);
        for (operand, original_type, index) in [
            (left, left_type, left_index),
            (right, right_type, right_index),
        ] {
            assert_eq!(operand.data_type, common);
            if original_type == common {
                assert_column(operand, index);
            } else {
                let bound::ExprKind::Cast { expr } = &operand.kind else {
                    panic!("numeric widening must be explicit")
                };
                assert_eq!(expr.data_type, original_type);
                assert_column(expr, index);
            }
        }
    }
    assert!(matches!(
        error(&binder, "SELECT name + id FROM t"),
        BindError::TypeMismatch { .. } | BindError::IncompatibleTypes { .. }
    ));
}

#[test]
fn operator_grouping_and_nullability_survive_binding() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    let query = select(
        &binder,
        "SELECT 1 + 2 * 3, -id, NOT flag, qty IS NULL, qty IS NOT NULL, id = qty, flag AND TRUE OR FALSE FROM t",
    );
    let arithmetic = &query.projection[0].expr;
    assert_eq!(arithmetic.data_type, LogicalType::Int32);
    assert!(!arithmetic.nullable);
    let bound::ExprKind::Binary {
        op: BinaryOp::Add,
        right,
        ..
    } = &arithmetic.kind
    else {
        panic!("expected addition")
    };
    assert!(matches!(
        right.kind,
        bound::ExprKind::Binary {
            op: BinaryOp::Multiply,
            ..
        }
    ));
    assert!(matches!(
        query.projection[1].expr.kind,
        bound::ExprKind::Unary {
            op: UnaryOp::Minus,
            ..
        }
    ));
    assert!(!query.projection[1].expr.nullable);
    assert!(matches!(
        query.projection[2].expr.kind,
        bound::ExprKind::Unary {
            op: UnaryOp::Not,
            ..
        }
    ));
    assert!(query.projection[2].expr.nullable);
    for (index, negated) in [(3, false), (4, true)] {
        let expr = &query.projection[index].expr;
        assert_eq!(expr.data_type, LogicalType::Boolean);
        assert!(!expr.nullable);
        assert!(
            matches!(expr.kind, bound::ExprKind::IsNull { negated: actual, .. } if actual == negated)
        );
    }
    assert_eq!(query.projection[5].expr.data_type, LogicalType::Boolean);
    assert!(query.projection[5].expr.nullable);
    let logical = &query.projection[6].expr;
    assert_eq!(logical.data_type, LogicalType::Boolean);
    assert!(logical.nullable);
    let bound::ExprKind::Binary {
        op: BinaryOp::Or,
        left,
        ..
    } = &logical.kind
    else {
        panic!("expected OR")
    };
    assert!(matches!(
        left.kind,
        bound::ExprKind::Binary {
            op: BinaryOp::And,
            ..
        }
    ));
}

#[test]
fn aggregate_signatures_coercions_and_alias_ordering() {
    use bound::{AggregateFunction as A, ExprKind as K};
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    let q = select(
        &binder,
        "SELECT count(*) AS n, sum(qty), avg(id), min(name), max(flag), sum(small), min(NULL) FROM t ORDER BY n DESC, 2",
    );
    assert!(q.is_aggregate);
    assert_eq!(q.projection[0].name, "n");
    assert_eq!(q.order_by[0].expr, q.projection[0].expr);
    assert_eq!(q.order_by[0].nulls, bound::NullOrder::First);
    assert_eq!(q.order_by[1].expr, q.projection[1].expr);
    assert_eq!(q.order_by[1].nulls, bound::NullOrder::Last);
    for (i, (fun, ty, nullable)) in [
        (A::Count, LogicalType::Int64, false),
        (A::Sum, LogicalType::Int64, true),
        (A::Avg, LogicalType::Float64, true),
        (A::Min, LogicalType::Text, true),
        (A::Max, LogicalType::Boolean, true),
        (A::Sum, LogicalType::Float64, true),
        (A::Min, LogicalType::Text, true),
    ]
    .into_iter()
    .enumerate()
    {
        let expr = &q.projection[i].expr;
        assert_eq!(expr.data_type, ty);
        assert_eq!(expr.nullable, nullable);
        let K::Aggregate(a) = &expr.kind else {
            panic!()
        };
        assert_eq!(a.function, fun);
        assert_eq!(a.args.len(), usize::from(i != 0));
    }
    let q = select(
        &binder,
        "SELECT count(DISTINCT name) FILTER (WHERE flag) FROM t",
    );
    let K::Aggregate(a) = &q.projection[0].expr.kind else {
        panic!()
    };
    assert!(a.distinct && a.filter.is_some());
}

#[test]
fn grouping_is_structural_and_reaches_window_inputs() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    for sql in [
        "SELECT t.id + 1, sum(qty) FROM t GROUP BY id",
        "SELECT id + 1 FROM t GROUP BY t.id + 1",
        "SELECT 1 HAVING count(*) > 0",
        "SELECT 1 HAVING true",
        "SELECT sum(sum(qty)) OVER () FROM t",
        "SELECT rank() OVER (ORDER BY sum(qty)) FROM t",
        "SELECT row_number() OVER (PARTITION BY id ORDER BY sum(qty)) FROM t GROUP BY id",
        "SELECT * FROM t GROUP BY id, qty, price, name, flag, small",
        "SELECT 1 FROM t ORDER BY count(*)",
    ] {
        assert!(select(&binder, sql).is_aggregate, "{sql}");
    }
    for sql in [
        "SELECT id, sum(qty) FROM t",
        "SELECT id FROM t GROUP BY id + 1",
        "SELECT id FROM t HAVING true",
        "SELECT * FROM t GROUP BY id",
        "SELECT sum(qty) OVER () FROM t GROUP BY id",
        "SELECT count(*) FROM t ORDER BY id",
        "SELECT row_number() OVER (PARTITION BY qty) FROM t GROUP BY id",
        "SELECT row_number() OVER (ORDER BY qty) FROM t GROUP BY id",
    ] {
        assert_eq!(error(&binder, sql), BindError::UngroupedColumn, "{sql}");
    }
    assert!(
        !select(
            &binder,
            "SELECT sum(qty) OVER (), row_number() OVER () FROM t"
        )
        .is_aggregate
    );
}

#[test]
fn invalid_function_contexts_and_signatures_are_errors() {
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    for sql in [
        "SELECT count()",
        "SELECT sum(*)",
        "SELECT count(DISTINCT *)",
        "SELECT sum(1, 2)",
        "SELECT row_number()",
        "SELECT rank(1) OVER ()",
        "SELECT rank() FILTER (WHERE true) OVER ()",
        "SELECT count(DISTINCT id) OVER () FROM t",
        "SELECT sum(count(*)) FROM t",
        "SELECT count(*) FILTER (WHERE count(*) > 0)",
        "SELECT id FROM t WHERE count(*) > 0",
        "SELECT id FROM t GROUP BY sum(id)",
        "SELECT 1 HAVING row_number() OVER () > 0",
        "SELECT sum(row_number() OVER ()) OVER ()",
        "SELECT row_number() OVER (ORDER BY rank() OVER ())",
        "SELECT sum(row_number() OVER ())",
        "UPDATE t SET id = count(*)",
        "DELETE FROM t WHERE rank() OVER () = 1",
        "INSERT INTO t (id) VALUES (count(*))",
    ] {
        assert!(
            matches!(error(&binder, sql), BindError::InvalidFunction(_)),
            "{sql}"
        );
    }
    for sql in ["SELECT mystery(1)", "SELECT \"COUNT\"(*)"] {
        assert!(matches!(error(&binder, sql), BindError::UnknownFunction(_)));
    }
    assert!(matches!(
        error(&binder, "SELECT sum(name) FROM t"),
        BindError::TypeMismatch { .. }
    ));
    assert!(matches!(
        error(&binder, "SELECT count(*) FILTER (WHERE 1)"),
        BindError::TypeMismatch { .. }
    ));
}

#[test]
fn windows_resolve_defaults_and_validate_frames() {
    use bound::{ExprKind as K, FrameBound as B, FrameUnits as U};
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    for (sql, units, start, end) in [
        (
            "SELECT rank() OVER ()",
            U::Range,
            B::UnboundedPreceding,
            B::UnboundedFollowing,
        ),
        (
            "SELECT rank() OVER (ORDER BY 1)",
            U::Range,
            B::UnboundedPreceding,
            B::CurrentRow,
        ),
        (
            "SELECT sum(1) OVER (ROWS BETWEEN 2 PRECEDING AND 1 FOLLOWING)",
            U::Rows,
            B::Preceding(2),
            B::Following(1),
        ),
        (
            "SELECT sum(1) OVER (ROWS BETWEEN 0 PRECEDING AND CURRENT ROW)",
            U::Rows,
            B::Preceding(0),
            B::CurrentRow,
        ),
    ] {
        let q = select(&binder, sql);
        let K::Window(w) = &q.projection[0].expr.kind else {
            panic!()
        };
        assert_eq!(w.frame, bound::WindowFrame { units, start, end });
    }
    for frame in [
        "ROWS BETWEEN CURRENT ROW AND 0 PRECEDING",
        "ROWS BETWEEN 0 FOLLOWING AND CURRENT ROW",
        "ROWS BETWEEN 0 FOLLOWING AND 0 PRECEDING",
        "ROWS BETWEEN UNBOUNDED FOLLOWING AND UNBOUNDED FOLLOWING",
        "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED PRECEDING",
        "RANGE BETWEEN 1 PRECEDING AND CURRENT ROW",
        "ROWS BETWEEN 18446744073709551616 PRECEDING AND CURRENT ROW",
    ] {
        let sql = format!("SELECT sum(1) OVER ({frame})");
        assert!(
            matches!(error(&binder, &sql), BindError::InvalidWindowFrame(_)),
            "{sql}"
        );
    }
}

#[test]
fn manual_window_trees_preserve_depth_and_offset_validation() {
    use chilidb_parser::{Expr, FrameBound, Statement};
    let catalog = TestCatalog::default();
    let binder = Binder::new(&catalog);
    let mut parsed = parse_sql("SELECT row_number() OVER (PARTITION BY 1)").unwrap();
    let Statement::Select { projection, .. } = &mut parsed[0] else {
        panic!()
    };
    let Expr::FunctionCall {
        over: Some(over), ..
    } = &mut projection[0]
    else {
        panic!()
    };
    let mut nested = over.partition_by.remove(0);
    for _ in 0..256 {
        nested = Expr::Unary {
            op: UnaryOp::Plus,
            expr: Box::new(nested),
        };
    }
    over.partition_by.push(nested);
    assert_eq!(
        binder.bind(&parsed[0]).unwrap_err(),
        BindError::ExpressionTooDeep
    );

    for offset in ["", "-1", "+1", "1.5", "1e2", " 2"] {
        let mut parsed = parse_sql("SELECT sum(1) OVER (ROWS 1 PRECEDING)").unwrap();
        let Statement::Select { projection, .. } = &mut parsed[0] else {
            panic!()
        };
        let Expr::FunctionCall {
            over: Some(over), ..
        } = &mut projection[0]
        else {
            panic!()
        };
        over.frame.as_mut().unwrap().start = FrameBound::Preceding(offset);
        assert!(matches!(
            binder.bind(&parsed[0]),
            Err(BindError::InvalidWindowFrame(_))
        ));
    }
}
