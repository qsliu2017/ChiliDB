//! Bound-tree lowering to owned DataFusion expressions and relational nodes.
use crate::table_source::{arrow_type, column, ctid, scan};
use crate::{
    Command, Field, LogicalType, ModifyTable, OutputSchema, PlanError, PlannedStatement, Scalar,
    Target, UpdateAssignment,
};
use chilidb_binder::bound as b;
use datafusion_common::{Column, DFSchema, ExprSchema, ScalarValue};
use datafusion_expr::expr::{
    AggregateFunction, BinaryExpr, Cast, Sort, WindowFunction, WindowFunctionDefinition,
};
use datafusion_expr::logical_plan::{Aggregate, Filter, Projection, Window};
use datafusion_expr::{
    AggregateUDF, Expr, ExprSchemable, LogicalPlan, LogicalPlanBuilder as Builder, Operator,
    WindowFrame, WindowFrameBound, WindowFrameUnits,
};
use std::sync::Arc;

#[derive(Debug, Default, Clone, Copy)]
pub struct Planner;
impl Planner {
    pub fn new() -> Self {
        Self
    }
    pub fn plan<'sql>(
        &self,
        statement: &b::Statement<'sql>,
    ) -> Result<PlannedStatement<'sql>, PlanError> {
        use b::Statement as S;
        Ok(match statement {
            S::Select(q) => return select(q),
            S::CreateTable(d) => PlannedStatement::Command(Command::CreateTable(d.clone())),
            S::Begin => PlannedStatement::Command(Command::Begin),
            S::Commit => PlannedStatement::Command(Command::Commit),
            S::Rollback => PlannedStatement::Command(Command::Rollback),
            S::Insert(i) => {
                let target_schema = i.table.source.schema();
                let empty = DFSchema::empty();
                let lower = Lower::raw(&empty);
                let mut nullable = vec![false; target_schema.columns.len()];
                let mut rows = vec![];
                for row in &i.rows {
                    if row.len() != nullable.len() {
                        return invalid("INSERT row width differs from target");
                    }
                    let mut values = vec![];
                    for (n, value) in row.iter().enumerate() {
                        check_depth(value)?;
                        if value.data_type != target_schema.columns[n].data_type {
                            return invalid("INSERT value type differs from target");
                        }
                        nullable[n] |= value.nullable;
                        values.push(lower.expr(value, 1)?);
                    }
                    rows.push(values);
                }
                let schema = Arc::new(DFSchema::try_from(arrow_schema::Schema::new(
                    target_schema
                        .columns
                        .iter()
                        .enumerate()
                        .map(|(n, c)| {
                            arrow_schema::Field::new(
                                format!("__insert{n}"),
                                arrow_type(&c.data_type),
                                nullable[n],
                            )
                        })
                        .collect::<Vec<_>>(),
                ))?);
                let input = Builder::values_with_schema(rows, &schema)?.build()?;
                // The builder validates values but replaces supplied names with columnN.
                let LogicalPlan::Values(mut values_plan) = input else {
                    return invalid("expected VALUES input");
                };
                values_plan.schema = schema;
                let input = LogicalPlan::Values(values_plan);
                let values = (0..nullable.len())
                    .map(|n| Column::from_name(format!("__insert{n}")))
                    .collect();
                PlannedStatement::ModifyTable {
                    plan: ModifyTable::try_insert(Target::new(i.table.clone()), input, values)?
                        .into_plan(),
                }
            }
            S::Update(u) => {
                let input = where_filter(scan(&u.table, true)?, u.filter.as_ref())?;
                let lower = Lower::raw(input.schema());
                let mut expressions = vec![Expr::Column(ctid(&u.table)).alias("__ctid")];
                let mut assignments = vec![];
                for (n, a) in u.assignments.iter().enumerate() {
                    check_depth(&a.value)?;
                    let schema = u.table.source.schema();
                    let Some(c) = schema.columns.get(a.column_index) else {
                        return invalid("UPDATE target index is out of bounds");
                    };
                    if c.data_type != a.value.data_type {
                        return invalid("UPDATE value type differs from target");
                    }
                    let name = format!("__new{n}");
                    expressions.push(lower.expr(&a.value, 1)?.alias(&name));
                    assignments.push(UpdateAssignment {
                        target_column_index: a.column_index,
                        value: Column::from_name(name),
                    });
                }
                let input =
                    LogicalPlan::Projection(Projection::try_new(expressions, Arc::new(input))?);
                PlannedStatement::ModifyTable {
                    plan: ModifyTable::try_update(
                        Target::new(u.table.clone()),
                        input,
                        Column::from_name("__ctid"),
                        assignments,
                    )?
                    .into_plan(),
                }
            }
            S::Delete(d) => {
                let input = where_filter(scan(&d.table, true)?, d.filter.as_ref())?;
                let input = Builder::from(input)
                    .project(vec![Expr::Column(ctid(&d.table))])?
                    .build()?;
                PlannedStatement::ModifyTable {
                    plan: ModifyTable::try_delete(
                        Target::new(d.table.clone()),
                        input,
                        ctid(&d.table),
                    )?
                    .into_plan(),
                }
            }
        })
    }
}
fn invalid<T>(message: &'static str) -> Result<T, PlanError> {
    Err(PlanError::InvalidBound(message))
}
fn depth(n: usize) -> Result<(), PlanError> {
    if n > 256 {
        Err(PlanError::ExpressionTooDeep)
    } else {
        Ok(())
    }
}
fn children<'a, 'sql>(expr: &'a b::Expr<'sql>) -> Vec<&'a b::Expr<'sql>> {
    use b::ExprKind as K;
    match &expr.kind {
        K::Unary { expr, .. } | K::Cast { expr } | K::IsNull { expr, .. } => vec![expr],
        K::Binary { left, right, .. } => vec![left, right],
        K::Aggregate(a) => a.args.iter().chain(a.filter.as_deref()).collect(),
        K::Window(w) => w
            .args
            .iter()
            .chain(w.filter.as_deref())
            .chain(&w.partition_by)
            .chain(w.order_by.iter().map(|o| &o.expr))
            .collect(),
        _ => vec![],
    }
}
// Structural equality is recursive, so validate depth before deduplication.
fn check_depth(expr: &b::Expr<'_>) -> Result<(), PlanError> {
    let mut stack = vec![(expr, 1)];
    while let Some((e, n)) = stack.pop() {
        depth(n)?;
        stack.extend(children(e).into_iter().map(|c| (c, n + 1)));
    }
    Ok(())
}
fn cast(expr: Expr, ty: &LogicalType) -> Expr {
    Expr::Cast(Cast::new(Box::new(expr), arrow_type(ty)))
}
fn literal(value: &Scalar<'_>, ty: &LogicalType) -> Result<Expr, PlanError> {
    let value = match value {
        Scalar::Null => ScalarValue::try_from(&arrow_type(ty))?,
        Scalar::Boolean(v) => ScalarValue::Boolean(Some(*v)),
        Scalar::Int32(v) => ScalarValue::Int32(Some(*v)),
        Scalar::Int64(v) => ScalarValue::Int64(Some(*v)),
        Scalar::Uint32(v) => ScalarValue::UInt32(Some(*v)),
        Scalar::Float32(v) => ScalarValue::Float32(Some(*v)),
        Scalar::Float64(v) => ScalarValue::Float64(Some(*v)),
        Scalar::String(v) => ScalarValue::Utf8(Some(v.to_string())),
    };
    Ok(Expr::Literal(value, None))
}
struct Lower<'a, 'sql> {
    schema: &'a DFSchema,
    grouped: bool,
    mapped: Vec<(&'a b::Expr<'sql>, Expr)>,
}
impl<'a, 'sql> Lower<'a, 'sql> {
    fn raw(schema: &'a DFSchema) -> Self {
        Self {
            schema,
            grouped: false,
            mapped: vec![],
        }
    }
    #[recursive::recursive]
    fn expr(&self, expr: &b::Expr<'sql>, n: usize) -> Result<Expr, PlanError> {
        depth(n)?;
        if let Some((_, mapped)) = self.mapped.iter().find(|(e, _)| *e == expr) {
            return Ok(mapped.clone());
        }
        use b::ExprKind as K;
        Ok(match &expr.kind {
            K::Literal(v) => literal(v, &expr.data_type)?,
            K::Column(binding) => {
                if self.grouped {
                    return Err(PlanError::UnknownColumn(*binding));
                }
                let c = column(*binding);
                let f = self
                    .schema
                    .field_from_column(&c)
                    .map_err(|_| PlanError::UnknownColumn(*binding))?;
                if f.data_type() != &arrow_type(&expr.data_type) {
                    return invalid("column type differs from input");
                }
                Expr::Column(c)
            }
            K::Unary { op, expr: e } => {
                let e = self.expr(e, n + 1)?;
                match op {
                    b::UnaryOp::Not => Expr::Not(Box::new(e)),
                    b::UnaryOp::Plus => e,
                    b::UnaryOp::Minus => Expr::Negative(Box::new(e)),
                }
            }
            K::Binary { op, left, right } => {
                use b::BinaryOp as O;
                let op = match op {
                    O::Or => Operator::Or,
                    O::And => Operator::And,
                    O::Eq => Operator::Eq,
                    O::NotEq => Operator::NotEq,
                    O::Less => Operator::Lt,
                    O::LessEq => Operator::LtEq,
                    O::Greater => Operator::Gt,
                    O::GreaterEq => Operator::GtEq,
                    O::Add => Operator::Plus,
                    O::Subtract => Operator::Minus,
                    O::Multiply => Operator::Multiply,
                    O::Divide => Operator::Divide,
                    O::Modulo => Operator::Modulo,
                };
                Expr::BinaryExpr(BinaryExpr::new(
                    Box::new(self.expr(left, n + 1)?),
                    op,
                    Box::new(self.expr(right, n + 1)?),
                ))
            }
            K::Cast { expr: e } => cast(self.expr(e, n + 1)?, &expr.data_type),
            K::IsNull { expr: e, negated } => {
                let e = Box::new(self.expr(e, n + 1)?);
                if *negated {
                    Expr::IsNotNull(e)
                } else {
                    Expr::IsNull(e)
                }
            }
            K::Aggregate(_) | K::Window(_) => {
                return invalid("aggregate or window is unavailable in this phase");
            }
        })
    }
    fn expressions(&self, es: &[b::Expr<'sql>]) -> Result<Vec<Expr>, PlanError> {
        es.iter().map(|e| self.expr(e, 1)).collect()
    }
    fn predicate(&self, e: Option<&b::Expr<'sql>>) -> Result<Option<Box<Expr>>, PlanError> {
        e.map(|e| {
            if e.data_type != LogicalType::Boolean {
                return invalid("filter predicate is not Boolean");
            }
            Ok(Box::new(self.expr(e, 1)?))
        })
        .transpose()
    }
    fn order(&self, order: &[b::OrderByExpr<'sql>]) -> Result<Vec<Sort>, PlanError> {
        order
            .iter()
            .map(|o| {
                Ok(Sort::new(
                    self.expr(&o.expr, 1)?,
                    o.direction == b::SortDirection::Ascending,
                    o.nulls == b::NullOrder::First,
                ))
            })
            .collect()
    }
}
fn where_filter(
    input: LogicalPlan,
    predicate: Option<&b::Expr<'_>>,
) -> Result<LogicalPlan, PlanError> {
    if let Some(p) = predicate {
        check_depth(p)?;
        let e = Lower::raw(input.schema()).predicate(Some(p))?.unwrap();
        Ok(LogicalPlan::Filter(Filter::try_new(*e, Arc::new(input))?))
    } else {
        Ok(input)
    }
}
#[recursive::recursive]
fn collect<'a, 'sql>(
    expr: &'a b::Expr<'sql>,
    windows: bool,
    found: &mut Vec<&'a b::Expr<'sql>>,
    n: usize,
) -> Result<(), PlanError> {
    depth(n)?;
    let selected = if windows {
        matches!(expr.kind, b::ExprKind::Window(_))
    } else {
        matches!(expr.kind, b::ExprKind::Aggregate(_))
    };
    if selected {
        if !found.contains(&expr) {
            found.push(expr);
        }
        return Ok(());
    }
    for c in children(expr) {
        collect(c, windows, found, n + 1)?;
    }
    Ok(())
}
fn aggregate_udf(f: b::AggregateFunction) -> Arc<AggregateUDF> {
    use datafusion_functions_aggregate::{average, count, min_max, sum};
    match f {
        b::AggregateFunction::Count => count::count_udaf(),
        b::AggregateFunction::Sum => sum::sum_udaf(),
        b::AggregateFunction::Avg => average::avg_udaf(),
        b::AggregateFunction::Min => min_max::min_udaf(),
        b::AggregateFunction::Max => min_max::max_udaf(),
    }
}
fn arguments(
    lower: &Lower<'_, '_>,
    f: b::AggregateFunction,
    args: &[b::Expr<'_>],
) -> Result<Vec<Expr>, PlanError> {
    if f == b::AggregateFunction::Count && args.is_empty() {
        Ok(vec![Expr::Literal(ScalarValue::Int64(Some(1)), None)])
    } else {
        lower.expressions(args)
    }
}
fn endpoint(b: b::FrameBound) -> WindowFrameBound {
    use b::FrameBound as B;
    match b {
        B::UnboundedPreceding => WindowFrameBound::Preceding(ScalarValue::UInt64(None)),
        B::Preceding(n) => WindowFrameBound::Preceding(ScalarValue::UInt64(Some(n))),
        B::CurrentRow => WindowFrameBound::CurrentRow,
        B::Following(n) => WindowFrameBound::Following(ScalarValue::UInt64(Some(n))),
        B::UnboundedFollowing => WindowFrameBound::Following(ScalarValue::UInt64(None)),
    }
}
fn select<'sql>(q: &b::Select<'sql>) -> Result<PlannedStatement<'sql>, PlanError> {
    for e in q
        .projection
        .iter()
        .map(|p| &p.expr)
        .chain(q.filter.as_ref())
        .chain(&q.group_by)
        .chain(q.having.as_ref())
        .chain(q.order_by.iter().map(|o| &o.expr))
    {
        check_depth(e)?;
    }
    let input = match &q.source {
        Some(t) => scan(t, false)?,
        None => Builder::empty(true).build()?,
    };
    let mut input = where_filter(input, q.filter.as_ref())?;
    let mut aggregates = vec![];
    for e in q
        .projection
        .iter()
        .map(|p| &p.expr)
        .chain(q.having.as_ref())
        .chain(q.order_by.iter().map(|o| &o.expr))
    {
        collect(e, false, &mut aggregates, 1)?;
    }
    let grouped = !q.group_by.is_empty() || q.having.is_some() || !aggregates.is_empty();
    if grouped != q.is_aggregate {
        return invalid("inconsistent is_aggregate flag");
    }
    let mut mapped = vec![];
    if grouped {
        let lower = Lower::raw(input.schema());
        let mut groups = vec![];
        for (n, e) in q.group_by.iter().enumerate() {
            let name = format!("__group{n}");
            groups.push(lower.expr(e, 1)?.alias(&name));
            mapped.push((e, Expr::Column(Column::from_name(name))));
        }
        let mut aggs = vec![];
        for (n, e) in aggregates.iter().enumerate() {
            let b::ExprKind::Aggregate(a) = &e.kind else {
                return invalid("expected aggregate");
            };
            let name = format!("__agg{n}");
            let value = Expr::AggregateFunction(AggregateFunction::new_udf(
                aggregate_udf(a.function),
                arguments(&lower, a.function, &a.args)?,
                a.distinct,
                lower.predicate(a.filter.as_deref())?,
                vec![],
                None,
            ));
            let mapped_value = Expr::Column(Column::from_name(&name));
            let mapped_value = if value.get_type(input.schema())? != arrow_type(&e.data_type) {
                cast(mapped_value, &e.data_type)
            } else {
                mapped_value
            };
            aggs.push(value.alias(name));
            mapped.push((*e, mapped_value));
        }
        if groups.is_empty() && aggs.is_empty() {
            aggs.push(
                Expr::AggregateFunction(AggregateFunction::new_udf(
                    aggregate_udf(b::AggregateFunction::Count),
                    vec![Expr::Literal(ScalarValue::Int64(Some(1)), None)],
                    false,
                    None,
                    vec![],
                    None,
                ))
                .alias("__dummy_count"),
            );
        }
        input = LogicalPlan::Aggregate(Aggregate::try_new(Arc::new(input), groups, aggs)?);
    }
    if let Some(having) = &q.having {
        let lower = Lower {
            schema: input.schema(),
            grouped,
            mapped: mapped.clone(),
        };
        let predicate = lower.predicate(Some(having))?.unwrap();
        input = LogicalPlan::Filter(Filter::try_new(*predicate, Arc::new(input))?);
    }
    let mut windows = vec![];
    for e in q
        .projection
        .iter()
        .map(|p| &p.expr)
        .chain(q.order_by.iter().map(|o| &o.expr))
    {
        collect(e, true, &mut windows, 1)?;
    }
    // Window operands see the pre-window phase, regardless of projection order.
    let window_input_schema = Arc::clone(input.schema());
    let window_mappings = mapped.clone();
    for (n, e) in windows.iter().enumerate() {
        let b::ExprKind::Window(w) = &e.kind else {
            return invalid("expected window");
        };
        let lower = Lower {
            schema: &window_input_schema,
            grouped,
            mapped: window_mappings.clone(),
        };
        use datafusion_functions_window::{rank, row_number};
        let fun = match w.function {
            b::WindowFunction::Aggregate(f) => {
                WindowFunctionDefinition::AggregateUDF(aggregate_udf(f))
            }
            b::WindowFunction::RowNumber => {
                WindowFunctionDefinition::WindowUDF(row_number::row_number_udwf())
            }
            b::WindowFunction::Rank => WindowFunctionDefinition::WindowUDF(rank::rank_udwf()),
            b::WindowFunction::DenseRank => {
                WindowFunctionDefinition::WindowUDF(rank::dense_rank_udwf())
            }
        };
        let args = match w.function {
            b::WindowFunction::Aggregate(f) => arguments(&lower, f, &w.args)?,
            _ => lower.expressions(&w.args)?,
        };
        let mut value = WindowFunction::new(fun, args);
        value.params.partition_by = lower.expressions(&w.partition_by)?;
        value.params.order_by = lower.order(&w.order_by)?;
        value.params.filter = lower.predicate(w.filter.as_deref())?;
        value.params.distinct = w.distinct;
        value.params.window_frame = WindowFrame::new_bounds(
            match w.frame.units {
                b::FrameUnits::Rows => WindowFrameUnits::Rows,
                b::FrameUnits::Range => WindowFrameUnits::Range,
            },
            endpoint(w.frame.start),
            endpoint(w.frame.end),
        );
        let name = format!("__window{n}");
        let value = Expr::WindowFunction(Box::new(value));
        let mapped_value = Expr::Column(Column::from_name(&name));
        let mapped_value = if value.get_type(input.schema())? != arrow_type(&e.data_type) {
            cast(mapped_value, &e.data_type)
        } else {
            mapped_value
        };
        input = LogicalPlan::Window(Window::try_new(vec![value.alias(name)], Arc::new(input))?);
        mapped.push((*e, mapped_value));
    }
    if !q.order_by.is_empty() {
        let order = Lower {
            schema: input.schema(),
            grouped,
            mapped: mapped.clone(),
        }
        .order(&q.order_by)?;
        input = LogicalPlan::Sort(datafusion_expr::logical_plan::Sort {
            expr: order,
            input: Arc::new(input),
            fetch: None,
        });
    }
    let lower = Lower {
        schema: input.schema(),
        grouped,
        mapped,
    };
    let expressions = q
        .projection
        .iter()
        .enumerate()
        .map(|(n, p)| Ok(lower.expr(&p.expr, 1)?.alias(format!("__output{n}"))))
        .collect::<Result<Vec<_>, PlanError>>()?;
    let output = OutputSchema {
        fields: q
            .projection
            .iter()
            .map(|p| Field {
                name: p.name.clone(),
                data_type: p.expr.data_type.clone(),
                nullable: p.expr.nullable,
                origin: match p.expr.kind {
                    b::ExprKind::Column(c) => Some(c),
                    _ => None,
                },
            })
            .collect(),
    };
    // Columns are already resolved; avoid the builder's redundant recursive
    // normalization, which can overflow for otherwise valid depth-256 trees.
    let plan = LogicalPlan::Projection(Projection::try_new(expressions, Arc::new(input))?);
    plan.check_invariants(datafusion_expr::logical_plan::InvariantLevel::Executable)?;
    Ok(PlannedStatement::Query { plan, output })
}
