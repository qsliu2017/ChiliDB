use std::convert::Infallible;
use std::sync::Arc;

use chilidb::binder::{Catalog, ColumnSchema, LogicalType, TableSchema, TableSource};

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

/// Exposes items(id Int32 NOT NULL) without storage or catalog mutation.
pub fn catalog() -> impl Catalog<Error = Infallible> {
    DemoCatalog(Arc::new(Items(Arc::new(TableSchema {
        name: "items".into(),
        columns: vec![ColumnSchema {
            name: "id".into(),
            data_type: LogicalType::Int32,
            nullable: false,
        }],
    }))))
}
