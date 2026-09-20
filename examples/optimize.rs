//! Inspect rewrites: cargo run --example optimize -- 'INSERT INTO items VALUES (1 + 2)'
mod support;

use std::sync::Arc;

use chilidb::{
    binder::Binder,
    optimizer::Optimizer,
    planner::{PlannedStatement, Planner},
};
use datafusion_optimizer::{
    eliminate_filter::EliminateFilter, optimize_projections::OptimizeProjections,
    simplify_expressions::SimplifyExpressions,
};

fn display(statement: &PlannedStatement<'_>) {
    match statement {
        PlannedStatement::Query { plan, output } => {
            println!("{}", plan.display_indent_schema());
            println!("SQL output: {:?}", output.fields);
        }
        PlannedStatement::ModifyTable { plan } => println!("{}", plan.display_indent_schema()),
        PlannedStatement::Command(command) => println!("{command:#?}"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "SELECT id + (1 + 2) AS total FROM items WHERE TRUE".into());
    let catalog = support::catalog();
    let binder = Binder::new(&catalog);
    let planner = Planner::new();
    let optimizer = Optimizer::new(vec![
        Arc::new(SimplifyExpressions::new()),
        Arc::new(EliminateFilter::new()),
        Arc::new(OptimizeProjections::new()),
    ]);
    for statement in chilidb::parser::parse_sql(&sql)? {
        let planned = planner.plan(&binder.bind(&statement)?)?;
        println!("Before:");
        display(&planned);
        let optimized = optimizer.optimize(planned)?;
        println!("After:");
        display(&optimized);
    }
    Ok(())
}
