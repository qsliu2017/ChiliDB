#![doc = include_str!("../README.md")]

#[cfg(doctest)]
#[doc = include_str!("../docs/compile-tests.md")]
mod compile_tests {}

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use std::collections::{HashMap, HashSet};
use syn::{Ident, LitStr, parse_macro_input};

#[derive(Debug)]
enum Expr {
    Ref(String),
    Lit(String, bool),
    Class(Vec<(char, char)>, bool),
    Any,
    Seq(Vec<Expr>),
    Choice(Vec<Expr>),
    Optional(Box<Expr>),
    Repeat(Box<Expr>, bool),
    Predicate(Box<Expr>, bool),
}
struct Rule {
    name: String,
    expr: Expr,
}
struct Grammar<'a> {
    text: &'a str,
    pos: usize,
    nesting: usize,
}
impl<'a> Grammar<'a> {
    fn error(&self, message: &str) -> String {
        format!("grammar byte {}: {message}", self.pos)
    }
    fn peek(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }
    fn skip(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.peek() != Some('#') {
                break;
            }
            while self.bump().is_some_and(|c| c != '\n') {}
        }
    }
    fn eat(&mut self, s: &str) -> bool {
        self.skip();
        if self.text[self.pos..].starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }
    fn name(&mut self) -> Result<String, String> {
        self.skip();
        let start = self.pos;
        if !self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        {
            return Err(self.error("expected rule name"));
        }
        self.bump();
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.bump();
        }
        Ok(self.text[start..self.pos].to_owned())
    }
    fn escaped(&mut self) -> Result<char, String> {
        let c = self
            .bump()
            .ok_or_else(|| self.error("unterminated literal or class"))?;
        if c != '\\' {
            return Ok(c);
        }
        match self.bump().ok_or_else(|| self.error("unfinished escape"))? {
            'n' => Ok('\n'),
            'r' => Ok('\r'),
            't' => Ok('\t'),
            '0' => Ok('\0'),
            c @ ('\\' | '\'' | '"' | '[' | ']' | '-' | '^') => Ok(c),
            'x' => {
                let mut value = 0;
                for _ in 0..2 {
                    value = value * 16
                        + self
                            .bump()
                            .and_then(|c| c.to_digit(16))
                            .ok_or_else(|| self.error("expected two hex digits"))?;
                }
                char::from_u32(value).ok_or_else(|| self.error("invalid character"))
            }
            'u' => {
                if self.bump() != Some('{') {
                    return Err(self.error("expected { after \\u"));
                }
                let mut value = 0u32;
                let mut digits = 0;
                while self.peek() != Some('}') {
                    let digit = self
                        .bump()
                        .and_then(|c| c.to_digit(16))
                        .ok_or_else(|| self.error("invalid Unicode escape"))?;
                    digits += 1;
                    if digits > 6 {
                        return Err(self.error("Unicode escape too long"));
                    }
                    value = value * 16 + digit;
                }
                self.bump();
                if digits == 0 {
                    return Err(self.error("empty Unicode escape"));
                }
                char::from_u32(value).ok_or_else(|| self.error("invalid Unicode scalar"))
            }
            _ => Err(self.error("unknown escape")),
        }
    }
    fn choice(&mut self) -> Result<Expr, String> {
        self.nesting += 1;
        if self.nesting > 128 {
            return Err(self.error("grammar nesting limit exceeded"));
        }
        let mut choices = vec![self.sequence()?];
        while self.eat("/") {
            choices.push(self.sequence()?);
        }
        self.nesting -= 1;
        Ok(if choices.len() == 1 {
            choices.remove(0)
        } else {
            Expr::Choice(choices)
        })
    }
    fn sequence(&mut self) -> Result<Expr, String> {
        let mut items = vec![];
        loop {
            self.skip();
            if self.peek().is_none_or(|c| matches!(c, '/' | ';' | ')')) {
                break;
            }
            items.push(self.term()?);
        }
        if items.is_empty() {
            return Err(self.error("empty sequence; use '' for epsilon"));
        }
        Ok(if items.len() == 1 {
            items.remove(0)
        } else {
            Expr::Seq(items)
        })
    }
    fn term(&mut self) -> Result<Expr, String> {
        self.skip();
        let mut prefixes = vec![];
        while matches!(self.peek(), Some('&' | '!')) {
            prefixes.push(self.bump() == Some('&'));
            self.skip();
            if prefixes.len() > 128 {
                return Err(self.error("too many predicates"));
            }
        }
        // Bound the combined group/predicate nesting, not just each local chain.
        self.nesting += prefixes.len();
        if self.nesting > 128 {
            return Err(self.error("grammar nesting limit exceeded"));
        }
        let mut e = match self.peek() {
            Some('\'' | '"') => {
                let delimiter = self.bump().unwrap();
                let mut value = String::new();
                while self.peek() != Some(delimiter) {
                    value.push(self.escaped()?);
                }
                self.bump();
                let insensitive = self.peek() == Some('i');
                if insensitive {
                    self.bump();
                }
                Expr::Lit(value, insensitive)
            }
            Some('[') => {
                self.bump();
                let negated = self.peek() == Some('^');
                if negated {
                    self.bump();
                }
                let mut ranges = vec![];
                while self.peek() != Some(']') {
                    let start = self.escaped()?;
                    let end =
                        if self.peek() == Some('-') && !self.text[self.pos..].starts_with("-]") {
                            self.bump();
                            self.escaped()?
                        } else {
                            start
                        };
                    if start > end {
                        return Err(self.error("reversed character range"));
                    }
                    ranges.push((start, end));
                }
                self.bump();
                if ranges.is_empty() {
                    return Err(self.error("empty character class"));
                }
                Expr::Class(ranges, negated)
            }
            Some('.') => {
                self.bump();
                Expr::Any
            }
            Some('(') => {
                self.bump();
                let e = self.choice()?;
                if !self.eat(")") {
                    return Err(self.error("expected )"));
                }
                e
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => Expr::Ref(self.name()?),
            _ => return Err(self.error("expected literal, class, rule, or group")),
        };
        self.skip();
        e = match self.peek() {
            Some('?') => {
                self.bump();
                Expr::Optional(Box::new(e))
            }
            Some('*') => {
                self.bump();
                Expr::Repeat(Box::new(e), false)
            }
            Some('+') => {
                self.bump();
                Expr::Repeat(Box::new(e), true)
            }
            _ => e,
        };
        self.nesting -= prefixes.len();
        for positive in prefixes.into_iter().rev() {
            e = Expr::Predicate(Box::new(e), positive);
        }
        Ok(e)
    }
    fn rules(mut self) -> Result<Vec<Rule>, String> {
        let mut rules = vec![];
        self.skip();
        while self.peek().is_some() {
            let name = self.name()?;
            if !self.eat("<-") {
                return Err(self.error("expected <-"));
            }
            let expr = self.choice()?;
            if !self.eat(";") {
                return Err(self.error("expected ; after rule"));
            }
            rules.push(Rule { name, expr });
            self.skip();
        }
        if rules.is_empty() {
            return Err(self.error("expected at least one rule"));
        }
        Ok(rules)
    }
}

fn nullable(e: &Expr, names: &HashMap<String, usize>, null: &[bool]) -> bool {
    match e {
        Expr::Ref(n) => null[names[n]],
        Expr::Lit(s, _) => s.is_empty(),
        Expr::Any | Expr::Class(..) => false,
        Expr::Seq(es) => es.iter().all(|e| nullable(e, names, null)),
        Expr::Choice(es) => es.iter().any(|e| nullable(e, names, null)),
        Expr::Optional(_) | Expr::Predicate(..) | Expr::Repeat(_, false) => true,
        Expr::Repeat(e, true) => nullable(e, names, null),
    }
}
fn walk(e: &Expr, f: &mut impl FnMut(&Expr) -> Result<(), String>) -> Result<(), String> {
    f(e)?;
    match e {
        Expr::Seq(es) | Expr::Choice(es) => {
            for e in es {
                walk(e, f)?;
            }
        }
        Expr::Optional(e) | Expr::Repeat(e, _) | Expr::Predicate(e, _) => walk(e, f)?,
        _ => (),
    }
    Ok(())
}
fn prefixes(e: &Expr, names: &HashMap<String, usize>, null: &[bool], out: &mut HashSet<usize>) {
    match e {
        Expr::Ref(n) => {
            out.insert(names[n]);
        }
        Expr::Seq(es) => {
            for e in es {
                prefixes(e, names, null, out);
                if !nullable(e, names, null) {
                    break;
                }
            }
        }
        Expr::Choice(es) => {
            for e in es {
                prefixes(e, names, null, out);
            }
        }
        Expr::Optional(e) | Expr::Repeat(e, _) | Expr::Predicate(e, _) => {
            prefixes(e, names, null, out)
        }
        _ => (),
    }
}
fn validate(rules: &[Rule]) -> Result<HashMap<String, usize>, String> {
    let mut names = HashMap::new();
    for (i, rule) in rules.iter().enumerate() {
        if names.insert(rule.name.clone(), i).is_some() {
            return Err(format!("duplicate rule {}", rule.name));
        }
    }
    for rule in rules {
        walk(&rule.expr, &mut |e| {
            if let Expr::Ref(n) = e
                && !names.contains_key(n)
            {
                return Err(format!("undefined rule {n}"));
            }
            Ok(())
        })?;
    }
    let mut null = vec![false; rules.len()];
    loop {
        let next: Vec<_> = rules
            .iter()
            .map(|r| nullable(&r.expr, &names, &null))
            .collect();
        if next == null {
            break;
        }
        null = next;
    }
    let mut edges = vec![HashSet::new(); rules.len()];
    for (i, rule) in rules.iter().enumerate() {
        walk(&rule.expr, &mut |e| {
            if let Expr::Repeat(inner, _) = e
                && nullable(inner, &names, &null)
            {
                return Err(format!("nullable repetition in rule {}", rule.name));
            }
            Ok(())
        })?;
        prefixes(&rule.expr, &names, &null, &mut edges[i]);
    }
    for (i, rule) in rules.iter().enumerate() {
        let mut seen = HashSet::new();
        let mut pending: Vec<_> = edges[i].iter().copied().collect();
        while let Some(j) = pending.pop() {
            if j == i {
                return Err(format!(
                    "nullable-prefix left recursion involving {}",
                    rule.name
                ));
            }
            if seen.insert(j) {
                pending.extend(edges[j].iter().copied());
            }
        }
    }
    Ok(names)
}

struct Generator<'a> {
    names: &'a HashMap<String, usize>,
    functions: Vec<proc_macro2::TokenStream>,
    count: usize,
}
impl Generator<'_> {
    fn expression(&mut self, e: &Expr) -> Ident {
        let name = format_ident!("expr_{}", self.count);
        self.count += 1;
        let body = match e {
            Expr::Ref(n) => {
                let f = format_ident!("rule_{}", self.names[n]);
                quote! { #f(state) }
            }
            Expr::Lit(s, insensitive) => {
                let expected = format!("{s:?}{}", if *insensitive { "i" } else { "" });
                quote! {
                    let literal: &str = #s;
                    let end = state.pos + literal.len();
                    let matched = state.input.get(state.pos..end).is_some_and(|part| if #insensitive { part.eq_ignore_ascii_case(literal) } else { part == literal });
                    if matched { state.pos = end; true } else { state.expect(#expected); false }
                }
            }
            Expr::Class(ranges, negated) => {
                let tests = ranges.iter().map(|(a, b)| match (*a, *b) {
                    ('a', 'z') => quote! { c.is_ascii_lowercase() },
                    ('A', 'Z') => quote! { c.is_ascii_uppercase() },
                    ('0', '9') => quote! { c.is_ascii_digit() },
                    _ => quote! { (#a..=#b).contains(&c) },
                });
                let expected = format!(
                    "{}class {:?}",
                    if *negated { "negated " } else { "" },
                    ranges
                );
                quote! {
                    if let Some(c) = state.input[state.pos..].chars().next() {
                        if (false #(|| #tests)*) != #negated { state.pos += c.len_utf8(); true }
                        else { state.expect(#expected); false }
                    } else { state.expect(#expected); false }
                }
            }
            Expr::Any => quote! {
                if let Some(c) = state.input[state.pos..].chars().next() { state.pos += c.len_utf8(); true }
                else { state.expect("any character"); false }
            },
            Expr::Seq(es) => {
                let fs: Vec<_> = es.iter().map(|e| self.expression(e)).collect();
                quote! { true #(&& #fs(state))* }
            }
            Expr::Choice(es) => {
                let fs: Vec<_> = es.iter().map(|e| self.expression(e)).collect();
                quote! { false #(|| #fs(state))* }
            }
            Expr::Optional(e) => {
                let f = self.expression(e);
                quote! { let _ = #f(state); !state.limited }
            }
            Expr::Repeat(e, one) => {
                let f = self.expression(e);
                quote! { let mut count = 0usize; while #f(state) { count += 1; } !state.limited && (!#one || count > 0) }
            }
            Expr::Predicate(e, positive) => {
                let f = self.expression(e);
                quote! {
                    state.suppressed_errors += 1;
                    let matched = #f(state);
                    state.suppressed_errors -= 1;
                    state.pos = start; state.nodes.truncate(mark);
                    let ok = matched == #positive && !state.limited;
                    if !ok && !state.limited { state.expect(if #positive { "positive predicate" } else { "negative predicate" }); }
                    ok
                }
            }
        };
        self.functions.push(quote! {
            fn #name(state: &mut State<'_>) -> bool {
                if state.limited { return false; }
                if state.depth >= 512 {
                    state.limited = true;
                    state.error = ParseError { offset: state.pos, expected: vec!["recursion limit (512 parser calls)".into()] };
                    return false;
                }
                if state.fuel == 0 {
                    state.limited = true;
                    state.error = ParseError { offset: state.pos, expected: vec!["work limit (1000000 expression calls)".into()] };
                    return false;
                }
                state.fuel -= 1;
                state.depth += 1;
                let start = state.pos; let mark = state.nodes.len();
                let ok = { #body };
                state.depth -= 1;
                if !ok { state.pos = start; state.nodes.truncate(mark); }
                ok
            }
        });
        name
    }
}

/// Generates a parser from a PEG grammar string literal.
///
/// Invoke once per module. The expansion defines `Node`, `ParseError`, `parse`,
/// and `parse_rule`; the first grammar rule is the default entry point.
/// See the crate documentation for grammar syntax and resource limits.
///
/// # Errors
///
/// Expansion fails for malformed grammars, duplicate or undefined rules,
/// nullable repetition, or nullable-prefix left recursion.
///
/// # Example
///
/// ```
/// mod words {
///     peg::grammar!("Root <- Word; Word <- [a-z]+;");
/// }
/// let tree = words::parse("hello").unwrap();
/// assert_eq!(tree.children()[0].text(), "hello");
/// assert!(words::parse_rule("Word", "123").is_err());
/// ```
#[proc_macro]
pub fn grammar(input: TokenStream) -> TokenStream {
    let grammar = parse_macro_input!(input as LitStr);
    let rules = match (Grammar {
        text: &grammar.value(),
        pos: 0,
        nesting: 0,
    })
    .rules()
    {
        Ok(rules) => rules,
        Err(e) => return syn::Error::new(grammar.span(), e).to_compile_error().into(),
    };
    let names = match validate(&rules) {
        Ok(names) => names,
        Err(e) => return syn::Error::new(grammar.span(), e).to_compile_error().into(),
    };
    let mut generator = Generator {
        names: &names,
        functions: vec![],
        count: 0,
    };
    let mut rule_functions = vec![];
    let mut dispatch = vec![];
    for (i, rule) in rules.iter().enumerate() {
        let f = format_ident!("rule_{i}");
        let expression = generator.expression(&rule.expr);
        let label = &rule.name;
        let silent = label.starts_with('_');
        dispatch.push(quote! { #label => #f(&mut state), });
        rule_functions.push(quote! {
            fn #f(state: &mut State<'_>) -> bool {
                if state.limited { return false; }
                if state.depth >= 512 {
                    state.limited = true;
                    state.error = ParseError { offset: state.pos, expected: vec!["recursion limit (512 parser calls)".into()] };
                    return false;
                }
                state.depth += 1;
                let start = state.pos; let mark = state.nodes.len();
                let ok = #expression(state);
                state.depth -= 1;
                if ok {
                    if #silent { state.nodes.truncate(mark); }
                    else {
                        let children = state.nodes.split_off(mark);
                        state.nodes.push(Node { rule: #label, text: &state.input[start..state.pos], span: start..state.pos, children });
                    }
                }
                ok
            }
        });
    }
    let functions = generator.functions;
    let root = &rules[0].name;
    let labels: Vec<_> = rules.iter().map(|r| &r.name).collect();
    quote! {
            /// A named parse-tree node with owned children and borrowed input text.
            ///
            /// Spans are UTF-8 byte offsets in the original input. Fields are private;
            /// accessors preserve the span and source-text invariants.
            #[derive(Clone, Debug, PartialEq, Eq)]
            pub struct Node<'sql> {
                rule: &'static str,
                text: &'sql str,
                span: ::std::ops::Range<usize>,
                children: Vec<Node<'sql>>,
            }
            impl<'sql> Node<'sql> {
                /// Returns the grammar rule name.
                pub fn rule(&self) -> &'static str { self.rule }
                /// Returns matched text, borrowed for the input lifetime.
                pub fn text(&self) -> &'sql str { self.text }
                /// Returns the matched UTF-8 byte range in the original input.
                pub fn span(&self) -> ::std::ops::Range<usize> { self.span.clone() }
                /// Returns named children in input order; silent rules are omitted.
                pub fn children(&self) -> &[Node<'sql>] { &self.children }

                /// Assemble a node with valid UTF-8 byte spans and ordered,
                /// non-overlapping children borrowed from the same input.
                /// Empty spans and gaps between children are allowed.
                ///
                /// # Errors
                ///
                /// Returns an error for an invalid span, children outside the parent
                /// or out of order, or child text whose pointer and length differ
                /// from its slice of `input`. Equal text from another allocation
                /// is not sufficient. The rule name is not grammar-validated.
                pub fn new(
                    rule: &'static str,
                    input: &'sql str,
                    span: ::std::ops::Range<usize>,
                    children: Vec<Node<'sql>>,
                ) -> Result<Self, ParseError> {
                    let invalid = |offset, expected: &'static str| ParseError {
                        offset, expected: vec![expected.into()],
                    };
                    let text = input.get(span.clone()).ok_or_else(||
                        invalid(span.start, "valid UTF-8 node span"))?;
                    let mut previous_end = span.start;
                    for child in &children {
                        if child.span.start < previous_end || child.span.end > span.end {
                            return Err(invalid(child.span.start, "contained, ordered child spans"));
                        }
                        let child_text = input.get(child.span.clone()).ok_or_else(||
                            invalid(child.span.start, "valid UTF-8 child span"))?;
                        if child.text.as_ptr() != child_text.as_ptr() || child.text.len() != child_text.len() {
                            return Err(invalid(child.span.start, "child text borrowed from the same input span"));
                        }
                        previous_end = child.span.end;
                    }
                    Ok(Self { rule, text, span, children })
                }
            }
            /// A parse, resource-limit, or node-construction failure.
            ///
            /// Errors do not borrow the input or entry-rule argument. Parsing records
            /// distinct expectations at the furthest attempted byte offset;
            /// literal mismatches point to the literal's start.
            #[derive(Clone, Debug, PartialEq, Eq)]
            pub struct ParseError {
                /// Failure byte offset, or the offending span offset for construction.
                pub offset: usize,
                /// Expected matches or failure descriptions; generated messages are static.
                pub expected: Vec<::std::borrow::Cow<'static, str>>,
            }
            impl ::std::fmt::Display for ParseError {
                fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                    write!(f, "parse error at byte {}: expected {}", self.offset, self.expected.join(", "))
                }
            }
            impl ::std::error::Error for ParseError {}
            struct State<'sql> { input: &'sql str, pos: usize, nodes: Vec<Node<'sql>>, error: ParseError, depth: usize, limited: bool, fuel: usize, suppressed_errors: usize }
            impl State<'_> {
                fn expect(&mut self, expected: &'static str) {
                    if self.limited || self.suppressed_errors > 0 || self.pos < self.error.offset { return; }
                    if self.pos > self.error.offset { self.error.offset = self.pos; self.error.expected.clear(); }
                    if !self.error.expected.iter().any(|s| s == expected) { self.error.expected.push(expected.into()); }
                }
            }
            /// Parses the first grammar rule, requiring complete input consumption.
            ///
            /// # Errors
            ///
            /// Returns an error on a mismatch, trailing input, or exhaustion of the
            /// 512 nested-call or 1,000,000 expression-call budget.
            pub fn parse(input: &str) -> Result<Node<'_>, ParseError> { parse_rule(#root, input) }
            /// Parses a named rule, requiring complete input consumption.
            /// A silent start rule returns a childless root containing all input.
            /// The returned tree borrows only `input`, not `rule`.
            ///
            /// # Errors
            ///
            /// Returns an error for an unknown rule, mismatch, trailing input, or
            /// exhaustion of the 512 nested-call or 1,000,000 expression-call budget.
            /// Resource errors are fatal, including inside optional branches and predicates.
            pub fn parse_rule<'sql>(rule: &str, input: &'sql str) -> Result<Node<'sql>, ParseError> {
                let mut state = State { input, pos: 0, nodes: vec![], error: ParseError { offset: 0, expected: vec![] }, depth: 0, limited: false, fuel: 1_000_000, suppressed_errors: 0 };
                let ok = match rule {
                    #(#dispatch)*
                    _ => return Err(ParseError { offset: 0, expected: vec![format!("known rule (unknown: {rule})").into()] }),
                };
                if !ok || state.limited { return Err(state.error); }
                if state.pos != input.len() { state.expect("end of input"); return Err(state.error); }
                if let Some(root) = state.nodes.pop() { Ok(root) }
                else {
                    let label = match rule { #(#labels => #labels,)* _ => unreachable!() };
                    Ok(Node { rule: label, text: input, span: 0..input.len(), children: vec![] })
                }
            }
            #(#rule_functions)*
            #(#functions)*
    }.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(text: &str) -> Result<(), String> {
        let rules = Grammar {
            text,
            pos: 0,
            nesting: 0,
        }
        .rules()?;
        validate(&rules).map(|_| ())
    }

    #[test]
    fn validation_diagnostics() {
        for (grammar, message) in [
            ("A <- B;", "undefined rule B"),
            ("A <- ''; A <- '';", "duplicate rule A"),
            ("A <- B*; B <- C?; C <- 'a';", "nullable repetition"),
            ("A <- (&'a')+;", "nullable repetition"),
            ("A <- &B 'x'; B <- A / 'x';", "left recursion"),
            ("A <- B? A / 'x'; B <- 'b';", "left recursion"),
            ("A <- [z-a];", "reversed character range"),
            ("A <- '\\u{d800}';", "invalid Unicode scalar"),
            ("A <- '\\q';", "unknown escape"),
            ("A <- 'a'", "expected ;"),
            ("A <- 'a' / ;", "empty sequence"),
        ] {
            let error = check(grammar).unwrap_err();
            assert!(error.contains(message), "{grammar}: {error}");
        }
        check("A <- 'a' A / ''; ").unwrap();
        check("A <- B+; B <- [a-z];").unwrap();
    }

    #[test]
    fn malformed_and_deep_grammar_return_errors() {
        for grammar in [
            "",
            "A",
            "A <- '",
            "A <- [",
            "A <- ()",
            "A <- 'a'**;",
            "A <- '\\u{';",
        ] {
            assert!(check(grammar).is_err(), "{grammar}");
        }
        let deep = format!("A <- {}'a'{};", "!(".repeat(1000), ")".repeat(1000));
        assert!(check(&deep).unwrap_err().contains("nesting limit"));
    }
}
