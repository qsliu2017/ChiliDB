//! Statement dispatch, table lookup, and SELECT projection binding.

use std::borrow::Cow;

use chilidb_parser::{Expr, Statement};

use crate::{BindContext, BindError, Binder, Catalog, RelationBinding, Scope, bound, expression};

impl<'catalog, C: Catalog + ?Sized> Binder<'catalog, C> {
    /// Create a binder borrowing a consistent catalog view.
    pub fn new(catalog: &'catalog C) -> Self {
        Self { catalog }
    }

    /// Bind a parsed statement into a resolved command and typed expressions.
    ///
    /// Each call starts a fresh query scope. The output retains table sources
    /// and borrows SQL text, not the parsed statement or the binder. Owned text
    /// in the parsed AST is copied when it must also be owned by the bound AST.
    /// This does not evaluate expressions or execute the query.
    ///
    /// # Errors
    /// Returns catalog, name-resolution, type, or declaration errors. Catalog
    /// mutation, constraint enforcement, and transaction state changes belong
    /// to execution, not binding.
    pub fn bind<'sql>(
        &self,
        statement: &Statement<'sql>,
    ) -> Result<bound::Statement<'sql>, BindError<C::Error>> {
        Ok(match statement {
            Statement::Select {
                projection,
                from,
                filter,
                group_by,
                having,
                order_by,
            } => bound::Statement::Select(self.bind_select(
                projection,
                from.as_ref(),
                filter.as_ref(),
                group_by,
                having.as_ref(),
                order_by,
            )?),
            Statement::CreateTable { name, columns } => {
                bound::Statement::CreateTable(self.bind_create_table(name, columns)?)
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => bound::Statement::Insert(self.bind_insert(table, columns, rows)?),
            Statement::Update {
                table,
                assignments,
                filter,
            } => bound::Statement::Update(self.bind_update(table, assignments, filter.as_ref())?),
            Statement::Delete { table, filter } => {
                bound::Statement::Delete(self.bind_delete(table, filter.as_ref())?)
            }
            Statement::Begin => bound::Statement::Begin,
            Statement::Commit => bound::Statement::Commit,
            Statement::Rollback => bound::Statement::Rollback,
        })
    }

    pub(crate) fn lookup_table(&self, name: &str) -> Result<bound::Table, BindError<C::Error>> {
        let source = self
            .catalog
            .get_table(name)
            .map_err(BindError::Catalog)?
            .ok_or_else(|| BindError::UnknownTable(name.to_owned()))?;
        Ok(bound::Table {
            relation: bound::RelationId(0),
            source,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn bind_select<'sql>(
        &self,
        projection: &[Expr<'sql>],
        from: Option<&Cow<'sql, str>>,
        filter: Option<&Expr<'sql>>,
        group_by: &[Expr<'sql>],
        having: Option<&Expr<'sql>>,
        order_by: &[chilidb_parser::OrderByExpr<'sql>],
    ) -> Result<bound::Select<'sql>, BindError<C::Error>> {
        if projection.is_empty() {
            return Err(BindError::EmptyProjection);
        }
        let source = from.map(|name| self.lookup_table(name)).transpose()?;
        let relations = match (&source, from) {
            (Some(table), Some(name)) => vec![RelationBinding {
                name: name.clone(),
                table: table.clone(),
            }],
            _ => vec![],
        };
        let context = BindContext {
            scopes: vec![Scope { relations }],
        };
        let projection = if matches!(projection, [Expr::Wildcard]) {
            let table = source.as_ref().ok_or(BindError::InvalidWildcard)?;
            table
                .source
                .schema()
                .columns
                .iter()
                .enumerate()
                .map(|(column_index, column)| bound::NamedExpr {
                    name: Cow::Owned(column.name.clone()),
                    expr: bound::Expr {
                        data_type: column.data_type.clone(),
                        nullable: column.nullable,
                        kind: bound::ExprKind::Column(bound::ColumnBinding {
                            relation: table.relation,
                            column_index,
                        }),
                    },
                })
                .collect()
        } else {
            projection
                .iter()
                .map(|expr| {
                    let (expr, alias) = match expr {
                        Expr::Alias { expr, alias } => (expr.as_ref(), Some(alias)),
                        _ => (expr, None),
                    };
                    let name = match expr {
                        Expr::Identifier(name) | Expr::QualifiedIdentifier { column: name, .. } => {
                            name.clone()
                        }
                        _ => Cow::Borrowed("?column?"),
                    };
                    Ok(bound::NamedExpr {
                        name: alias.cloned().unwrap_or(name),
                        expr: expression::bind_expr_inner(
                            expr,
                            &context,
                            usize::from(alias.is_some()),
                            true,
                            true,
                        )?,
                    })
                })
                .collect::<Result<Vec<_>, BindError<C::Error>>>()?
        };
        let filter = filter
            .map(|expr| expression::boolean(expression::bind_expr(expr, &context)?, "WHERE"))
            .transpose()?;
        let group_by = group_by
            .iter()
            .map(|expr| expression::bind_expr(expr, &context))
            .collect::<Result<Vec<_>, BindError<C::Error>>>()?;
        let having = having
            .map(|expr| {
                expression::boolean(
                    expression::bind_expr_inner(expr, &context, 0, true, false)?,
                    "HAVING",
                )
            })
            .transpose()?;
        let order_by = order_by
            .iter()
            .map(|order| {
                let expr = match &order.expr {
                    Expr::Identifier(name) => {
                        let matches: Vec<_> =
                            projection.iter().filter(|p| p.name == *name).collect();
                        match matches.as_slice() {
                            [p] => p.expr.clone(),
                            [] => {
                                expression::bind_expr_inner(&order.expr, &context, 0, true, true)?
                            }
                            _ => {
                                return Err(BindError::AmbiguousColumn {
                                    qualifier: None,
                                    name: name.to_string(),
                                });
                            }
                        }
                    }
                    Expr::Literal(chilidb_parser::Literal::Number(n))
                        if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) =>
                    {
                        let ordinal: usize = n.parse().map_err(|_| {
                            BindError::InvalidStatement("ORDER BY ordinal out of range")
                        })?;
                        projection
                            .get(ordinal.checked_sub(1).ok_or(BindError::InvalidStatement(
                                "ORDER BY ordinal out of range",
                            ))?)
                            .ok_or(BindError::InvalidStatement("ORDER BY ordinal out of range"))?
                            .expr
                            .clone()
                    }
                    _ => expression::bind_expr_inner(&order.expr, &context, 0, true, true)?,
                };
                Ok(crate::aggregate::order(expr, order))
            })
            .collect::<Result<Vec<_>, BindError<C::Error>>>()?;
        let is_aggregate = !group_by.is_empty()
            || having.is_some()
            || projection
                .iter()
                .any(|p| crate::aggregate::contains_aggregate(&p.expr))
            || order_by
                .iter()
                .any(|o| crate::aggregate::contains_aggregate(&o.expr));
        if is_aggregate {
            for expr in projection
                .iter()
                .map(|p| &p.expr)
                .chain(having.iter())
                .chain(order_by.iter().map(|o| &o.expr))
            {
                crate::aggregate::validate_grouped::<C::Error>(expr, &group_by)?;
            }
        }
        Ok(bound::Select {
            source,
            projection,
            filter,
            group_by,
            having,
            order_by,
            is_aggregate,
        })
    }
}
