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
            } => bound::Statement::Select(self.bind_select(
                projection,
                from.as_ref(),
                filter.as_ref(),
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

    fn bind_select<'sql>(
        &self,
        projection: &[Expr<'sql>],
        from: Option<&Cow<'sql, str>>,
        filter: Option<&Expr<'sql>>,
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
                    let name = match expr {
                        Expr::Identifier(name) | Expr::QualifiedIdentifier { column: name, .. } => {
                            name.clone()
                        }
                        _ => Cow::Borrowed("?column?"),
                    };
                    Ok(bound::NamedExpr {
                        name,
                        expr: expression::bind_expr(expr, &context)?,
                    })
                })
                .collect::<Result<Vec<_>, BindError<C::Error>>>()?
        };
        let filter = filter
            .map(|expr| expression::boolean(expression::bind_expr(expr, &context)?, "WHERE"))
            .transpose()?;
        Ok(bound::Select {
            source,
            projection,
            filter,
        })
    }
}
