pub mod basic {
    peg::grammar!(
        r#"
    Root <- _Space ('SELECT'i _Space)? Word (_Space ',' _Space Word)* _Space !.;
    Word <- [a-zA-Z_é]+;
    _Space <- ([ \t\n] / Hidden)*;
    Hidden <- '#' [^\n]* '\n';
"#
    );
}
mod rollback {
    peg::grammar!("Root <- (A 'x' / B) !.; A <- 'a'; B <- 'a';");
}
mod predicates {
    peg::grammar!("Root <- &A !B A; A <- 'é'; B <- 'x';");
}
mod unicode {
    peg::grammar!(r#"Root <- Symbol .; Symbol <- '\u{1f600}';"#);
}
mod recursion {
    peg::grammar!("Root <- '(' Root ')' / ''; ");
}
mod errors {
    peg::grammar!("Root <- 'a' ('b' / 'c');");
}
mod greedy {
    peg::grammar!("Root <- 'a'* 'a';");
}
mod escaped {
    peg::grammar!(r#"Root <- '\'' '\\' '\x41' [\]\-] [^a-c];"#);
}
mod isolation {
    peg::grammar!("Root <- !('abc' 'z') 'a' 'x';");
}
mod choices {
    peg::grammar!("Root <- ('a' / 'ab') 'c';");
}

#[test]
fn tree_and_silent_rules() {
    let input = " # comment\n SeLeCt one, café ";
    let tree = basic::parse(input).unwrap();
    assert_eq!(tree.span(), 0..input.len());
    assert_eq!(tree.children().len(), 2);
    assert_eq!(&input[tree.children()[1].span()], "café");
    assert!(tree.children().iter().all(|n| n.rule() == "Word"));
    assert!(
        basic::parse_rule("_Space", " #hello\n ")
            .unwrap()
            .children()
            .is_empty()
    );
    assert_eq!(basic::parse_rule("_Space", " ").unwrap().rule(), "_Space");
    assert!(basic::parse_rule("Unknown", "").is_err());
}
#[test]
fn rollback_and_predicate_isolation() {
    let tree = rollback::parse("a").unwrap();
    assert_eq!(tree.children().len(), 1);
    assert_eq!(tree.children()[0].rule(), "B");
    let tree = predicates::parse("é").unwrap();
    assert_eq!(tree.children().len(), 1);
    assert_eq!(tree.children()[0].span(), 0..2);
    let error = isolation::parse("abcq").unwrap_err();
    assert_eq!(error.offset, 1);
    assert_eq!(error.expected, vec!["\"x\""]);
}
#[test]
fn unicode_and_escapes() {
    let tree = unicode::parse("😀é").unwrap();
    assert_eq!(tree.span(), 0..6);
    assert_eq!(tree.children()[0].span(), 0..4);
    assert!(unicode::parse("😀").is_err());
    escaped::parse("'\\A]z").unwrap();
    escaped::parse("'\\A-z").unwrap();
}
#[test]
fn furthest_expectations_and_complete_consumption() {
    let error = errors::parse("ax").unwrap_err();
    assert_eq!(error.offset, 1);
    assert_eq!(error.expected, vec!["\"b\"", "\"c\""]);
    assert_eq!(errors::parse("ab!").unwrap_err().offset, 2);
    assert!(greedy::parse("aa").is_err());
    assert!(choices::parse("abc").is_err());
}
#[test]
fn recursion_is_bounded_without_overflow() {
    recursion::parse("((()))").unwrap();
    let error = recursion::parse(&"(".repeat(10_000)).unwrap_err();
    assert!(error.expected[0].contains("recursion limit"));
}

mod budget {
    peg::grammar!(
        "
    Choice <- A 'z' / .*;
    Optional <- A? .*;
    Predicate <- !A .* / .*;
    Repetition <- ('a' A)* .*;
    A <- 'a' A 'x' / 'a' A 'y' / '';
"
    );
}

#[test]
fn exponential_backtracking_has_a_fatal_work_budget() {
    // Without a work budget A explores both recursive alternatives at each
    // input position. The suffix cannot match, so this takes exponential work.
    // Every entry point could succeed via a fallback if exhaustion were hidden.
    let input = format!("{}z", "a".repeat(24));
    for rule in ["Choice", "Optional", "Predicate", "Repetition"] {
        let error = budget::parse_rule(rule, &input).unwrap_err();
        assert_eq!(
            error.expected,
            ["work limit (1000000 expression calls)"],
            "{rule}: {error}"
        );
    }
    // Ordinary errors must retain their normal diagnostics.
    assert_eq!(
        errors::parse("ax").unwrap_err().expected,
        ["\"b\"", "\"c\""]
    );
}

#[test]
fn diagnostics_borrow_static_expectations() {
    use std::borrow::Cow;

    let error = errors::parse("ax").unwrap_err();
    assert!(
        error
            .expected
            .iter()
            .all(|item| matches!(item, Cow::Borrowed(_)))
    );
    // Dynamic errors own their message; they must not borrow the rule argument.
    let error = {
        let rule = String::from("missing");
        errors::parse_rule(&rule, "").unwrap_err()
    };
    assert!(matches!(&error.expected[0], Cow::Owned(_)));
    assert!(error.to_string().contains("missing"));
}

mod nested_predicates {
    peg::grammar!("Root <- &(!'z' 'a') 'a' 'b';");
}

#[test]
fn nested_predicates_do_not_leak_expectations() {
    let error = nested_predicates::parse("ax").unwrap_err();
    assert_eq!(error.offset, 1);
    assert_eq!(error.expected, ["\"b\""]);
}

#[test]
fn useful_recursion_depth_is_supported() {
    let input = format!("{}{}", "(".repeat(32), ")".repeat(32));
    recursion::parse(&input).unwrap();
}

#[test]
fn nodes_borrow_source_and_not_entry_rule_or_tree() {
    let input = String::from("one, café");
    let text = {
        let rule = String::from("Root");
        let tree = basic::parse_rule(&rule, &input).unwrap();
        fn check(node: &basic::Node<'_>, input: &str) {
            let source = &input[node.span()];
            assert_eq!(node.text(), source);
            assert_eq!(node.text().as_ptr(), source.as_ptr());
            for child in node.children() {
                check(child, input);
            }
        }
        check(&tree, &input);
        tree.children()[1].text()
    };
    assert_eq!(text, "café");
    let silent = basic::parse_rule("_Space", " \n").unwrap();
    assert_eq!(silent.text(), " \n");
}

#[test]
fn checked_node_construction_validates_spans_and_source_identity() {
    use basic::Node;
    let input = String::from("éabc");
    let child = || Node::new("Word", &input, 2..3, vec![]).unwrap();
    let tree = Node::new("Root", &input, 0..5, vec![child()]).unwrap();
    assert_eq!(tree.rule(), "Root");
    assert_eq!(tree.text().as_ptr(), input.as_ptr());
    assert_eq!(tree.children()[0].text(), "a");
    assert_eq!(tree.clone(), tree);
    for span in [
        1..2,
        0..6,
        std::ops::Range { start: 4, end: 3 },
        usize::MAX..usize::MAX,
    ] {
        let error = Node::new("Root", &input, span, vec![]).unwrap_err();
        assert!(
            error
                .expected
                .iter()
                .all(|e| matches!(e, std::borrow::Cow::Borrowed(_)))
        );
    }
    assert!(Node::new("Root", &input, 3..5, vec![child()]).is_err());
    assert!(Node::new("Root", &input, 0..2, vec![child()]).is_err());
    assert!(Node::new("Root", &input, 0..5, vec![child(), child()]).is_err());
    let later = Node::new("Word", &input, 3..4, vec![]).unwrap();
    assert!(Node::new("Root", &input, 0..5, vec![later, child()]).is_err());
    let other = input.clone();
    let foreign = Node::new("Word", &other, 2..3, vec![]).unwrap();
    assert!(Node::new("Root", &input, 0..5, vec![foreign]).is_err());
    // Equal text at a different source offset must also be rejected.
    let repeated = String::from("aa");
    let shifted = Node::new("Word", &repeated[1..], 0..1, vec![]).unwrap();
    assert!(Node::new("Root", &repeated, 0..2, vec![shifted]).is_err());
    let empty = Node::new("Empty", &input, 3..3, vec![]).unwrap();
    Node::new("Root", &input, 2..3, vec![child(), empty]).unwrap();
    Node::new("Empty", &input, 5..5, vec![]).unwrap();
}

#[test]
fn errors_outlive_sql_input() {
    let error = {
        let input = String::from("ax");
        errors::parse(&input).unwrap_err()
    };
    assert_eq!(error.offset, 1);
    let error = {
        let input = String::from("é");
        basic::Node::new("Root", &input, 0..1, vec![]).unwrap_err()
    };
    assert_eq!(error.expected, ["valid UTF-8 node span"]);
}
