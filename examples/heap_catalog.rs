//! Create, flush, and reopen a catalog in a temporary page file.
use std::sync::Arc;

use chilidb::{
    binder::{Binder, TableSource, bound},
    catalog::HeapCatalog,
    storage::{BufferPool, FilePageStore},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("pages");
    let root = {
        let pool = Arc::new(BufferPool::new(1, Arc::new(FilePageStore::open(&path)?))?);
        let catalog = HeapCatalog::create(pool.clone())?;
        let sql = "CREATE TABLE items (id INT PRIMARY KEY, name VARCHAR(32))";
        let parsed = chilidb::parser::parse_sql(sql)?;
        let bound::Statement::CreateTable(declaration) = Binder::new(&catalog).bind(&parsed[0])?
        else {
            unreachable!("constant CREATE TABLE statement")
        };
        let table = catalog.create_table(&declaration)?;
        println!(
            "created catalog {}; items heap {}",
            catalog.root_page_id(),
            table.heap().head_page_id()
        );
        pool.flush_all()?;
        catalog.root_page_id()
    };
    let pool = Arc::new(BufferPool::new(1, Arc::new(FilePageStore::open(path)?))?);
    let catalog = HeapCatalog::open(pool, root)?;
    let table = catalog.table("items")?.expect("persisted table");
    println!(
        "reopened {} ({} columns)",
        table.schema().name,
        table.schema().columns.len()
    );
    let parsed = chilidb::parser::parse_sql("SELECT id, name FROM items")?;
    Binder::new(&catalog).bind(&parsed[0])?;
    println!("SELECT binds against the reopened catalog; SQL heap execution is not wired yet");
    Ok(())
}
