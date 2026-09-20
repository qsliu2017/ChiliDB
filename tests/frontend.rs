//! Cross-crate contract: the SQL frontend works without any other DB layer.
use chilidb::parser::{ParseNode, Statement, parse_sql};

#[test]
fn oltp_script_needs_no_catalog() {
    let sql = "
        CREATE TABLE accounts (id INTEGER PRIMARY KEY, balance BIGINT NOT NULL);
        BEGIN;
        INSERT INTO accounts (id, balance) VALUES (1, 100), (2, 200);
        UPDATE accounts SET balance = balance - 10 WHERE id = 1;
        UPDATE accounts SET balance = balance + 10 WHERE id = 2;
        SELECT id, balance FROM accounts WHERE balance >= 0;
        DELETE FROM accounts WHERE id = 2;
        COMMIT;
    ";
    let statements = parse_sql(sql).expect("syntactically valid OLTP script");
    assert_eq!(statements.len(), 8);
    assert!(matches!(statements[1], Statement::Begin));
    assert!(matches!(statements[7], Statement::Commit));
}

#[test]
fn statement_conversion_is_available_through_the_public_api() {
    let sql = String::from("SELECT account FROM accounts");
    let statement = {
        let tree = chilidb::parser::grammar::parse(&sql).unwrap();
        Statement::parse(&tree.children()[0]).unwrap()
    };
    assert_eq!(vec![statement], parse_sql(&sql).unwrap());
}

#[test]
fn parsed_statement_outlives_its_node() {
    let sql = String::from("SELECT account FROM accounts");
    let tree = chilidb::parser::grammar::parse_rule("Statement", &sql).unwrap();
    let statement = Statement::parse(&tree).unwrap();
    drop(tree);
    // The AST borrows `sql`, not the dropped tree.
    let Statement::Select { projection, .. } = &statement else {
        panic!("expected SELECT");
    };
    let chilidb::parser::Expr::Identifier(std::borrow::Cow::Borrowed(name)) = &projection[0] else {
        panic!("expected borrowed identifier");
    };
    assert_eq!(name.as_ptr(), sql[7..].as_ptr());
    assert_eq!(vec![statement], parse_sql(&sql).unwrap());
}

#[test]
fn syntax_does_not_bind_names_or_check_types() {
    assert!(parse_sql("SELECT missing_column + 'text' FROM missing_table").is_ok());
}

#[test]
fn invalid_suffix_does_not_return_a_partial_script() {
    assert!(parse_sql("BEGIN; SELECT 1; this is not sql").is_err());
}

#[test]
fn comments_can_follow_operators_and_keywords() {
    for sql in [
        "SELECT 1 + /* plus */ 2",
        "SELECT - /* negative */ 2",
        "SELECT 4 >= -- comparison\n 2",
        "SELECT true AND /* boolean */ NOT false",
        "CREATE TABLE t (x INTEGER /* type */ NOT NULL)",
    ] {
        assert!(parse_sql(sql).is_ok(), "{sql}");
    }
}

#[test]
fn ordinary_expression_nesting_is_supported() {
    let sql = format!("SELECT {}1{}", "(".repeat(8), ")".repeat(8));
    assert!(parse_sql(&sql).is_ok(), "{sql}");
}

#[test]
fn arbitrary_short_inputs_never_panic() {
    // Fixed seed, no dependency or wall-clock performance threshold. This is a
    // small robustness smoke test, not a replacement for fuzzing the grammar.
    let alphabet = [
        "a", "'", "\"", "(", ")", ";", "-", "/", "*", "\n", "λ", "\0", "1",
    ];
    let mut seed = 42_u64;
    for len in 0..48 {
        for _ in 0..12 {
            let mut input = String::new();
            for _ in 0..len {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                input.push_str(alphabet[(seed >> 32) as usize % alphabet.len()]);
            }
            let _ = parse_sql(&input);
        }
    }
}
