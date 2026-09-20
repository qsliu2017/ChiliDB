use chilidb_binder::{LogicalType, Scalar, bound as b};
use chilidb_planner::{LogicalPlan, PlanError, PlannedStatement, Planner};

fn literal() -> b::Expr<'static> {
    b::Expr {
        data_type: LogicalType::Int32,
        nullable: false,
        kind: b::ExprKind::Literal(Scalar::Int32(1)),
    }
}

fn query(expr: b::Expr<'static>) -> b::Statement<'static> {
    b::Statement::Select(b::Select {
        group_by: vec![],
        having: None,
        order_by: vec![],
        is_aggregate: false,
        source: None,
        projection: vec![b::NamedExpr {
            name: "one".into(),
            expr,
        }],
        filter: None,
    })
}

#[test]
fn plan_does_not_borrow_bound_statement() {
    let plan = Planner::new().plan(&query(literal())).unwrap();
    let PlannedStatement::Query { plan, output } = plan else {
        panic!()
    };
    assert_eq!(output.fields[0].name, "one");
    assert_eq!(output.fields[0].origin, None);
    let LogicalPlan::Projection(p) = plan else {
        panic!()
    };
    assert_eq!(p.expr.len(), 1);
    assert!(matches!(p.input.as_ref(), LogicalPlan::EmptyRelation(e) if e.produce_one_row));
}

#[test]
fn depth_is_checked_before_structural_deduplication() {
    let mut expr = literal();
    for _ in 0..256 {
        expr = b::Expr {
            data_type: LogicalType::Int32,
            nullable: false,
            kind: b::ExprKind::Cast {
                expr: Box::new(expr),
            },
        };
    }
    assert!(matches!(
        Planner.plan(&query(expr)),
        Err(PlanError::ExpressionTooDeep)
    ));
}

#[test]
fn nested_windows_are_rejected_independently_of_projection_order() {
    let inner = b::Expr {
        data_type: LogicalType::Int64,
        nullable: false,
        kind: b::ExprKind::Window(Box::new(b::WindowExpr {
            function: b::WindowFunction::RowNumber,
            args: vec![],
            distinct: false,
            filter: None,
            partition_by: vec![],
            order_by: vec![],
            frame: b::WindowFrame {
                units: b::FrameUnits::Range,
                start: b::FrameBound::UnboundedPreceding,
                end: b::FrameBound::UnboundedFollowing,
            },
        })),
    };
    let b::ExprKind::Window(template) = &inner.kind else {
        panic!()
    };
    for operand in 0..4 {
        let mut outer = template.as_ref().clone();
        outer.function = b::WindowFunction::Aggregate(b::AggregateFunction::Count);
        outer.args = vec![literal()];
        match operand {
            0 => outer.args = vec![inner.clone()],
            1 => outer.partition_by = vec![inner.clone()],
            2 => {
                outer.order_by = vec![b::OrderByExpr {
                    expr: inner.clone(),
                    direction: b::SortDirection::Ascending,
                    nulls: b::NullOrder::Last,
                }]
            }
            _ => {
                outer.filter = Some(Box::new(b::Expr {
                    data_type: LogicalType::Boolean,
                    nullable: false,
                    kind: b::ExprKind::IsNull {
                        expr: Box::new(inner.clone()),
                        negated: false,
                    },
                }))
            }
        }
        for placement in 0..3 {
            let mut statement = query(b::Expr {
                data_type: LogicalType::Int64,
                nullable: false,
                kind: b::ExprKind::Window(Box::new(outer.clone())),
            });
            let b::Statement::Select(select) = &mut statement else {
                panic!()
            };
            let named_inner = b::NamedExpr {
                name: "inner".into(),
                expr: inner.clone(),
            };
            match placement {
                0 => {}
                1 => select.projection.insert(0, named_inner),
                _ => select.projection.push(named_inner),
            }
            assert!(
                Planner.plan(&statement).is_err(),
                "operand={operand}, placement={placement}"
            );
        }
    }
}

#[test]
fn maximum_allowed_depth_can_be_lowered() {
    let mut expr = literal();
    for _ in 1..256 {
        expr = b::Expr {
            data_type: LogicalType::Int32,
            nullable: false,
            kind: b::ExprKind::Cast {
                expr: Box::new(expr),
            },
        };
    }
    assert!(Planner.plan(&query(expr)).is_ok());
}
