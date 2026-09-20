use std::sync::Arc;

use chilidb_binder::{Binder, Catalog, ColumnSchema, LogicalType, TableSchema, TableSource, bound};

#[derive(Debug)]
struct TestSource {
    backend_id: u64,
    schema: Arc<TableSchema>,
}

impl TableSource for TestSource {
    fn schema(&self) -> Arc<TableSchema> {
        Arc::clone(&self.schema)
    }
}

// The catalog itself does not need Clone, Debug, Send, or Sync.
struct TestCatalog {
    source: Arc<dyn TableSource>,
}

impl Catalog for TestCatalog {
    type Error = &'static str;

    fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
        match name {
            "unavailable" => Err("catalog unavailable"),
            "items" => Ok(Some(Arc::clone(&self.source))),
            _ => Ok(None),
        }
    }
}

#[test]
fn binder_uses_a_source_handle_without_catalog_id_parameters() {
    let schema = Arc::new(TableSchema {
        name: "items".into(),
        columns: vec![ColumnSchema {
            name: "id".into(),
            data_type: LogicalType::Int32,
            nullable: false,
        }],
    });
    let catalog = TestCatalog {
        source: Arc::new(TestSource {
            backend_id: 7,
            schema: Arc::clone(&schema),
        }),
    };
    let binder = Binder { catalog: &catalog };
    let source = binder.catalog.get_table("items").unwrap().unwrap();
    assert!(Arc::ptr_eq(&source, &catalog.source));
    assert!(Arc::ptr_eq(&source.schema(), &schema));
    assert!(binder.catalog.get_table("missing").unwrap().is_none());
    assert!(binder.catalog.get_table("unavailable").is_err());

    let relation = bound::RelationId(0);
    let expr: bound::Expr<'_> = bound::Expr {
        data_type: LogicalType::Int32,
        nullable: false,
        kind: bound::ExprKind::Column(bound::ColumnBinding {
            relation,
            column_index: 0,
        }),
    };
    let statement: bound::Statement<'_> = bound::Statement::Select(bound::Select {
        source: Some(bound::Table { relation, source }),
        projection: vec![bound::NamedExpr {
            name: "id".into(),
            expr,
        }],
        filter: None,
        group_by: vec![],
        having: None,
        order_by: vec![],
        is_aggregate: false,
    });
    let bound::Statement::Select(query) = statement.clone() else {
        panic!("expected SELECT")
    };
    let table = query.source.unwrap();
    assert!(Arc::ptr_eq(&table.source, &catalog.source));
    assert!(table.source.is::<TestSource>());
    assert_eq!(
        table
            .source
            .downcast_ref::<TestSource>()
            .unwrap()
            .backend_id,
        7
    );
}

#[test]
fn identical_metadata_does_not_erase_source_identity() {
    let schema = Arc::new(TableSchema {
        name: "items".into(),
        columns: vec![],
    });
    let source = |backend_id| -> Arc<dyn TableSource> {
        Arc::new(TestSource {
            backend_id,
            schema: Arc::clone(&schema),
        })
    };
    let a = source(1);
    let b = source(2);
    assert!(Arc::ptr_eq(&a.schema(), &b.schema()));
    assert!(!Arc::ptr_eq(&a, &b));
    assert_ne!(
        a.downcast_ref::<TestSource>().unwrap().backend_id,
        b.downcast_ref::<TestSource>().unwrap().backend_id,
    );
}
