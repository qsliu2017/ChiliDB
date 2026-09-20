use std::sync::Arc;

use chilidb_binder::{
    BindError, Binder, Catalog, ColumnSchema, LogicalType as T, Scalar, TableSchema, TableSource,
    bound,
};
use chilidb_parser::{Statement, parse_sql};

#[derive(Debug)]
struct Source(Arc<TableSchema>);
impl TableSource for Source {
    fn schema(&self) -> Arc<TableSchema> {
        self.0.clone()
    }
}
struct Metadata(Arc<dyn TableSource>);
impl Catalog for Metadata {
    type Error = &'static str;
    fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
        match name {
            "t" => Ok(Some(self.0.clone())),
            "broken" => Err("offline"),
            _ => Ok(None),
        }
    }
}
fn catalog() -> Metadata {
    Metadata(Arc::new(Source(Arc::new(TableSchema {
        name: "backend_name".into(),
        columns: [
            ("id", T::Int32, false),
            ("qty", T::Uint32, true),
            ("label", T::Varchar(std::num::NonZeroU32::new(8)), false),
            ("flag", T::Boolean, true),
        ]
        .into_iter()
        .map(|(name, data_type, nullable)| ColumnSchema {
            name: name.into(),
            data_type,
            nullable,
        })
        .collect(),
    }))))
}
fn bind<'s>(
    binder: &Binder<'_, Metadata>,
    sql: &'s str,
) -> Result<bound::Statement<'s>, BindError<&'static str>> {
    binder.bind(&parse_sql(sql).unwrap()[0])
}
fn literal<'a, 's>(expr: &'a bound::Expr<'s>) -> &'a Scalar<'s> {
    match &expr.kind {
        bound::ExprKind::Cast { expr } => literal(expr),
        bound::ExprKind::Literal(value) => value,
        _ => panic!("expected literal"),
    }
}

#[test]
fn insert_normalizes_order_and_omitted_nonnullable_columns() {
    let catalog = catalog();
    let binder = Binder::new(&catalog);
    let bound::Statement::Insert(insert) = bind(
        &binder,
        "INSERT INTO t (flag, qty) VALUES (TRUE, 1), (NULL, -2)",
    )
    .unwrap() else {
        panic!()
    };
    assert!(Arc::ptr_eq(&insert.table.source, &catalog.0));
    for row in &insert.rows {
        assert_eq!(row.len(), 4);
        assert_eq!(
            row.iter().map(|e| e.data_type.clone()).collect::<Vec<_>>(),
            vec![
                T::Int32,
                T::Uint32,
                T::Varchar(std::num::NonZeroU32::new(8)),
                T::Boolean
            ]
        );
        for index in [0, 2] {
            assert_eq!(literal(&row[index]), &Scalar::Null);
            assert!(row[index].nullable);
        }
        assert!(matches!(row[1].kind, bound::ExprKind::Cast { .. }));
    }
    assert_eq!(literal(&insert.rows[0][1]), &Scalar::Int32(1));
    assert_eq!(literal(&insert.rows[1][1]), &Scalar::Int32(-2));
    let bound::Statement::Insert(insert) = bind(
        &binder,
        "INSERT INTO t VALUES (1.5, 2, 'longer than eight characters', NULL)",
    )
    .unwrap() else {
        panic!()
    };
    assert!(matches!(
        insert.rows[0][0].kind,
        bound::ExprKind::Cast { .. }
    ));
    assert!(matches!(
        insert.rows[0][2].kind,
        bound::ExprKind::Cast { .. }
    ));
}

#[test]
fn targets_and_every_row_width_are_checked() {
    let catalog = catalog();
    let binder = Binder::new(&catalog);
    for sql in [
        "INSERT INTO t (id, id) VALUES (1, 2)",
        "UPDATE t SET id = 1, id = 2",
    ] {
        assert_eq!(
            bind(&binder, sql).unwrap_err(),
            BindError::DuplicateColumn("id".into())
        );
    }
    for sql in [
        "INSERT INTO t (absent) VALUES (1)",
        "UPDATE t SET absent = 1",
    ] {
        assert_eq!(
            bind(&binder, sql).unwrap_err(),
            BindError::UnknownColumn {
                qualifier: None,
                name: "absent".into()
            }
        );
    }
    assert_eq!(
        bind(&binder, "INSERT INTO t (id) VALUES (1), (2, 3)").unwrap_err(),
        BindError::RowWidth {
            row: 2,
            expected: 1,
            actual: 2
        }
    );
    assert_eq!(
        bind(&binder, "INSERT INTO t VALUES (1)").unwrap_err(),
        BindError::RowWidth {
            row: 1,
            expected: 4,
            actual: 1
        }
    );
    for statement in [
        Statement::Insert {
            table: "t".into(),
            columns: vec![],
            rows: vec![],
        },
        Statement::Update {
            table: "t".into(),
            assignments: vec![],
            filter: None,
        },
    ] {
        assert!(matches!(
            binder.bind(&statement),
            Err(BindError::InvalidStatement(_))
        ));
    }
}

#[test]
fn values_have_no_target_scope_and_expressions_are_typed() {
    let catalog = catalog();
    let binder = Binder::new(&catalog);
    for sql in [
        "INSERT INTO t (id) VALUES (id)",
        "INSERT INTO t (id) VALUES (t.id)",
    ] {
        assert!(matches!(
            bind(&binder, sql),
            Err(BindError::UnknownColumn { .. })
        ));
    }
    for sql in [
        "INSERT INTO t (id) VALUES ('1')",
        "INSERT INTO t (flag) VALUES (1)",
        "UPDATE t SET id = label",
        "UPDATE t SET label = TRUE",
        "INSERT INTO t (id) VALUES (1 + '2')",
    ] {
        assert!(
            matches!(
                bind(&binder, sql),
                Err(BindError::IncompatibleTypes { .. } | BindError::TypeMismatch { .. })
            ),
            "{sql}"
        );
    }
}

#[test]
fn update_reads_original_row_and_nullability_is_not_a_constraint_check() {
    let catalog = catalog();
    let binder = Binder::new(&catalog);
    let bound::Statement::Update(update) = bind(
        &binder,
        "UPDATE t SET id = qty, qty = t.id, label = NULL WHERE t.flag",
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(update.table.relation, bound::RelationId(0));
    for (a, target, source) in [
        (&update.assignments[0], 0, 1),
        (&update.assignments[1], 1, 0),
    ] {
        assert_eq!(a.column_index, target);
        let bound::ExprKind::Cast { expr } = &a.value.kind else {
            panic!()
        };
        assert_eq!(
            expr.kind,
            bound::ExprKind::Column(bound::ColumnBinding {
                relation: bound::RelationId(0),
                column_index: source
            })
        );
    }
    assert!(update.assignments[0].value.nullable);
    assert!(update.assignments[2].value.nullable);
    assert_eq!(literal(&update.assignments[2].value), &Scalar::Null);
    assert_eq!(update.filter.unwrap().data_type, T::Boolean);
}

#[test]
fn predicates_target_names_and_reuse_are_isolated() {
    let catalog = catalog();
    let binder = Binder::new(&catalog);
    for sql in ["DELETE FROM t WHERE 1", "UPDATE t SET id = 1 WHERE 'yes'"] {
        assert!(matches!(
            bind(&binder, sql),
            Err(BindError::TypeMismatch { .. })
        ));
    }
    for sql in [
        "DELETE FROM t WHERE backend_name.flag",
        "UPDATE t SET id = other.id",
    ] {
        assert!(matches!(
            bind(&binder, sql),
            Err(BindError::UnknownColumn { .. })
        ));
    }
    for _ in 0..2 {
        let bound::Statement::Delete(delete) =
            bind(&binder, "DELETE FROM t WHERE t.id > 0").unwrap()
        else {
            panic!()
        };
        assert!(Arc::ptr_eq(&delete.table.source, &catalog.0));
        assert_eq!(delete.table.relation, bound::RelationId(0));
        assert!(matches!(
            bind(&binder, "INSERT INTO t (id) VALUES (id)"),
            Err(BindError::UnknownColumn { .. })
        ));
        assert!(matches!(
            bind(&binder, "SELECT id"),
            Err(BindError::UnknownColumn { .. })
        ));
    }
    let bound::Statement::Delete(delete) = bind(&binder, "DELETE FROM t WHERE NULL").unwrap()
    else {
        panic!()
    };
    assert_eq!(delete.filter.unwrap().data_type, T::Boolean);
    let bound::Statement::Delete(delete) = bind(&binder, "DELETE FROM t").unwrap() else {
        panic!()
    };
    assert!(delete.filter.is_none());
    for sql in [
        "DELETE FROM broken",
        "UPDATE broken SET id = 1",
        "INSERT INTO broken VALUES (1)",
    ] {
        assert_eq!(
            bind(&binder, sql).unwrap_err(),
            BindError::Catalog("offline")
        );
    }
    assert_eq!(
        bind(&binder, "DELETE FROM absent").unwrap_err(),
        BindError::UnknownTable("absent".into())
    );
    assert_eq!(catalog.0.schema().name, "backend_name");
}

#[test]
fn ambiguous_target_metadata_is_rejected() {
    let column = ColumnSchema {
        name: "id".into(),
        data_type: T::Int32,
        nullable: false,
    };
    let catalog = Metadata(Arc::new(Source(Arc::new(TableSchema {
        name: "t".into(),
        columns: vec![column.clone(), column],
    }))));
    let binder = Binder::new(&catalog);
    for sql in ["INSERT INTO t (id) VALUES (1)", "UPDATE t SET id = 1"] {
        assert_eq!(
            bind(&binder, sql).unwrap_err(),
            BindError::AmbiguousColumn {
                qualifier: None,
                name: "id".into()
            }
        );
    }
}
