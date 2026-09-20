//! Inspect a typed query: cargo run --example bind -- 'SELECT id + 1 FROM items'
use std::convert::Infallible;
use std::sync::Arc;

use chilidb::binder::{Binder, Catalog, ColumnSchema, LogicalType, TableSchema, TableSource};

#[derive(Debug)]
struct Items(Arc<TableSchema>);

impl TableSource for Items {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.0)
    }
}

struct DemoCatalog(Arc<dyn TableSource>);

impl Catalog for DemoCatalog {
    type Error = Infallible;

    fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
        Ok((name == "items").then(|| Arc::clone(&self.0)))
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "SELECT id + 1 FROM items WHERE id > 0".into());
    let catalog = DemoCatalog(Arc::new(Items(Arc::new(TableSchema {
        name: "items".into(),
        columns: vec![ColumnSchema {
            name: "id".into(),
            data_type: LogicalType::Int32,
            nullable: false,
        }],
    }))));
    let binder = Binder::new(&catalog);
    for statement in chilidb::parser::parse_sql(&sql)? {
        println!("{:#?}", binder.bind(&statement)?);
    }
    Ok(())
}
