//! Built-in signatures, window frames, and structural grouping validation.
use crate::{BindContext, BindError, LogicalType as T, bound as b, expression as e};
use chilidb_parser as p;

#[allow(clippy::too_many_arguments)]
pub(crate) fn bind_call<'sql, E>(
    name: &str,
    args: &[p::Expr<'sql>],
    distinct: bool,
    filter: Option<&p::Expr<'sql>>,
    over: Option<&p::WindowSpec<'sql>>,
    context: &BindContext<'sql>,
    depth: usize,
    aggregates: bool,
    windows: bool,
) -> Result<b::Expr<'sql>, BindError<E>> {
    use b::{AggregateFunction as A, WindowFunction as W};
    let function = match name {
        "count" => W::Aggregate(A::Count),
        "sum" => W::Aggregate(A::Sum),
        "avg" => W::Aggregate(A::Avg),
        "min" => W::Aggregate(A::Min),
        "max" => W::Aggregate(A::Max),
        "row_number" => W::RowNumber,
        "rank" => W::Rank,
        "dense_rank" => W::DenseRank,
        _ => return Err(BindError::UnknownFunction(name.to_owned())),
    };
    if over.is_some() {
        if !windows {
            return Err(BindError::InvalidFunction(
                "window is not allowed in this context",
            ));
        }
        if distinct {
            return Err(BindError::InvalidFunction("window DISTINCT is unsupported"));
        }
    } else if !matches!(function, W::Aggregate(_)) {
        return Err(BindError::InvalidFunction("ranking functions require OVER"));
    } else if !aggregates {
        return Err(BindError::InvalidFunction(
            "aggregate is not allowed in this context",
        ));
    }
    let star = matches!(args, [p::Expr::Wildcard]);
    match function {
        W::Aggregate(a) => {
            if args.len() != 1 || (star && a != A::Count) || (star && distinct) {
                return Err(BindError::InvalidFunction(
                    "aggregate requires one argument; only COUNT accepts non-DISTINCT *",
                ));
            }
        }
        _ if !args.is_empty() || distinct || filter.is_some() => {
            return Err(BindError::InvalidFunction(
                "ranking functions take no arguments, DISTINCT, or FILTER",
            ));
        }
        _ => {}
    }
    // A window consumes group output, so its operands may contain plain aggregates.
    let bind = |x: &p::Expr<'sql>| e::bind_expr_inner(x, context, depth + 1, over.is_some(), false);
    let mut args = if star {
        vec![]
    } else {
        args.iter().map(bind).collect::<Result<Vec<_>, _>>()?
    };
    let filter = filter
        .map(|x| {
            // Window FILTER consumes group output, just like window arguments.
            e::boolean(bind(x)?, "FILTER").map(Box::new)
        })
        .transpose()?;
    let (data_type, nullable) = match function {
        W::Aggregate(A::Count) | W::RowNumber | W::Rank | W::DenseRank => (T::Int64, false),
        W::Aggregate(a) => {
            let arg = args.remove(0);
            let target = match a {
                A::Sum | A::Avg => {
                    e::require_numeric(&arg.data_type, false, "aggregate")?;
                    if a == A::Avg || matches!(arg.data_type, T::Float32 | T::Float64) {
                        T::Float64
                    } else {
                        T::Int64
                    }
                }
                A::Min | A::Max => {
                    if arg.data_type == T::Null {
                        T::Text
                    } else {
                        arg.data_type.clone()
                    }
                }
                A::Count => unreachable!(),
            };
            args.push(e::cast(arg, target.clone()));
            (target, true)
        }
    };
    let kind = if let Some(over) = over {
        let partition_by = over
            .partition_by
            .iter()
            .map(bind)
            .collect::<Result<_, _>>()?;
        let order_by = over
            .order_by
            .iter()
            .map(|o| Ok(order(bind(&o.expr)?, o)))
            .collect::<Result<_, BindError<E>>>()?;
        b::ExprKind::Window(Box::new(b::WindowExpr {
            function,
            args,
            distinct,
            filter,
            partition_by,
            order_by,
            frame: frame(over.frame.as_ref(), !over.order_by.is_empty())?,
        }))
    } else {
        let W::Aggregate(function) = function else {
            unreachable!()
        };
        b::ExprKind::Aggregate(Box::new(b::AggregateExpr {
            function,
            args,
            distinct,
            filter,
        }))
    };
    Ok(b::Expr {
        data_type,
        nullable,
        kind,
    })
}

pub(crate) fn order<'sql>(
    expr: b::Expr<'sql>,
    order: &p::OrderByExpr<'sql>,
) -> b::OrderByExpr<'sql> {
    b::OrderByExpr {
        expr,
        direction: order.direction,
        nulls: order.nulls.unwrap_or(match order.direction {
            p::SortDirection::Ascending => p::NullOrder::Last,
            p::SortDirection::Descending => p::NullOrder::First,
        }),
    }
}

fn frame<E>(
    frame: Option<&p::WindowFrame<'_>>,
    ordered: bool,
) -> Result<b::WindowFrame, BindError<E>> {
    use b::FrameBound as B;
    let Some(frame) = frame else {
        return Ok(b::WindowFrame {
            units: p::FrameUnits::Range,
            start: B::UnboundedPreceding,
            end: if ordered {
                B::CurrentRow
            } else {
                B::UnboundedFollowing
            },
        });
    };
    let convert = |bound: &p::FrameBound<'_>| -> Result<B, BindError<E>> {
        Ok(match bound {
            p::FrameBound::UnboundedPreceding => B::UnboundedPreceding,
            p::FrameBound::UnboundedFollowing => B::UnboundedFollowing,
            p::FrameBound::CurrentRow => B::CurrentRow,
            p::FrameBound::Preceding(n) | p::FrameBound::Following(n) => {
                if frame.units == p::FrameUnits::Range {
                    return Err(BindError::InvalidWindowFrame(
                        "RANGE offsets are unsupported",
                    ));
                }
                if n.is_empty() || !n.bytes().all(|x| x.is_ascii_digit()) {
                    return Err(BindError::InvalidWindowFrame(
                        "ROWS offset must be a nonnegative integer",
                    ));
                }
                let n = n
                    .parse()
                    .map_err(|_| BindError::InvalidWindowFrame("ROWS offset exceeds u64"))?;
                if matches!(bound, p::FrameBound::Preceding(_)) {
                    B::Preceding(n)
                } else {
                    B::Following(n)
                }
            }
        })
    };
    let start = convert(&frame.start)?;
    let end = convert(&frame.end)?;
    // SQL orders boundary categories, not offsets within a category.
    // Reversed offsets in one category describe a potentially empty frame.
    // Zero offsets retain their syntactic category during this validation.
    let category = |b| match b {
        B::UnboundedPreceding => 0,
        B::Preceding(_) => 1,
        B::CurrentRow => 2,
        B::Following(_) => 3,
        B::UnboundedFollowing => 4,
    };
    if start == B::UnboundedFollowing
        || end == B::UnboundedPreceding
        || category(start) > category(end)
    {
        return Err(BindError::InvalidWindowFrame(
            "frame boundaries are reversed or use an invalid unbounded endpoint",
        ));
    }
    Ok(b::WindowFrame {
        units: frame.units,
        start,
        end,
    })
}

pub(crate) fn contains_aggregate(expr: &b::Expr<'_>) -> bool {
    if matches!(expr.kind, b::ExprKind::Aggregate(_)) {
        return true;
    }
    children(expr).into_iter().any(contains_aggregate)
}

pub(crate) fn validate_grouped<E>(
    expr: &b::Expr<'_>,
    groups: &[b::Expr<'_>],
) -> Result<(), BindError<E>> {
    if groups.contains(expr) || matches!(expr.kind, b::ExprKind::Aggregate(_)) {
        return Ok(());
    }
    if matches!(expr.kind, b::ExprKind::Column(_)) {
        return Err(BindError::UngroupedColumn);
    }
    for child in children(expr) {
        validate_grouped(child, groups)?;
    }
    Ok(())
}

fn children<'a, 'sql>(expr: &'a b::Expr<'sql>) -> Vec<&'a b::Expr<'sql>> {
    use b::ExprKind as K;
    match &expr.kind {
        K::Unary { expr, .. } | K::Cast { expr } | K::IsNull { expr, .. } => vec![expr],
        K::Binary { left, right, .. } => vec![left, right],
        K::Window(w) => w
            .args
            .iter()
            .chain(w.filter.as_deref())
            .chain(w.partition_by.iter())
            .chain(w.order_by.iter().map(|o| &o.expr))
            .collect(),
        K::Literal(_) | K::Column(_) | K::Aggregate(_) => vec![],
    }
}
