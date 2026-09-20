//! Inspect the unbound AST: cargo run --example parse -- 'SELECT 1 + 2 * 3'
use chilidb::parser::parse_sql;

fn main() {
    let sql = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "SELECT 1 + 2 * 3".to_owned());
    match parse_sql(&sql) {
        Ok(statements) => println!("{statements:#?}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
