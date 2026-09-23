//! Run a constant query: cargo run --example execute -- 'SELECT 1 + 2 AS total'
mod support;

use chilidb::{binder::Binder, executor::Executor, planner::Planner};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "SELECT 1 + 2 AS total".into());
    let catalog = support::catalog();
    let binder = Binder::new(&catalog);
    let planner = Planner::new();
    let executor = Executor::new();
    for statement in chilidb::parser::parse_sql(&sql)? {
        let planned = planner.plan(&binder.bind(&statement)?)?;
        // Optionally apply an Optimizer with an explicit rule list here.
        let result = executor.execute(planned).await?;
        println!("SQL output: {:?}", result.output.fields);
        for batch in result.batches {
            println!("{batch:?}");
        }
    }
    Ok(())
}
