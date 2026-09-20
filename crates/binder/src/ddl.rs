//! Validation of table declarations without catalog mutation.

use std::{borrow::Cow, collections::HashSet, num::NonZeroU32};

use chilidb_parser as parser;

use crate::{BindError, Binder, Catalog, LogicalType, bound};

impl<C: Catalog + ?Sized> Binder<'_, C> {
    pub(crate) fn bind_create_table<'sql>(
        &self,
        name: &Cow<'sql, str>,
        columns: &[parser::ColumnDef<'sql>],
    ) -> Result<bound::CreateTable<'sql>, BindError<C::Error>> {
        if name.is_empty() || columns.is_empty() || columns.iter().any(|c| c.name.is_empty()) {
            return Err(BindError::InvalidStatement(
                "CREATE TABLE requires a table name and named columns",
            ));
        }
        if self
            .catalog
            .get_table(name)
            .map_err(BindError::Catalog)?
            .is_some()
        {
            return Err(BindError::TableAlreadyExists(name.to_string()));
        }

        let mut names = HashSet::new();
        let mut result = bound::CreateTable {
            name: name.clone(),
            columns: Vec::with_capacity(columns.len()),
            constraints: Vec::new(),
        };
        let mut primary_key = false;
        for (index, column) in columns.iter().enumerate() {
            if !names.insert(column.name.as_ref()) {
                return Err(BindError::DuplicateColumn(column.name.to_string()));
            }
            let data_type = match column.data_type {
                parser::DataType::Integer => LogicalType::Int32,
                parser::DataType::BigInt => LogicalType::Int64,
                parser::DataType::Real => LogicalType::Float32,
                parser::DataType::Double => LogicalType::Float64,
                parser::DataType::Boolean => LogicalType::Boolean,
                parser::DataType::Text => LogicalType::Text,
                parser::DataType::Varchar(None) => LogicalType::Varchar(None),
                parser::DataType::Varchar(Some(length)) => {
                    let bound = (!length.is_empty() && length.bytes().all(|b| b.is_ascii_digit()))
                        .then(|| length.parse::<u32>().ok().and_then(NonZeroU32::new))
                        .flatten()
                        .ok_or_else(|| BindError::InvalidType(format!("VARCHAR({length})")))?;
                    LogicalType::Varchar(Some(bound))
                }
            };
            let mut seen = [false; 3];
            let mut nullable = true;
            for constraint in &column.constraints {
                let slot = match constraint {
                    parser::ColumnConstraint::NotNull => 0,
                    parser::ColumnConstraint::PrimaryKey => 1,
                    parser::ColumnConstraint::Unique => 2,
                };
                if seen[slot] {
                    return Err(BindError::InvalidConstraint(format!(
                        "duplicate {constraint:?} on {}",
                        column.name
                    )));
                }
                seen[slot] = true;
                match constraint {
                    parser::ColumnConstraint::NotNull => nullable = false,
                    parser::ColumnConstraint::PrimaryKey => {
                        if primary_key {
                            return Err(BindError::InvalidConstraint(
                                "multiple PRIMARY KEY declarations".into(),
                            ));
                        }
                        primary_key = true;
                        nullable = false;
                        result
                            .constraints
                            .push(bound::TableConstraint::PrimaryKey(vec![index]));
                    }
                    parser::ColumnConstraint::Unique => {
                        result
                            .constraints
                            .push(bound::TableConstraint::Unique(vec![index]));
                    }
                }
            }
            result.columns.push(bound::CreateColumn {
                name: column.name.clone(),
                data_type,
                nullable,
            });
        }
        Ok(result)
    }
}
