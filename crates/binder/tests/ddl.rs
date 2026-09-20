//! CREATE TABLE semantic validation and read-only catalog behavior.
use std::{cell::RefCell, num::NonZeroU32, sync::Arc};

use chilidb_binder::{BindError, Binder, Catalog, LogicalType, TableSchema, TableSource, bound};
use chilidb_parser::{ColumnDef, DataType, Statement, parse_sql};

#[derive(Debug)]
struct Source;
impl TableSource for Source {
    fn schema(&self) -> Arc<TableSchema> {
        panic!("CREATE TABLE must not inspect existing table metadata")
    }
}

#[derive(Default)]
struct TestCatalog {
    lookups: RefCell<Vec<String>>,
    existing: bool,
    failure: bool,
}
impl Catalog for TestCatalog {
    type Error = &'static str;
    fn get_table(&self, name: &str) -> Result<Option<Arc<dyn TableSource>>, Self::Error> {
        self.lookups.borrow_mut().push(name.into());
        if self.failure {
            Err("catalog unavailable")
        } else {
            Ok(self
                .existing
                .then(|| Arc::new(Source) as Arc<dyn TableSource>))
        }
    }
}

fn create<'sql>(
    catalog: &TestCatalog,
    sql: &'sql str,
) -> Result<bound::CreateTable<'sql>, BindError<&'static str>> {
    let parsed = parse_sql(sql).unwrap();
    match Binder::new(catalog).bind(&parsed[0])? {
        bound::Statement::CreateTable(table) => Ok(table),
        other => panic!("expected CREATE TABLE, got {other:?}"),
    }
}

#[test]
fn maps_types_without_mutating_catalog() {
    let catalog = TestCatalog::default();
    let sql = "CREATE TABLE T (a INTEGER, b BIGINT, c REAL, d DOUBLE, e BOOLEAN, f TEXT, g VARCHAR, h VARCHAR(1), i VARCHAR(4294967295))";
    let table = create(&catalog, sql).unwrap();
    assert_eq!(table.name, "t");
    assert_eq!(
        table
            .columns
            .iter()
            .map(|c| c.data_type.clone())
            .collect::<Vec<_>>(),
        vec![
            LogicalType::Int32,
            LogicalType::Int64,
            LogicalType::Float32,
            LogicalType::Float64,
            LogicalType::Boolean,
            LogicalType::Text,
            LogicalType::Varchar(None),
            LogicalType::Varchar(NonZeroU32::new(1)),
            LogicalType::Varchar(NonZeroU32::new(u32::MAX)),
        ]
    );
    assert!(table.columns.iter().all(|c| c.nullable));
    assert!(table.constraints.is_empty());
    assert_eq!(create(&catalog, sql).unwrap(), table);
    assert_eq!(*catalog.lookups.borrow(), ["t", "t"]);
}

#[test]
fn catalog_conflicts_and_failures_are_preserved() {
    let existing = TestCatalog {
        existing: true,
        ..Default::default()
    };
    assert_eq!(
        create(&existing, "CREATE TABLE T (x INT)"),
        Err(BindError::TableAlreadyExists("t".into()))
    );
    assert_eq!(*existing.lookups.borrow(), ["t"]);
    let broken = TestCatalog {
        failure: true,
        ..Default::default()
    };
    assert_eq!(
        create(&broken, "CREATE TABLE t (x INT)"),
        Err(BindError::Catalog("catalog unavailable"))
    );
}

#[test]
fn identifier_normalization_is_not_repeated() {
    let catalog = TestCatalog::default();
    for sql in [
        "CREATE TABLE t (X INT, x INT)",
        "CREATE TABLE t (x INT, \"x\" INT)",
    ] {
        assert_eq!(
            create(&catalog, sql),
            Err(BindError::DuplicateColumn("x".into()))
        );
    }
    let table = create(&catalog, "CREATE TABLE \"T\" (x INT, \"X\" INT)").unwrap();
    assert_eq!(table.name, "T");
    assert_eq!(table.columns[1].name, "X");
    assert_eq!(catalog.lookups.borrow().last().unwrap(), "T");
}

#[test]
fn constraint_order_and_nullability() {
    use bound::TableConstraint::{PrimaryKey, Unique};
    let table = create(
        &TestCatalog::default(),
        "CREATE TABLE t (a INT UNIQUE, b INT NOT NULL, c INT UNIQUE NOT NULL PRIMARY KEY, d INT)",
    )
    .unwrap();
    assert_eq!(
        table.columns.iter().map(|c| c.nullable).collect::<Vec<_>>(),
        [true, false, false, true]
    );
    assert_eq!(
        table.constraints,
        [Unique(vec![0]), Unique(vec![2]), PrimaryKey(vec![2])]
    );
    let table = create(
        &TestCatalog::default(),
        "CREATE TABLE t (a INT PRIMARY KEY UNIQUE NOT NULL)",
    )
    .unwrap();
    assert!(!table.columns[0].nullable);
    assert_eq!(table.constraints, [PrimaryKey(vec![0]), Unique(vec![0])]);
    let table = create(
        &TestCatalog::default(),
        "CREATE TABLE t (a INT PRIMARY KEY)",
    )
    .unwrap();
    assert!(!table.columns[0].nullable);
}

#[test]
fn rejects_duplicate_constraints_and_multiple_primary_keys() {
    for columns in [
        "a INT NOT NULL NOT NULL",
        "a INT UNIQUE UNIQUE",
        "a INT PRIMARY KEY PRIMARY KEY",
        "a INT PRIMARY KEY, b INT PRIMARY KEY",
    ] {
        assert!(matches!(
            create(
                &TestCatalog::default(),
                &format!("CREATE TABLE t ({columns})")
            ),
            Err(BindError::InvalidConstraint(_))
        ));
    }
}

#[test]
fn validates_varchar_lengths_even_for_hand_built_ast() {
    let catalog = TestCatalog::default();
    for length in [
        "0",
        "000",
        "4294967296",
        "99999999999999999999999999",
        "",
        "+1",
        "-1",
        "1.0",
        "1e2",
        " 1",
        "1 ",
        "١",
    ] {
        let statement = Statement::CreateTable {
            name: "t".into(),
            columns: vec![ColumnDef {
                name: "a".into(),
                data_type: DataType::Varchar(Some(length)),
                constraints: vec![],
            }],
        };
        assert!(
            matches!(
                Binder::new(&catalog).bind(&statement),
                Err(BindError::InvalidType(_))
            ),
            "length {length:?}"
        );
    }
    assert_eq!(
        create(&catalog, "CREATE TABLE t (a VARCHAR(0001))")
            .unwrap()
            .columns[0]
            .data_type,
        LogicalType::Varchar(NonZeroU32::new(1))
    );
}

#[test]
fn rejects_empty_manual_declarations() {
    let catalog = TestCatalog::default();
    for (name, names) in [("", vec!["a"]), ("t", vec![]), ("t", vec![""])] {
        let statement = Statement::CreateTable {
            name: name.into(),
            columns: names
                .into_iter()
                .map(|name| ColumnDef {
                    name: name.into(),
                    data_type: DataType::Integer,
                    constraints: vec![],
                })
                .collect(),
        };
        assert!(matches!(
            Binder::new(&catalog).bind(&statement),
            Err(BindError::InvalidStatement(_))
        ));
    }
}
