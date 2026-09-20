//! Target resolution and typing for data manipulation, without executing writes.

use std::{borrow::Cow, collections::HashSet};

use chilidb_parser as parser;

use crate::{
    BindContext, BindError, Binder, Catalog, LogicalType, RelationBinding, Scalar, Scope,
    TableSchema, bound,
    expression::{assignment, bind_expr, boolean},
};

fn target<E>(schema: &TableSchema, name: &str) -> Result<usize, BindError<E>> {
    let mut matches = schema
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name == name);
    let Some((index, _)) = matches.next() else {
        return Err(BindError::UnknownColumn {
            qualifier: None,
            name: name.into(),
        });
    };
    if matches.next().is_some() {
        return Err(BindError::AmbiguousColumn {
            qualifier: None,
            name: name.into(),
        });
    }
    Ok(index)
}

fn context<'sql>(name: &Cow<'sql, str>, table: &bound::Table) -> BindContext<'sql> {
    BindContext {
        scopes: vec![Scope {
            relations: vec![RelationBinding {
                name: name.clone(),
                table: table.clone(),
            }],
        }],
    }
}

impl<C: Catalog + ?Sized> Binder<'_, C> {
    pub(crate) fn bind_insert<'sql>(
        &self,
        table_name: &Cow<'sql, str>,
        columns: &[Cow<'sql, str>],
        rows: &[Vec<parser::Expr<'sql>>],
    ) -> Result<bound::Insert<'sql>, BindError<C::Error>> {
        let table = self.lookup_table(table_name)?;
        let schema = table.source.schema();
        let targets = if columns.is_empty() {
            (0..schema.columns.len()).collect::<Vec<_>>()
        } else {
            let mut seen = HashSet::new();
            columns
                .iter()
                .map(|name| {
                    let index = target(&schema, name)?;
                    if !seen.insert(index) {
                        return Err(BindError::DuplicateColumn(name.to_string()));
                    }
                    Ok(index)
                })
                .collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            return Err(BindError::InvalidStatement(
                "INSERT requires at least one row",
            ));
        }
        for (index, row) in rows.iter().enumerate() {
            if row.len() != targets.len() {
                return Err(BindError::RowWidth {
                    row: index + 1,
                    expected: targets.len(),
                    actual: row.len(),
                });
            }
            if row.is_empty() {
                return Err(BindError::InvalidStatement("INSERT requires nonempty rows"));
            }
        }
        let empty = BindContext { scopes: vec![] };
        let rows = rows
            .iter()
            .map(|row| {
                let mut normalized: Vec<Option<bound::Expr<'sql>>> =
                    std::iter::repeat_with(|| None)
                        .take(schema.columns.len())
                        .collect();
                for (expr, &index) in row.iter().zip(&targets) {
                    normalized[index] = Some(assignment(
                        bind_expr(expr, &empty)?,
                        &schema.columns[index].data_type,
                    )?);
                }
                normalized
                    .into_iter()
                    .zip(&schema.columns)
                    .map(|(value, column)| match value {
                        Some(value) => Ok(value),
                        None => assignment(
                            bound::Expr {
                                data_type: LogicalType::Null,
                                nullable: true,
                                kind: bound::ExprKind::Literal(Scalar::Null),
                            },
                            &column.data_type,
                        ),
                    })
                    .collect::<Result<Vec<_>, BindError<C::Error>>>()
            })
            .collect::<Result<Vec<_>, BindError<C::Error>>>()?;
        Ok(bound::Insert { table, rows })
    }

    pub(crate) fn bind_update<'sql>(
        &self,
        table_name: &Cow<'sql, str>,
        assignments: &[parser::Assignment<'sql>],
        filter: Option<&parser::Expr<'sql>>,
    ) -> Result<bound::Update<'sql>, BindError<C::Error>> {
        let table = self.lookup_table(table_name)?;
        let schema = table.source.schema();
        if assignments.is_empty() {
            return Err(BindError::InvalidStatement(
                "UPDATE requires at least one assignment",
            ));
        }
        let context = context(table_name, &table);
        let mut seen = HashSet::new();
        let assignments = assignments
            .iter()
            .map(|a| {
                let column_index = target(&schema, &a.column)?;
                if !seen.insert(column_index) {
                    return Err(BindError::DuplicateColumn(a.column.to_string()));
                }
                Ok(bound::Assignment {
                    column_index,
                    value: assignment(
                        bind_expr(&a.value, &context)?,
                        &schema.columns[column_index].data_type,
                    )?,
                })
            })
            .collect::<Result<Vec<_>, BindError<C::Error>>>()?;
        let filter = filter
            .map(|expr| boolean(bind_expr(expr, &context)?, "WHERE"))
            .transpose()?;
        Ok(bound::Update {
            table,
            assignments,
            filter,
        })
    }

    pub(crate) fn bind_delete<'sql>(
        &self,
        table_name: &Cow<'sql, str>,
        filter: Option<&parser::Expr<'sql>>,
    ) -> Result<bound::Delete<'sql>, BindError<C::Error>> {
        let table = self.lookup_table(table_name)?;
        let context = context(table_name, &table);
        let filter = filter
            .map(|expr| boolean(bind_expr(expr, &context)?, "WHERE"))
            .transpose()?;
        Ok(bound::Delete { table, filter })
    }
}
