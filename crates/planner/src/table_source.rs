use std::sync::Arc;

use arrow_schema::{DataType, Field, Schema, SchemaRef};
use chilidb_binder::{LogicalType, bound};
use datafusion_common::{Column, Result, TableReference};
use datafusion_expr::{LogicalPlan, LogicalPlanBuilder, TableSource};

use crate::Ctid;

/// Arrow representation of a physical tuple-version address encoded by `Ctid::to_le_bytes`.
pub fn ctid_type() -> DataType {
    DataType::FixedSizeBinary(Ctid::BYTE_LEN as i32)
}

pub fn arrow_type(data_type: &LogicalType) -> DataType {
    match data_type {
        LogicalType::Null => DataType::Null,
        LogicalType::Boolean => DataType::Boolean,
        LogicalType::Int32 => DataType::Int32,
        LogicalType::Int64 => DataType::Int64,
        LogicalType::Uint32 => DataType::UInt32,
        LogicalType::Float32 => DataType::Float32,
        LogicalType::Float64 => DataType::Float64,
        LogicalType::Text | LogicalType::Varchar(_) => DataType::Utf8,
    }
}

fn qualifier(relation: bound::RelationId) -> TableReference {
    TableReference::bare(format!("__r{}", relation.0))
}

pub fn column(binding: bound::ColumnBinding) -> Column {
    Column::new(
        Some(qualifier(binding.relation)),
        format!("__c{}", binding.column_index),
    )
}

pub fn ctid(table: &bound::Table) -> Column {
    Column::new(Some(qualifier(table.relation)), "__ctid")
}

/// Retains SQL metadata separately from generated optimizer-facing field names.
#[derive(Clone, Debug)]
pub struct TableAdapter {
    table: bound::Table,
    with_ctid: bool,
    schema: SchemaRef,
}

impl TableAdapter {
    pub fn new(table: bound::Table, with_ctid: bool) -> Self {
        let mut fields: Vec<_> = table
            .source
            .schema()
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                Field::new(
                    format!("__c{index}"),
                    arrow_type(&column.data_type),
                    column.nullable,
                )
            })
            .collect();
        if with_ctid {
            fields.push(Field::new("__ctid", ctid_type(), false));
        }
        Self {
            table,
            with_ctid,
            schema: Arc::new(Schema::new(fields)),
        }
    }

    pub fn table(&self) -> &bound::Table {
        &self.table
    }
    pub fn with_ctid(&self) -> bool {
        self.with_ctid
    }
}

impl TableSource for TableAdapter {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

pub fn scan(table: &bound::Table, with_ctid: bool) -> Result<LogicalPlan> {
    LogicalPlanBuilder::scan(
        qualifier(table.relation),
        Arc::new(TableAdapter::new(table.clone(), with_ctid)),
        None,
    )?
    .build()
}
