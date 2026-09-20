//! Inspect logical operators: cargo run --example plan -- 'SELECT SUM(id) FROM items'
mod support;

use chilidb::{
    binder::Binder,
    planner::{PlannedStatement, Planner},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "SELECT SUM(id) FROM items WHERE id > 0".into());
    let catalog = support::catalog();
    let binder = Binder::new(&catalog);
    let planner = Planner::new();
    for statement in chilidb::parser::parse_sql(&sql)? {
        match planner.plan(&binder.bind(&statement)?)? {
            PlannedStatement::Query { plan, output } => {
                println!("{}", plan.display_indent_schema());
                println!("SQL output: {:?}", output.fields);
            }
            PlannedStatement::ModifyTable { plan } => println!("{}", plan.display_indent_schema()),
            PlannedStatement::Command(command) => println!("{command:#?}"),
        }
    }
    Ok(())
}
