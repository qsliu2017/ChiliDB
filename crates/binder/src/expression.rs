//! Expression resolution and explicit operand coercion.

use crate::{BindContext, BindError, LogicalType as T, Scalar, bound};
use chilidb_parser::{BinaryOp, Expr, Literal, UnaryOp};

pub(crate) fn bind_expr<'sql, E>(
    expr: &Expr<'sql>,
    context: &BindContext<'sql>,
) -> Result<bound::Expr<'sql>, BindError<E>> {
    bind_expr_inner(expr, context, 0, false, false)
}

pub(crate) fn bind_expr_inner<'sql, E>(
    expr: &Expr<'sql>,
    context: &BindContext<'sql>,
    depth: usize,
    aggregates: bool,
    windows: bool,
) -> Result<bound::Expr<'sql>, BindError<E>> {
    if depth >= 256 {
        return Err(BindError::ExpressionTooDeep);
    }
    match expr {
        Expr::Alias { .. } => Err(BindError::InvalidStatement(
            "alias only allowed on projection",
        )),
        Expr::FunctionCall {
            name,
            args,
            distinct,
            filter,
            over,
        } => crate::aggregate::bind_call(
            name,
            args,
            *distinct,
            filter.as_deref(),
            over.as_deref(),
            context,
            depth,
            aggregates,
            windows,
        ),

        Expr::Identifier(name) => context.resolve_column::<E>(None, name),
        Expr::QualifiedIdentifier { table, column } => {
            context.resolve_column::<E>(Some(table), column)
        }
        Expr::Wildcard => Err(BindError::InvalidWildcard),
        Expr::Literal(literal) => Ok(match literal {
            Literal::Null => value(T::Null, Scalar::Null),
            Literal::Boolean(b) => value(T::Boolean, Scalar::Boolean(*b)),
            Literal::String(s) => value(T::Text, Scalar::String(s.clone())),
            Literal::Number(s) => number(s, false)?,
        }),
        Expr::Unary { op, expr } => {
            // A signed token is inferred as a whole: this admits both signed minima
            // without first constructing an overflowing positive integer literal.
            if *op == UnaryOp::Minus
                && let Expr::Literal(Literal::Number(s)) = expr.as_ref()
            {
                return number(s, true);
            }
            let expr = bind_expr_inner(expr, context, depth + 1, aggregates, windows)?;
            let expr = match op {
                UnaryOp::Not => boolean(expr, "NOT")?,
                UnaryOp::Plus | UnaryOp::Minus => {
                    let expr = if expr.data_type == T::Null {
                        cast(expr, T::Int32)
                    } else {
                        expr
                    };
                    require_numeric(&expr.data_type, false, "unary sign")?;
                    if *op == UnaryOp::Minus && expr.data_type == T::Uint32 {
                        cast(expr, T::Int64)
                    } else {
                        expr
                    }
                }
            };
            Ok(bound::Expr {
                data_type: expr.data_type.clone(),
                nullable: expr.nullable,
                kind: bound::ExprKind::Unary {
                    op: *op,
                    expr: Box::new(expr),
                },
            })
        }
        Expr::Binary { left, op, right } => {
            let left = bind_expr_inner(left, context, depth + 1, aggregates, windows)?;
            let right = bind_expr_inner(right, context, depth + 1, aggregates, windows)?;
            let (left, right, data_type) =
                match op {
                    BinaryOp::And | BinaryOp::Or => (
                        boolean(left, "boolean operator")?,
                        boolean(right, "boolean operator")?,
                        T::Boolean,
                    ),
                    BinaryOp::Add
                    | BinaryOp::Subtract
                    | BinaryOp::Multiply
                    | BinaryOp::Divide
                    | BinaryOp::Modulo => {
                        let integral = *op == BinaryOp::Modulo;
                        require_numeric(&left.data_type, integral, "arithmetic")?;
                        require_numeric(&right.data_type, integral, "arithmetic")?;
                        let target = numeric_common(&left.data_type, &right.data_type).unwrap();
                        (
                            cast(left, target.clone()),
                            cast(right, target.clone()),
                            target,
                        )
                    }
                    BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Less
                    | BinaryOp::LessEq
                    | BinaryOp::Greater
                    | BinaryOp::GreaterEq => {
                        let target = comparison_common(&left.data_type, &right.data_type)
                            .ok_or_else(|| BindError::IncompatibleTypes {
                                context: "comparison",
                                left: left.data_type.clone(),
                                right: right.data_type.clone(),
                            })?;
                        (cast(left, target.clone()), cast(right, target), T::Boolean)
                    }
                };
            Ok(bound::Expr {
                data_type,
                nullable: left.nullable || right.nullable,
                kind: bound::ExprKind::Binary {
                    op: *op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            })
        }
        Expr::IsNull { expr, negated } => Ok(bound::Expr {
            data_type: T::Boolean,
            nullable: false,
            kind: bound::ExprKind::IsNull {
                expr: Box::new(bind_expr_inner(
                    expr,
                    context,
                    depth + 1,
                    aggregates,
                    windows,
                )?),
                negated: *negated,
            },
        }),
    }
}

pub(crate) fn boolean<'sql, E>(
    expr: bound::Expr<'sql>,
    context: &'static str,
) -> Result<bound::Expr<'sql>, BindError<E>> {
    match expr.data_type {
        T::Boolean => Ok(expr),
        T::Null => Ok(cast(expr, T::Boolean)),
        actual => Err(BindError::TypeMismatch {
            context,
            expected: "Boolean",
            actual,
        }),
    }
}

/// Coerce assignment values without evaluating runtime range or constraint checks.
pub(crate) fn assignment<'sql, E>(
    expr: bound::Expr<'sql>,
    target: &T,
) -> Result<bound::Expr<'sql>, BindError<E>> {
    let source = &expr.data_type;
    if source == target
        || *source == T::Null
        || (numeric(source) && numeric(target))
        || (matches!(source, T::Text | T::Varchar(_)) && matches!(target, T::Text | T::Varchar(_)))
    {
        Ok(cast(expr, target.clone()))
    } else {
        Err(BindError::IncompatibleTypes {
            context: "assignment",
            left: source.clone(),
            right: target.clone(),
        })
    }
}

fn value(data_type: T, scalar: Scalar<'_>) -> bound::Expr<'_> {
    bound::Expr {
        data_type,
        nullable: matches!(scalar, Scalar::Null),
        kind: bound::ExprKind::Literal(scalar),
    }
}

pub(crate) fn cast(expr: bound::Expr<'_>, target: T) -> bound::Expr<'_> {
    if expr.data_type == target {
        return expr;
    }
    bound::Expr {
        data_type: target,
        nullable: expr.nullable,
        kind: bound::ExprKind::Cast {
            expr: Box::new(expr),
        },
    }
}

fn numeric(t: &T) -> bool {
    matches!(t, T::Int32 | T::Int64 | T::Uint32 | T::Float32 | T::Float64)
}

pub(crate) fn require_numeric<E>(
    t: &T,
    integral: bool,
    context: &'static str,
) -> Result<(), BindError<E>> {
    if *t == T::Null || (numeric(t) && (!integral || matches!(t, T::Int32 | T::Int64 | T::Uint32)))
    {
        Ok(())
    } else {
        Err(BindError::TypeMismatch {
            context,
            expected: if integral { "integer" } else { "numeric" },
            actual: t.clone(),
        })
    }
}

fn numeric_common(left: &T, right: &T) -> Option<T> {
    if *left == T::Null && *right == T::Null {
        return Some(T::Int32);
    }
    if *left == T::Null {
        return numeric(right).then(|| right.clone());
    }
    if *right == T::Null {
        return numeric(left).then(|| left.clone());
    }
    if !numeric(left) || !numeric(right) {
        return None;
    }
    if left == right {
        return Some(left.clone());
    }
    if matches!(left, T::Float64 | T::Float32) || matches!(right, T::Float64 | T::Float32) {
        return Some(T::Float64);
    }
    Some(T::Int64)
}

fn comparison_common(left: &T, right: &T) -> Option<T> {
    if *left == T::Null && *right == T::Null {
        return Some(T::Text);
    }
    if *left == T::Null {
        return comparison_common(right, right);
    }
    if *right == T::Null {
        return comparison_common(left, left);
    }
    if *left == T::Boolean && *right == T::Boolean {
        return Some(T::Boolean);
    }
    if matches!(left, T::Text | T::Varchar(_)) && matches!(right, T::Text | T::Varchar(_)) {
        return Some(T::Text);
    }
    numeric_common(left, right)
}

// Match the parser's unsigned decimal-token grammar even for hand-built ASTs.
fn valid_number(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let whole = i;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    } else if whole == 0 {
        return false;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == bytes.len()
}

fn number<'sql, E>(s: &str, negative: bool) -> Result<bound::Expr<'sql>, BindError<E>> {
    let invalid = || {
        BindError::InvalidNumber(if negative {
            format!("-{s}")
        } else {
            s.to_owned()
        })
    };
    if !valid_number(s) {
        return Err(invalid());
    }
    if s.contains(['.', 'e', 'E']) {
        let n: f64 = s.parse().map_err(|_| invalid())?;
        if !n.is_finite() {
            return Err(invalid());
        }
        return Ok(value(
            T::Float64,
            Scalar::Float64(if negative { -n } else { n }),
        ));
    }
    let magnitude: u64 = s.parse().map_err(|_| invalid())?;
    let n = if negative {
        if magnitude == (i64::MAX as u64) + 1 {
            i64::MIN
        } else {
            -i64::try_from(magnitude).map_err(|_| invalid())?
        }
    } else {
        i64::try_from(magnitude).map_err(|_| invalid())?
    };
    Ok(if let Ok(n) = i32::try_from(n) {
        value(T::Int32, Scalar::Int32(n))
    } else {
        value(T::Int64, Scalar::Int64(n))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    fn bind(expr: &Expr<'static>) -> Result<bound::Expr<'static>, BindError<Infallible>> {
        bind_expr(expr, &BindContext { scopes: vec![] })
    }
    fn binary(left: Expr<'static>, op: BinaryOp, right: Expr<'static>) -> Expr<'static> {
        Expr::Binary {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }
    fn num(s: &'static str) -> Expr<'static> {
        Expr::Literal(Literal::Number(s))
    }

    #[test]
    fn deeply_nested_expressions_return_an_error() {
        let mut expr = num("1");
        for _ in 0..300 {
            expr = Expr::Unary {
                op: UnaryOp::Plus,
                expr: Box::new(expr),
            };
        }
        assert!(matches!(bind(&expr), Err(BindError::ExpressionTooDeep)));
    }

    #[test]
    fn assignment_coercions_are_explicit_and_preserve_nullability() {
        let narrowed =
            assignment::<()>(value(T::Int64, Scalar::Int64(i64::MAX)), &T::Int32).unwrap();
        assert_eq!(narrowed.data_type, T::Int32);
        assert!(matches!(narrowed.kind, bound::ExprKind::Cast { .. }));
        let null = assignment::<()>(value(T::Null, Scalar::Null), &T::Boolean).unwrap();
        assert_eq!(null.data_type, T::Boolean);
        assert!(null.nullable);
        let varchar = T::Varchar(std::num::NonZeroU32::new(3));
        let string = assignment::<()>(
            value(T::Text, Scalar::String("long string".into())),
            &varchar,
        )
        .unwrap();
        assert_eq!(string.data_type, varchar);
        assert!(matches!(string.kind, bound::ExprKind::Cast { .. }));
        assert!(assignment::<()>(value(T::Text, Scalar::String("12".into())), &T::Int32).is_err());
        assert!(assignment::<()>(value(T::Int32, Scalar::Int32(1)), &T::Boolean).is_err());
    }

    #[test]
    fn numeric_matrix() {
        let types = [T::Int32, T::Uint32, T::Int64, T::Float32, T::Float64];
        let expected = [
            [T::Int32, T::Int64, T::Int64, T::Float64, T::Float64],
            [T::Int64, T::Uint32, T::Int64, T::Float64, T::Float64],
            [T::Int64, T::Int64, T::Int64, T::Float64, T::Float64],
            [T::Float64, T::Float64, T::Float64, T::Float32, T::Float64],
            [T::Float64, T::Float64, T::Float64, T::Float64, T::Float64],
        ];
        for (i, left) in types.iter().enumerate() {
            for (j, right) in types.iter().enumerate() {
                assert_eq!(numeric_common(left, right), Some(expected[i][j].clone()));
            }
        }
    }

    #[test]
    fn numeric_boundaries_and_malformed_tokens() {
        for (token, negative, expected) in [
            ("2147483647", false, Scalar::Int32(i32::MAX)),
            ("2147483648", false, Scalar::Int64(2147483648)),
            ("2147483648", true, Scalar::Int32(i32::MIN)),
            ("9223372036854775808", true, Scalar::Int64(i64::MIN)),
            ("1.25e2", false, Scalar::Float64(125.0)),
        ] {
            let expr = if negative {
                Expr::Unary {
                    op: UnaryOp::Minus,
                    expr: Box::new(num(token)),
                }
            } else {
                num(token)
            };
            assert_eq!(
                bind(&expr).unwrap().kind,
                bound::ExprKind::Literal(expected)
            );
        }
        for token in [
            "",
            "NaN",
            "inf",
            "1.",
            "1e",
            "+1",
            "-1",
            " 1",
            "1e999",
            "9223372036854775808",
            "18446744073709551616",
        ] {
            assert!(
                matches!(bind(&num(token)), Err(BindError::InvalidNumber(_))),
                "{token}"
            );
        }
        assert!(number::<Infallible>("9223372036854775809", true).is_err());
    }

    #[test]
    fn null_context_casts_and_nullability() {
        let null = Expr::Literal(Literal::Null);
        assert_eq!(bind(&null).unwrap().data_type, T::Null);
        for (op, target, operand_type) in [
            (BinaryOp::Add, T::Int32, T::Int32),
            (BinaryOp::Eq, T::Boolean, T::Text),
            (BinaryOp::And, T::Boolean, T::Boolean),
        ] {
            let bound = bind(&binary(null.clone(), op, null.clone())).unwrap();
            assert_eq!(bound.data_type, target);
            assert!(bound.nullable);
            let bound::ExprKind::Binary { left, right, .. } = bound.kind else {
                panic!()
            };
            for operand in [left, right] {
                assert_eq!(operand.data_type, operand_type);
                assert!(operand.nullable);
                assert!(matches!(operand.kind, bound::ExprKind::Cast { .. }));
            }
        }
        assert!(
            !bind(&Expr::IsNull {
                expr: Box::new(null),
                negated: true
            })
            .unwrap()
            .nullable
        );
        let result = bind(&binary(num("1"), BinaryOp::Add, num("1.5"))).unwrap();
        assert!(!result.nullable);
        let bound::ExprKind::Binary { left, right, .. } = result.kind else {
            panic!()
        };
        assert!(matches!(left.kind, bound::ExprKind::Cast { .. }));
        assert!(matches!(right.kind, bound::ExprKind::Literal(_)));
    }

    #[test]
    fn rejects_invalid_operands_and_string_numeric_coercion() {
        let string = Expr::Literal(Literal::String("1".into()));
        for expr in [
            binary(string.clone(), BinaryOp::Add, num("1")),
            binary(num("1.5"), BinaryOp::Modulo, num("1")),
            binary(num("1"), BinaryOp::And, num("2")),
            Expr::Unary {
                op: UnaryOp::Plus,
                expr: Box::new(string.clone()),
            },
        ] {
            assert!(matches!(bind(&expr), Err(BindError::TypeMismatch { .. })));
        }
        assert!(matches!(
            bind(&binary(string, BinaryOp::Eq, num("1"))),
            Err(BindError::IncompatibleTypes { .. })
        ));
        assert!(matches!(
            bind(&Expr::Wildcard),
            Err(BindError::InvalidWildcard)
        ));
        assert_eq!(
            comparison_common(&T::Varchar(None), &T::Text),
            Some(T::Text)
        );
    }
}
