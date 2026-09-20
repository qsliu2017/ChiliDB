//! Inspect a typed query: cargo run --example bind -- 'SELECT id + 1 FROM items'
mod support;

use chilidb::binder::Binder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "SELECT id + 1 FROM items WHERE id > 0".into());
    let catalog = support::catalog();
    let binder = Binder::new(&catalog);
    for statement in chilidb::parser::parse_sql(&sql)? {
        println!("{:#?}", binder.bind(&statement)?);
    }
    Ok(())
}
