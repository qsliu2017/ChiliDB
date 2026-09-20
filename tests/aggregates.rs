//! End-to-end aggregate/window contract through the root public frontend API.
use std::sync::Arc;

use binder::{Binder, Catalog, ColumnSchema, LogicalType, TableSchema, TableSource, bound};
use chilidb::{binder, parser};

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
fn bind(sql: &str) -> Result<bound::Statement<'_>, String> {
    let statements = parser::parse_sql(sql).map_err(|e| format!("{e:?}"))?;
    assert_eq!(statements.len(), 1, "{sql}");
    Binder::new(&FakeCatalog)
        .bind(&statements[0])
        .map_err(|e| format!("{e:?}"))
}
fn select(sql: &str) -> bound::Select<'_> {
    match bind(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        bound::Statement::Select(select) => select,
        _ => panic!("expected SELECT: {sql}"),
    }
}
fn reject(queries: &[&str]) {
    for sql in queries {
        assert!(bind(sql).is_err(), "unexpectedly accepted: {sql}");
    }
}

#[test]
fn aggregate_types_and_count_star_representation() {
    use bound::AggregateFunction::*;
    let s =
        select("SELECT COUNT(*), COUNT(1), COUNT(b), SUM(a), AVG(a), MIN(label), MAX(a) FROM t");
    assert!(s.is_aggregate);
    for (i, (function, ty, nullable)) in [
        (Count, LogicalType::Int64, false),
        (Count, LogicalType::Int64, false),
        (Count, LogicalType::Int64, false),
        (Sum, LogicalType::Int64, true),
        (Avg, LogicalType::Float64, true),
        (Min, LogicalType::Text, true),
        (Max, LogicalType::Int32, true),
    ]
    .into_iter()
    .enumerate()
    {
        let expr = &s.projection[i].expr;
        assert_eq!(expr.data_type, ty);
        assert_eq!(expr.nullable, nullable);
        let bound::ExprKind::Aggregate(aggregate) = &expr.kind else {
            panic!("expected aggregate");
        };
        assert_eq!(aggregate.function, function);
        assert_eq!(aggregate.args.len(), usize::from(i != 0));
        assert!(!aggregate.distinct);
    }
}

#[test]
fn distinct_filters_aliases_and_group_composition() {
    let s = select(
        "SELECT a + 1 AS grouping_value, COUNT(DISTINCT b) FILTER (WHERE flag) AS n FROM t WHERE flag GROUP BY a HAVING COUNT(*) > 0 ORDER BY n DESC, grouping_value ASC",
    );
    assert!(s.is_aggregate);
    assert_eq!(s.group_by.len(), 1);
    assert!(s.having.is_some());
    assert!(s.filter.is_some());
    assert_eq!(s.projection[0].name, "grouping_value");
    assert_eq!(s.projection[1].name, "n");
    assert_eq!(s.order_by[0].expr, s.projection[1].expr);
    assert_eq!(s.order_by[1].expr, s.projection[0].expr);
    let bound::ExprKind::Aggregate(a) = &s.projection[1].expr.kind else {
        panic!("expected aggregate");
    };
    assert!(a.distinct);
    assert_eq!(a.filter.as_ref().unwrap().data_type, LogicalType::Boolean);
    select("SELECT a + b, (a + b) * 2 FROM t GROUP BY a + b");
    select("SELECT 1, COUNT(*) FROM t");
    select(
        "SELECT \"t\".\"a\" AS \"Key\", SUM(\"t\".\"b\") AS \"Total\" FROM \"t\" GROUP BY \"t\".\"a\" ORDER BY \"Total\", \"Key\"",
    );
    select("SELECT 'a' AS literal, COUNT(*) FROM t ORDER BY literal");
}

#[test]
fn grouping_is_triggered_outside_projection() {
    assert!(select("SELECT 1 FROM t HAVING TRUE").is_aggregate);
    assert!(select("SELECT 1 FROM t ORDER BY COUNT(*)").is_aggregate);
    reject(&[
        "SELECT a FROM t HAVING TRUE",
        "SELECT a FROM t ORDER BY COUNT(*)",
        "SELECT a, COUNT(*) FROM t",
        "SELECT b, SUM(a) FROM t GROUP BY a",
        "SELECT a FROM t GROUP BY a + b",
        "SELECT COUNT(*) FROM t HAVING b > 0",
        "SELECT COUNT(*) FROM t ORDER BY a",
        "SELECT * FROM t GROUP BY a",
    ]);
}

#[test]
fn aggregate_arity_types_and_clause_legality() {
    reject(&[
        "SELECT COUNT() FROM t",
        "SELECT COUNT(a, b) FROM t",
        "SELECT SUM(*) FROM t",
        "SELECT SUM() FROM t",
        "SELECT AVG(a, b) FROM t",
        "SELECT MIN() FROM t",
        "SELECT MAX(a, b) FROM t",
        "SELECT COUNT(DISTINCT *) FROM t",
        "SELECT SUM(label) FROM t",
        "SELECT AVG(flag) FROM t",
        "SELECT SUM(COUNT(*)) FROM t",
        "SELECT COUNT(SUM(a)) FROM t",
        "SELECT COUNT(*) FILTER (WHERE a) FROM t",
        "SELECT COUNT(*) FILTER (WHERE SUM(a) > 0) FROM t",
        "SELECT a FROM t WHERE COUNT(*) > 0",
        "SELECT COUNT(*) FROM t GROUP BY SUM(a)",
        "SELECT COUNT(*) FROM t HAVING 1",
        "UPDATE t SET a = COUNT(*)",
        "UPDATE t SET a = 1 WHERE SUM(b) > 0",
        "DELETE FROM t WHERE COUNT(*) > 0",
        "INSERT INTO t (a) VALUES (SUM(1))",
    ]);
}

#[test]
fn window_alone_does_not_group_and_ranking_is_nonnullable() {
    let s = select(
        "SELECT a, ROW_NUMBER() OVER (), RANK() OVER (ORDER BY b), DENSE_RANK() OVER (PARTITION BY label ORDER BY a), SUM(b) OVER (PARTITION BY label ORDER BY a), COUNT(*) OVER () FROM t ORDER BY ROW_NUMBER() OVER ()",
    );
    assert!(!s.is_aggregate);
    for (i, function) in [
        bound::WindowFunction::RowNumber,
        bound::WindowFunction::Rank,
        bound::WindowFunction::DenseRank,
    ]
    .into_iter()
    .enumerate()
    {
        let expr = &s.projection[i + 1].expr;
        assert_eq!(expr.data_type, LogicalType::Int64);
        assert!(!expr.nullable);
        let bound::ExprKind::Window(w) = &expr.kind else {
            panic!("expected window");
        };
        assert_eq!(w.function, function);
        assert!(w.args.is_empty());
    }
    let bound::ExprKind::Window(w) = &s.projection[4].expr.kind else {
        panic!("expected window");
    };
    assert_eq!(
        w.function,
        bound::WindowFunction::Aggregate(bound::AggregateFunction::Sum)
    );
    assert_eq!(w.partition_by.len(), 1);
    assert_eq!(w.order_by.len(), 1);
    let count = &s.projection[5].expr;
    assert_eq!(count.data_type, LogicalType::Int64);
    assert!(!count.nullable);
    let bound::ExprKind::Window(w) = &count.kind else {
        panic!("expected window");
    };
    assert!(w.args.is_empty());
}

#[test]
fn grouped_aggregates_can_feed_windows_but_raw_columns_cannot() {
    for sql in [
        "SELECT SUM(SUM(a)) OVER () FROM t",
        "SELECT SUM(SUM(a)) FILTER (WHERE COUNT(*) > 0) OVER () FROM t",
        "SELECT COUNT(*) FILTER (WHERE SUM(a) > 0) OVER () FROM t",
        "SELECT ROW_NUMBER() OVER (ORDER BY SUM(a)) FROM t",
        "SELECT COUNT(*) OVER (PARTITION BY SUM(a)) FROM t",
        "SELECT a, SUM(SUM(b)) OVER (PARTITION BY a ORDER BY COUNT(*)) FROM t GROUP BY a",
    ] {
        assert!(select(sql).is_aggregate, "{sql}");
    }
    reject(&[
        "SELECT SUM(a) OVER (), COUNT(*) FROM t",
        "SELECT ROW_NUMBER() OVER (PARTITION BY b), COUNT(*) FROM t GROUP BY a",
        "SELECT ROW_NUMBER() OVER (ORDER BY b), COUNT(*) FROM t GROUP BY a",
        "SELECT COUNT(*) OVER (PARTITION BY a) FROM t HAVING TRUE",
        "SELECT COUNT(*) FILTER (WHERE flag) OVER (), COUNT(*) FROM t",
    ]);
}

#[test]
fn windows_reject_forbidden_contexts_nesting_and_distinct() {
    reject(&[
        "SELECT a FROM t WHERE ROW_NUMBER() OVER () > 1",
        "SELECT a FROM t GROUP BY ROW_NUMBER() OVER ()",
        "SELECT COUNT(*) FROM t HAVING ROW_NUMBER() OVER () > 1",
        "SELECT SUM(ROW_NUMBER() OVER ()) FROM t",
        "SELECT SUM(ROW_NUMBER() OVER ()) OVER () FROM t",
        "SELECT ROW_NUMBER() OVER (PARTITION BY RANK() OVER ()) FROM t",
        "SELECT ROW_NUMBER() OVER (ORDER BY RANK() OVER ()) FROM t",
        "SELECT COUNT(*) FILTER (WHERE ROW_NUMBER() OVER () > 0) FROM t",
        "SELECT COUNT(*) FILTER (WHERE ROW_NUMBER() OVER () > 0) OVER () FROM t",
        "SELECT COUNT(DISTINCT a) OVER () FROM t",
        "SELECT SUM(DISTINCT a) OVER () FROM t",
        "SELECT ROW_NUMBER(a) OVER () FROM t",
        "SELECT RANK(1) OVER () FROM t",
        "SELECT DENSE_RANK() FROM t",
        "SELECT ROW_NUMBER() FILTER (WHERE flag) OVER () FROM t",
        "UPDATE t SET a = ROW_NUMBER() OVER ()",
        "DELETE FROM t WHERE RANK() OVER () > 0",
        "INSERT INTO t (a) VALUES (ROW_NUMBER() OVER ())",
    ]);
}

#[test]
fn default_and_explicit_window_frames_are_preserved() {
    use bound::{FrameBound::*, FrameUnits};
    for (sql, units, start, end) in [
        (
            "SELECT SUM(a) OVER () FROM t",
            FrameUnits::Range,
            UnboundedPreceding,
            UnboundedFollowing,
        ),
        (
            "SELECT SUM(a) OVER (ORDER BY b) FROM t",
            FrameUnits::Range,
            UnboundedPreceding,
            CurrentRow,
        ),
        (
            "SELECT SUM(a) OVER (ORDER BY b ROWS 3 PRECEDING) FROM t",
            FrameUnits::Rows,
            Preceding(3),
            CurrentRow,
        ),
        (
            "SELECT SUM(a) OVER (ORDER BY b ROWS BETWEEN 5 PRECEDING AND 2 PRECEDING) FROM t",
            FrameUnits::Rows,
            Preceding(5),
            Preceding(2),
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN 2 FOLLOWING AND 5 FOLLOWING) FROM t",
            FrameUnits::Rows,
            Following(2),
            Following(5),
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN 2 PRECEDING AND 5 PRECEDING) FROM t",
            FrameUnits::Rows,
            Preceding(2),
            Preceding(5),
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN 5 FOLLOWING AND 2 FOLLOWING) FROM t",
            FrameUnits::Rows,
            Following(5),
            Following(2),
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN 0 PRECEDING AND CURRENT ROW) FROM t",
            FrameUnits::Rows,
            Preceding(0),
            CurrentRow,
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN CURRENT ROW AND 0 FOLLOWING) FROM t",
            FrameUnits::Rows,
            CurrentRow,
            Following(0),
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN 0 PRECEDING AND 0 FOLLOWING) FROM t",
            FrameUnits::Rows,
            Preceding(0),
            Following(0),
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN 0 PRECEDING AND 5 PRECEDING) FROM t",
            FrameUnits::Rows,
            Preceding(0),
            Preceding(5),
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN 5 FOLLOWING AND 0 FOLLOWING) FROM t",
            FrameUnits::Rows,
            Following(5),
            Following(0),
        ),
        (
            "SELECT SUM(a) OVER (RANGE BETWEEN CURRENT ROW AND UNBOUNDED FOLLOWING) FROM t",
            FrameUnits::Range,
            CurrentRow,
            UnboundedFollowing,
        ),
        (
            "SELECT SUM(a) OVER (ROWS BETWEEN 18446744073709551615 PRECEDING AND CURRENT ROW) FROM t",
            FrameUnits::Rows,
            Preceding(u64::MAX),
            CurrentRow,
        ),
    ] {
        let s = select(sql);
        let bound::ExprKind::Window(w) = &s.projection[0].expr.kind else {
            panic!("expected window");
        };
        assert_eq!(w.frame, bound::WindowFrame { units, start, end }, "{sql}");
    }
    let s = select(
        "SELECT SUM(b) FILTER (WHERE flag) OVER (PARTITION BY label ORDER BY a DESC NULLS FIRST) AS total FROM t ORDER BY total",
    );
    let bound::ExprKind::Window(w) = &s.projection[0].expr.kind else {
        panic!("expected window");
    };
    assert!(w.filter.is_some());
    assert_eq!(w.order_by[0].direction, bound::SortDirection::Descending);
    assert_eq!(w.order_by[0].nulls, bound::NullOrder::First);
    assert_eq!(s.order_by[0].expr, s.projection[0].expr);
}

#[test]
fn invalid_frames_never_bind() {
    for frame in [
        "ROWS BETWEEN UNBOUNDED FOLLOWING AND UNBOUNDED FOLLOWING",
        "ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED PRECEDING",
        "ROWS BETWEEN CURRENT ROW AND 1 PRECEDING",
        "ROWS BETWEEN 1 FOLLOWING AND CURRENT ROW",
        "ROWS BETWEEN CURRENT ROW AND 0 PRECEDING",
        "ROWS BETWEEN 0 FOLLOWING AND CURRENT ROW",
        "ROWS BETWEEN 0 FOLLOWING AND 0 PRECEDING",
        "ROWS 1 FOLLOWING",
        "ROWS -1 PRECEDING",
        "ROWS 1.5 PRECEDING",
        "ROWS 18446744073709551616 PRECEDING",
        "ROWS a PRECEDING",
        "RANGE 1 PRECEDING",
        "RANGE BETWEEN CURRENT ROW AND 1 FOLLOWING",
    ] {
        let sql = format!("SELECT SUM(a) OVER (ORDER BY b {frame}) FROM t");
        assert!(bind(&sql).is_err(), "unexpectedly accepted: {sql}");
    }
}
