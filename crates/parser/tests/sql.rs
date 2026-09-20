use chilidb_parser::*;
fn num(n: &str) -> Expr<'_> {
    Expr::Literal(Literal::Number(n))
}
fn bin<'sql>(l: Expr<'sql>, op: BinaryOp, r: Expr<'sql>) -> Expr<'sql> {
    Expr::Binary {
        left: Box::new(l),
        op,
        right: Box::new(r),
    }
}
fn projection(sql: &str) -> Vec<Expr<'_>> {
    match parse_sql(sql).unwrap().remove(0) {
        Statement::Select { projection, .. } => projection,
        _ => panic!("expected SELECT"),
    }
}
#[test]
fn exact_select_and_precedence() {
    assert_eq!(
        parse_sql("SELECT A, 1 + 2 * 3 FROM Things WHERE NOT a = 4 OR true AND false").unwrap(),
        vec![Statement::Select {
            projection: vec![
                Expr::Identifier("a".into()),
                bin(
                    num("1"),
                    BinaryOp::Add,
                    bin(num("2"), BinaryOp::Multiply, num("3"))
                )
            ],
            group_by: vec![],
            having: None,
            order_by: vec![],
            from: Some("things".into()),
            filter: Some(bin(
                Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(bin(Expr::Identifier("a".into()), BinaryOp::Eq, num("4")))
                },
                BinaryOp::Or,
                bin(
                    Expr::Literal(Literal::Boolean(true)),
                    BinaryOp::And,
                    Expr::Literal(Literal::Boolean(false))
                )
            )),
        }]
    );
    assert_eq!(
        projection("select 8-3-1"),
        vec![bin(
            bin(num("8"), BinaryOp::Subtract, num("3")),
            BinaryOp::Subtract,
            num("1")
        )]
    );
    assert_eq!(
        projection("select -(1+2)"),
        vec![Expr::Unary {
            op: UnaryOp::Minus,
            expr: Box::new(bin(num("1"), BinaryOp::Add, num("2")))
        }]
    );
}
#[test]
fn literals_names_and_trivia() {
    assert_eq!(
        projection(
            "-- café\n SeLeCt /* 注 */ 'it''s 世界', \"a\"\"B\", .50e+2, NULL, selection, t.col --end"
        ),
        vec![
            Expr::Literal(Literal::String("it's 世界".into())),
            Expr::Identifier("a\"B".into()),
            num(".50e+2"),
            Expr::Literal(Literal::Null),
            Expr::Identifier("selection".into()),
            Expr::QualifiedIdentifier {
                table: "t".into(),
                column: "col".into()
            },
        ]
    );
    assert_eq!(
        projection("select a is not null"),
        vec![Expr::IsNull {
            expr: Box::new(Expr::Identifier("a".into())),
            negated: true
        }]
    );
    assert_eq!(
        projection("select 1+/* comment */2"),
        vec![bin(num("1"), BinaryOp::Add, num("2"))]
    );
    assert_eq!(
        projection("select \"世界\""),
        vec![Expr::Identifier("世界".into())]
    );
}
#[test]
fn ddl_and_dml() {
    assert_eq!(
        parse_sql("create table T (ID integer primary key, name varchar(30) not null unique)")
            .unwrap(),
        vec![Statement::CreateTable {
            name: "t".into(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: DataType::Integer,
                    constraints: vec![ColumnConstraint::PrimaryKey]
                },
                ColumnDef {
                    name: "name".into(),
                    data_type: DataType::Varchar(Some("30")),
                    constraints: vec![ColumnConstraint::NotNull, ColumnConstraint::Unique]
                },
            ],
        }]
    );
    assert_eq!(
        parse_sql("insert into T (a,b) values (1,'x'),(2,NULL)").unwrap(),
        vec![Statement::Insert {
            table: "t".into(),
            columns: vec!["a".into(), "b".into()],
            rows: vec![
                vec![num("1"), Expr::Literal(Literal::String("x".into()))],
                vec![num("2"), Expr::Literal(Literal::Null)]
            ],
        }]
    );
    assert_eq!(
        parse_sql("update t set a=1,b=2 where a<>0; delete from t").unwrap(),
        vec![
            Statement::Update {
                table: "t".into(),
                assignments: vec![
                    Assignment {
                        column: "a".into(),
                        value: num("1")
                    },
                    Assignment {
                        column: "b".into(),
                        value: num("2")
                    }
                ],
                filter: Some(bin(Expr::Identifier("a".into()), BinaryOp::NotEq, num("0")))
            },
            Statement::Delete {
                table: "t".into(),
                filter: None
            },
        ]
    );
    for sql in [
        "select * from t",
        "insert into t values (1)",
        "create table t(a bigint,b text,c boolean,d real,e double,f varchar)",
    ] {
        assert!(parse_sql(sql).is_ok(), "{sql}");
    }
}
#[test]
fn transactions_and_statement_boundaries() {
    assert_eq!(
        parse_sql("begin; commit; rollback;").unwrap(),
        vec![Statement::Begin, Statement::Commit, Statement::Rollback]
    );
    assert_eq!(parse_sql(" \n /* empty */ -- x").unwrap(), vec![]);
    assert_eq!(parse_sql("select ';';begin").unwrap().len(), 2);
    for sql in [
        "begin commit",
        "begin;;commit",
        ";",
        "beginning",
        "select 1 select 2",
    ] {
        assert!(parse_sql(sql).is_err(), "{sql}");
    }
}
#[test]
fn errors_reject_unsupported_and_malformed_input() {
    for sql in [
        "select",
        "select from",
        "select 1xyz",
        "select 'oops",
        "select \"\"",
        "select 1 /* unterminated",
        "select 1 from t join u",
        "select (select 1)",
        "select 1 order by",
        "select a < b < c",
        "select 1,",
        "insert into t values ()",
        "update t set",
        "create table t(a mystery)",
        "select 世界",
        "select 1; garbage",
    ] {
        let error = parse_sql(sql).expect_err(sql);
        assert!(error.offset <= sql.len(), "{sql}: {error}");
        assert!(!error.expected.is_empty(), "{sql}: {error}");
        assert!(!error.to_string().is_empty());
    }
}
#[test]
fn tree_has_byte_spans_and_silent_trivia() {
    fn visit(n: &grammar::Node, sql: &str) {
        assert!(!n.rule().starts_with('_'));
        assert!(sql.get(n.span()).is_some());
        for child in n.children() {
            assert!(child.span().start >= n.span().start && child.span().end <= n.span().end);
            visit(child, sql);
        }
    }
    let sql = "/* é */ SELECT '世界'";
    let root = grammar::parse(sql).unwrap();
    assert_eq!(root.span(), 0..sql.len());
    visit(&root, sql);
}

fn assert_in_input(input: &str, slice: &str) {
    let start = input.as_ptr() as usize;
    let pointer = slice.as_ptr() as usize;
    assert!(pointer >= start && pointer + slice.len() <= start + input.len());
}

#[test]
fn text_borrows_input_unless_normalization_is_needed() {
    use std::borrow::Cow;
    let sql = String::from(
        "select lower_2, MiXeD, \"世界A\", \"世\"\"界\"\"é\", '世界', '世''界''é', '', .50e+2 from things",
    );
    let statements = parse_sql(&sql).unwrap();
    let Statement::Select {
        projection, from, ..
    } = &statements[0]
    else {
        panic!("expected SELECT")
    };
    for (index, expected) in [(0, "lower_2"), (2, "世界A")] {
        let Expr::Identifier(Cow::Borrowed(value)) = &projection[index] else {
            panic!("identifier should borrow")
        };
        assert_eq!(*value, expected);
        assert_in_input(&sql, value);
    }
    for (index, expected) in [(1, "mixed"), (3, "世\"界\"é")] {
        let Expr::Identifier(Cow::Owned(value)) = &projection[index] else {
            panic!("normalized identifier should own")
        };
        assert_eq!(value, expected);
    }
    for (index, expected) in [(4, "世界"), (6, "")] {
        let Expr::Literal(Literal::String(Cow::Borrowed(value))) = &projection[index] else {
            panic!("unescaped string should borrow")
        };
        assert_eq!(*value, expected);
        assert_in_input(&sql, value);
    }
    assert!(
        matches!(&projection[5], Expr::Literal(Literal::String(Cow::Owned(value))) if value == "世'界'é")
    );
    let Expr::Literal(Literal::Number(number)) = &projection[7] else {
        panic!("expected number")
    };
    assert_eq!(*number, ".50e+2");
    assert_in_input(&sql, number);
    let Some(Cow::Borrowed(table)) = from else {
        panic!("table should borrow")
    };
    assert_in_input(&sql, table);
}

#[test]
fn ddl_names_and_length_borrow_input() {
    use std::borrow::Cow;
    let sql = String::from("create table things (label varchar(0030))");
    let statements = parse_sql(&sql).unwrap();
    let Statement::CreateTable {
        name: Cow::Borrowed(name),
        columns,
    } = &statements[0]
    else {
        panic!("table name should borrow")
    };
    assert_in_input(&sql, name);
    let Cow::Borrowed(column) = &columns[0].name else {
        panic!("column should borrow")
    };
    assert_in_input(&sql, column);
    let DataType::Varchar(Some(length)) = columns[0].data_type else {
        panic!("expected VARCHAR length")
    };
    assert_eq!(length, "0030");
    assert_in_input(&sql, length);
}

#[test]
fn aggregates_grouping_and_explicit_aliases() {
    let sql = "SELECT COUNT(*) AS n, mystery(DISTINCT x, y) FILTER (WHERE x > 0) FROM t WHERE y IS NOT NULL GROUP BY x, y HAVING count(*) > 1 ORDER BY n DESC NULLS LAST, x";
    let Statement::Select {
        projection,
        group_by,
        having,
        order_by,
        ..
    } = parse_sql(sql).unwrap().remove(0)
    else {
        panic!()
    };
    let Expr::Alias { expr, alias } = &projection[0] else {
        panic!()
    };
    assert_eq!(alias, "n");
    assert!(
        matches!(expr.as_ref(), Expr::FunctionCall { name, args, distinct: false, filter: None, over: None } if name == "count" && args == &[Expr::Wildcard])
    );
    assert!(
        matches!(&projection[1], Expr::FunctionCall { name, args, distinct: true, filter: Some(_), over: None } if name == "mystery" && args.len() == 2)
    );
    assert_eq!(group_by.len(), 2);
    assert!(having.is_some());
    assert_eq!(order_by[0].direction, SortDirection::Descending);
    assert_eq!(order_by[0].nulls, Some(NullOrder::Last));
    assert_eq!(order_by[1].direction, SortDirection::Ascending);
    assert_eq!(order_by[1].nulls, None);
    assert!(
        matches!(&projection_for_empty()[0], Expr::FunctionCall { args, .. } if args.is_empty())
    );
}
fn projection_for_empty() -> Vec<Expr<'static>> {
    projection("SELECT unknown()")
}

#[test]
fn inline_windows_and_borrowed_frames() {
    let sql = String::from(
        "SELECT sum(x) FILTER (WHERE true) OVER (PARTITION BY k ORDER BY x ASC NULLS FIRST ROWS BETWEEN 001 PRECEDING AND CURRENT ROW)",
    );
    let p = projection(&sql);
    let Expr::FunctionCall {
        over: Some(w),
        filter: Some(_),
        ..
    } = &p[0]
    else {
        panic!()
    };
    assert_eq!(w.partition_by.len(), 1);
    assert_eq!(w.order_by[0].nulls, Some(NullOrder::First));
    let f = w.frame.as_ref().unwrap();
    assert_eq!(f.units, FrameUnits::Rows);
    assert_eq!(f.end, FrameBound::CurrentRow);
    let FrameBound::Preceding(offset) = f.start else {
        panic!()
    };
    assert_eq!(offset, "001");
    assert_in_input(&sql, offset);
    for (frame, start, end) in [
        (
            "ROWS 2 PRECEDING",
            FrameBound::Preceding("2"),
            FrameBound::CurrentRow,
        ),
        (
            "RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING",
            FrameBound::UnboundedPreceding,
            FrameBound::UnboundedFollowing,
        ),
        (
            "ROWS BETWEEN CURRENT ROW AND 3 FOLLOWING",
            FrameBound::CurrentRow,
            FrameBound::Following("3"),
        ),
    ] {
        let sql = format!("SELECT sum(x) OVER ({frame})");
        let p = projection(&sql);
        let Expr::FunctionCall { over: Some(w), .. } = &p[0] else {
            panic!()
        };
        assert_eq!(w.frame.as_ref().unwrap().start, start);
        assert_eq!(w.frame.as_ref().unwrap().end, end);
    }
    let p = projection(
        "SELECT row_number() OVER (), count, overlap, grouping, first_value(x) OVER (ORDER BY x)",
    );
    assert!(
        matches!(&p[0], Expr::FunctionCall { over: Some(w), .. } if w.frame.is_none() && w.order_by.is_empty())
    );
    assert!(matches!(&p[4], Expr::FunctionCall { over: Some(w), .. } if w.frame.is_none()));
    assert!(parse_sql("SELECT SuM/*世*/(DISTINCT x) FILTER/*é*/(WHERE true) OVER (ORDER BY x NULLS/*中*/FIRST ROWS UNBOUNDED/*é*/PRECEDING) AS \"总数\"").is_ok());
}

#[test]
fn malformed_aggregate_and_window_clauses() {
    for sql in [
        "SELECT *, x",
        "SELECT x y",
        "SELECT * AS x",
        "SELECT f(*, x)",
        "SELECT f(x, *)",
        "SELECT f(,)",
        "SELECT f(x AS y)",
        "SELECT x FROM t WHERE x AS y",
        "SELECT x GROUP BY",
        "SELECT x HAVING",
        "SELECT x ORDER BY x NULLS",
        "SELECT x ORDER BY x GROUP BY x",
        "SELECT f() OVER w",
        "SELECT f() OVER (PARTITION BY)",
        "SELECT f() OVER (ORDER BY x PARTITION BY y)",
        "SELECT f() OVER (ROWS BETWEEN 1 PRECEDING)",
        "SELECT f() OVER (ROWS -1 PRECEDING)",
        "SELECT f() OVER (ROWS 1.5 PRECEDING)",
        "SELECT f() OVER (ROWS 1e2 PRECEDING)",
        "SELECT f() OVER (GROUPS CURRENT ROW)",
        "SELECT f() OVER (ROWS CURRENT ROW EXCLUDE CURRENT ROW)",
        "SELECT f() OVER () FILTER (WHERE true)",
        "SELECT f() FILTER (true)",
    ] {
        assert!(parse_sql(sql).is_err(), "{sql}");
    }
}
