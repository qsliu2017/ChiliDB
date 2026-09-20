//! Fallible conversion of the named PEG tree, without reparsing SQL.
use crate::{ParseError, ParseNode, ast::*, grammar::Node};
use std::borrow::Cow;
type Result<T> = std::result::Result<T, ParseError>;
fn error(n: &Node<'_>, expected: &'static str) -> ParseError {
    ParseError {
        offset: n.span().start,
        expected: vec![Cow::Borrowed(expected)],
    }
}
struct Children<'n, 'sql> {
    node: &'n Node<'sql>,
    rest: &'n [Node<'sql>],
}
impl<'n, 'sql> Children<'n, 'sql> {
    fn new(node: &'n Node<'sql>) -> Self {
        Self {
            node,
            rest: node.children(),
        }
    }
    fn take(&mut self, rule: &'static str) -> Result<&'n Node<'sql>> {
        match self.rest.split_first() {
            Some((n, rest)) if n.rule() == rule => {
                self.rest = rest;
                Ok(n)
            }
            _ => Err(error(self.node, rule)),
        }
    }
    fn optional(&mut self, rule: &'static str) -> Result<Option<&'n Node<'sql>>> {
        if self.rest.first().is_some_and(|n| n.rule() == rule) {
            self.take(rule).map(Some)
        } else {
            Ok(None)
        }
    }
    fn finish(self) -> Result<()> {
        if self.rest.is_empty() {
            Ok(())
        } else {
            Err(error(self.node, "no extra children"))
        }
    }
}
fn only<'n, 'sql>(n: &'n Node<'sql>, rule: &'static str) -> Result<&'n Node<'sql>> {
    let mut c = Children::new(n);
    let child = c.take(rule)?;
    c.finish()?;
    Ok(child)
}
fn leaf<'s>(n: &Node<'s>) -> Result<&'s str> {
    Children::new(n).finish()?;
    Ok(n.text())
}
// Named keyword/operator spans include silent trailing trivia.
fn trivia(mut s: &str) -> bool {
    loop {
        s = s.trim_start_matches([' ', '\t', '\r', '\n']);
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.find('\n').map_or("", |i| &rest[i..]);
        } else if let Some(rest) = s.strip_prefix("/*") {
            let Some(end) = rest.find("*/") else {
                return false;
            };
            s = &rest[end + 2..];
        } else {
            return s.is_empty();
        }
    }
}
fn token(n: &Node<'_>, choices: &[&str]) -> Result<usize> {
    let s = leaf(n)?;
    choices
        .iter()
        .position(|v| {
            s.get(..v.len()).is_some_and(|p| p.eq_ignore_ascii_case(v)) && trivia(&s[v.len()..])
        })
        .ok_or_else(|| error(n, "valid leaf token"))
}
fn unquote<'s>(n: &Node<'s>, quote: char) -> Result<Cow<'s, str>> {
    let s = leaf(n)?;
    let inner = s
        .strip_prefix(quote)
        .and_then(|s| s.strip_suffix(quote))
        .ok_or_else(|| error(n, "paired SQL quotes"))?;
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == quote && chars.next() != Some(quote) {
            return Err(error(n, "doubled SQL quote"));
        }
    }
    Ok(if inner.contains(quote) {
        Cow::Owned(match quote {
            '\'' => inner.replace("''", "'"),
            '"' => inner.replace("\"\"", "\""),
            _ => return Err(error(n, "SQL quote delimiter")),
        })
    } else {
        Cow::Borrowed(inner)
    })
}
fn identifier<'s>(n: &Node<'s>) -> Result<Cow<'s, str>> {
    if n.rule() != "Identifier" {
        return Err(error(n, "Identifier"));
    }
    let c = Children::new(n);
    let [child] = c.rest else {
        return Err(error(n, "one identifier token"));
    };
    match child.rule() {
        "QuotedIdentifier" => {
            let value = unquote(child, '"')?;
            if value.is_empty() {
                return Err(error(child, "nonempty identifier"));
            }
            Ok(value)
        }
        "BareIdentifier" => {
            let s = leaf(child)?;
            if !s
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            {
                return Err(error(child, "bare identifier"));
            }
            const RESERVED: &str = "SELECT FROM WHERE CREATE TABLE INSERT INTO VALUES UPDATE SET DELETE BEGIN COMMIT ROLLBACK OR AND NOT IS NULL TRUE FALSE PRIMARY KEY UNIQUE ORDER BY LIMIT JOIN AS GROUP HAVING UNION DISTINCT";
            if RESERVED
                .split_whitespace()
                .any(|word| word.eq_ignore_ascii_case(s))
            {
                return Err(error(child, "nonreserved identifier"));
            }
            Ok(if s.bytes().any(|b| b.is_ascii_uppercase()) {
                Cow::Owned(s.to_ascii_lowercase())
            } else {
                Cow::Borrowed(s)
            })
        }
        _ => Err(error(child, "identifier token")),
    }
}
pub(crate) fn statements<'s>(root: &Node<'s>) -> Result<Vec<Statement<'s>>> {
    if root.rule() != "Root" {
        return Err(error(root, "Root"));
    }
    root.children()
        .iter()
        .map(|node| {
            if node.rule() != "Statement" {
                return Err(error(node, "Statement"));
            }
            Statement::parse(node)
        })
        .collect()
}
fn expressions<'s>(n: &Node<'s>) -> Result<Vec<Expr<'s>>> {
    let mut c = Children::new(n);
    let mut values = vec![Expr::parse(c.take("Expr")?)?];
    while !c.rest.is_empty() {
        values.push(Expr::parse(c.take("Expr")?)?);
    }
    Ok(values)
}
fn filter<'s>(c: &mut Children<'_, 's>) -> Result<Option<Expr<'s>>> {
    c.optional("Where")?
        .map(|n| Expr::parse(only(n, "Expr")?))
        .transpose()
}
impl<'sql> ParseNode<'sql> for Statement<'sql> {
    fn parse(node: &Node<'sql>) -> Result<Self> {
        let n = if node.rule() == "Statement" {
            let c = Children::new(node);
            let [n] = c.rest else {
                return Err(error(node, "one statement"));
            };
            n
        } else {
            node
        };
        let mut c = Children::new(n);
        let result = match n.rule() {
            "Select" => {
                let p = c.take("Projection")?;
                let pc = Children::new(p);
                let projection = if let [star] = pc.rest
                    && star.rule() == "Star"
                {
                    vec![Expr::parse(star)?]
                } else {
                    expressions(p)?
                };
                let from = c
                    .optional("From")?
                    .map(|f| identifier(only(f, "Identifier")?))
                    .transpose()?;
                Self::Select {
                    projection,
                    from,
                    filter: filter(&mut c)?,
                }
            }
            "CreateTable" => {
                let name = identifier(c.take("Identifier")?)?;
                let mut columns = vec![ColumnDef::parse(c.take("ColumnDef")?)?];
                while !c.rest.is_empty() {
                    columns.push(ColumnDef::parse(c.take("ColumnDef")?)?);
                }
                Self::CreateTable { name, columns }
            }
            "Insert" => {
                let table = identifier(c.take("Identifier")?)?;
                let mut columns = Vec::new();
                if let Some(n) = c.optional("Columns")? {
                    let mut cols = Children::new(n);
                    columns.push(identifier(cols.take("Identifier")?)?);
                    while !cols.rest.is_empty() {
                        columns.push(identifier(cols.take("Identifier")?)?);
                    }
                }
                let mut rows = vec![expressions(c.take("Row")?)?];
                while !c.rest.is_empty() {
                    rows.push(expressions(c.take("Row")?)?);
                }
                Self::Insert {
                    table,
                    columns,
                    rows,
                }
            }
            "Update" => {
                let table = identifier(c.take("Identifier")?)?;
                let mut assignments = vec![Assignment::parse(c.take("Assignment")?)?];
                while let Some(a) = c.optional("Assignment")? {
                    assignments.push(Assignment::parse(a)?);
                }
                Self::Update {
                    table,
                    assignments,
                    filter: filter(&mut c)?,
                }
            }
            "Delete" => Self::Delete {
                table: identifier(c.take("Identifier")?)?,
                filter: filter(&mut c)?,
            },
            "Begin" => {
                token(n, &["BEGIN"])?;
                Self::Begin
            }
            "Commit" => {
                token(n, &["COMMIT"])?;
                Self::Commit
            }
            "Rollback" => {
                token(n, &["ROLLBACK"])?;
                Self::Rollback
            }
            _ => return Err(error(n, "statement node")),
        };
        c.finish()?;
        Ok(result)
    }
}
impl<'sql> ParseNode<'sql> for ColumnDef<'sql> {
    fn parse(n: &Node<'sql>) -> Result<Self> {
        if n.rule() != "ColumnDef" {
            return Err(error(n, "ColumnDef"));
        }
        let mut c = Children::new(n);
        let name = identifier(c.take("Identifier")?)?;
        let data_type = DataType::parse(c.take("DataType")?)?;
        let mut constraints = Vec::new();
        while !c.rest.is_empty() {
            constraints.push(ColumnConstraint::parse(c.take("Constraint")?)?);
        }
        Ok(Self {
            name,
            data_type,
            constraints,
        })
    }
}
impl<'sql> ParseNode<'sql> for Assignment<'sql> {
    fn parse(n: &Node<'sql>) -> Result<Self> {
        if n.rule() != "Assignment" {
            return Err(error(n, "Assignment"));
        }
        let mut c = Children::new(n);
        let result = Self {
            column: identifier(c.take("Identifier")?)?,
            value: Expr::parse(c.take("Expr")?)?,
        };
        c.finish()?;
        Ok(result)
    }
}
impl<'sql> ParseNode<'sql> for DataType<'sql> {
    fn parse(n: &Node<'sql>) -> Result<Self> {
        if n.rule() != "DataType" {
            return Err(error(n, "DataType"));
        }
        let c = Children::new(n);
        let [ty] = c.rest else {
            return Err(error(n, "one data type"));
        };
        let (value, tokens): (Self, &[&str]) = match ty.rule() {
            "IntegerType" => (Self::Integer, &["INTEGER", "INT"]),
            "BigIntType" => (Self::BigInt, &["BIGINT"]),
            "TextType" => (Self::Text, &["TEXT"]),
            "BooleanType" => (Self::Boolean, &["BOOLEAN", "BOOL"]),
            "RealType" => (Self::Real, &["REAL"]),
            "DoubleType" => (Self::Double, &["DOUBLE"]),
            "VarcharType" => {
                let mut c = Children::new(ty);
                let length = c
                    .optional("Length")?
                    .map(|n| {
                        let s = leaf(n)?;
                        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                            return Err(error(n, "decimal length"));
                        }
                        Ok(s)
                    })
                    .transpose()?;
                c.finish()?;
                if length.is_none() {
                    token(ty, &["VARCHAR"])?;
                }
                return Ok(Self::Varchar(length));
            }
            _ => return Err(error(ty, "data type")),
        };
        token(ty, tokens)?;
        Ok(value)
    }
}
impl<'sql> ParseNode<'sql> for ColumnConstraint {
    fn parse(n: &Node<'sql>) -> Result<Self> {
        if n.rule() != "Constraint" {
            return Err(error(n, "Constraint"));
        }
        let c = Children::new(n);
        let [n] = c.rest else {
            return Err(error(n, "one constraint"));
        };
        let (result, words): (Self, &[&str]) = match n.rule() {
            "NotNull" => (Self::NotNull, &["NOT", "NULL"]),
            "PrimaryKey" => (Self::PrimaryKey, &["PRIMARY", "KEY"]),
            "Unique" => (Self::Unique, &["UNIQUE"]),
            _ => return Err(error(n, "column constraint")),
        };
        leaf(n)?;
        keyword_sequence(n, words)?;
        Ok(result)
    }
}
fn keyword_sequence(n: &Node<'_>, words: &[&str]) -> Result<()> {
    let mut s = n.text();
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            // Locate the next keyword after trivia, including comments.
            loop {
                s = s.trim_start_matches([' ', '\t', '\r', '\n']);
                if let Some(rest) = s.strip_prefix("/*") {
                    let end = rest.find("*/").ok_or_else(|| error(n, "closed comment"))?;
                    s = &rest[end + 2..];
                } else if let Some(rest) = s.strip_prefix("--") {
                    s = rest.find('\n').map_or("", |i| &rest[i..]);
                } else {
                    break;
                }
            }
        }
        if !s
            .get(..word.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(word))
        {
            return Err(error(n, "keyword sequence"));
        }
        s = &s[word.len()..];
        if s.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(error(n, "keyword boundary"));
        }
    }
    if trivia(s) {
        Ok(())
    } else {
        Err(error(n, "trailing trivia"))
    }
}
impl<'sql> ParseNode<'sql> for Literal<'sql> {
    fn parse(n: &Node<'sql>) -> Result<Self> {
        Ok(match n.rule() {
            "String" => Self::String(unquote(n, '\'')?),
            "Number" => {
                let s = leaf(n)?;
                if !number(s) {
                    return Err(error(n, "numeric literal"));
                }
                Self::Number(s)
            }
            "Null" => {
                token(n, &["NULL"])?;
                Self::Null
            }
            "True" => {
                token(n, &["TRUE"])?;
                Self::Boolean(true)
            }
            "False" => {
                token(n, &["FALSE"])?;
                Self::Boolean(false)
            }
            _ => return Err(error(n, "literal node")),
        })
    }
}
fn number(s: &str) -> bool {
    let mut b = s.as_bytes();
    let digits = |b: &mut &[u8]| {
        let count = b.iter().take_while(|v| v.is_ascii_digit()).count();
        *b = &b[count..];
        count
    };
    let whole = digits(&mut b);
    if b.first() == Some(&b'.') {
        b = &b[1..];
        if digits(&mut b) == 0 {
            return false;
        }
    } else if whole == 0 {
        return false;
    }
    if matches!(b.first(), Some(b'e' | b'E')) {
        b = &b[1..];
        if matches!(b.first(), Some(b'+' | b'-')) {
            b = &b[1..];
        }
        if digits(&mut b) == 0 {
            return false;
        }
    }
    b.is_empty()
}
impl<'sql> ParseNode<'sql> for Expr<'sql> {
    fn parse(n: &Node<'sql>) -> Result<Self> {
        let mut c = Children::new(n);
        let result = match n.rule() {
            "Expr" => Self::parse(c.take("Or")?)?,
            "Primary" => {
                let [child] = c.rest else {
                    return Err(error(n, "one primary expression"));
                };
                if !matches!(
                    child.rule(),
                    "Number" | "String" | "Null" | "True" | "False" | "Name" | "Expr"
                ) {
                    return Err(error(child, "primary expression"));
                }
                c.rest = &[];
                Self::parse(child)?
            }
            "Or" | "And" | "Additive" | "Multiplicative" => {
                let (operand, operator) = match n.rule() {
                    "Or" => ("And", "OrOp"),
                    "And" => ("Not", "AndOp"),
                    "Additive" => ("Multiplicative", "AddOp"),
                    _ => ("Unary", "MulOp"),
                };
                let mut left = Self::parse(c.take(operand)?)?;
                while !c.rest.is_empty() {
                    let op = binary_op(c.take(operator)?)?;
                    let right = Self::parse(c.take(operand)?)?;
                    left = Self::Binary {
                        left: Box::new(left),
                        op,
                        right: Box::new(right),
                    };
                }
                left
            }
            "Not" | "Unary" => {
                let (operator, operand) = if n.rule() == "Not" {
                    ("NotOp", "Comparison")
                } else {
                    ("UnaryOp", "Primary")
                };
                if let Some(op) = c.optional(operator)? {
                    let op = if operator == "NotOp" {
                        token(op, &["NOT"])?;
                        UnaryOp::Not
                    } else if token(op, &["+", "-"])? == 0 {
                        UnaryOp::Plus
                    } else {
                        UnaryOp::Minus
                    };
                    Self::Unary {
                        op,
                        expr: Box::new(Self::parse(c.take(n.rule())?)?),
                    }
                } else {
                    Self::parse(c.take(operand)?)?
                }
            }
            "Comparison" => {
                let left = Self::parse(c.take("Additive")?)?;
                if let Some(is) = c.optional("IsNull")? {
                    let mut parts = Children::new(is);
                    let negated = if let Some(not) = parts.optional("IsNot")? {
                        token(not, &["NOT"])?;
                        true
                    } else {
                        false
                    };
                    parts.finish()?;
                    keyword_sequence(
                        is,
                        if negated {
                            &["IS", "NOT", "NULL"]
                        } else {
                            &["IS", "NULL"]
                        },
                    )?;
                    Self::IsNull {
                        expr: Box::new(left),
                        negated,
                    }
                } else if let Some(op) = c.optional("CompareOp")? {
                    Self::Binary {
                        left: Box::new(left),
                        op: binary_op(op)?,
                        right: Box::new(Self::parse(c.take("Additive")?)?),
                    }
                } else {
                    left
                }
            }
            "Name" => {
                let first = identifier(c.take("Identifier")?)?;
                if let Some(second) = c.optional("Identifier")? {
                    Self::QualifiedIdentifier {
                        table: first,
                        column: identifier(second)?,
                    }
                } else {
                    Self::Identifier(first)
                }
            }
            "Star" => {
                token(n, &["*"])?;
                Self::Wildcard
            }
            "Number" | "String" | "Null" | "True" | "False" => Self::Literal(Literal::parse(n)?),
            _ => return Err(error(n, "expression node")),
        };
        c.finish()?;
        Ok(result)
    }
}
fn binary_op(n: &Node<'_>) -> Result<BinaryOp> {
    let (tokens, ops): (&[&str], &[BinaryOp]) = match n.rule() {
        "OrOp" => (&["OR"], &[BinaryOp::Or]),
        "AndOp" => (&["AND"], &[BinaryOp::And]),
        "AddOp" => (&["+", "-"], &[BinaryOp::Add, BinaryOp::Subtract]),
        "MulOp" => (
            &["*", "/", "%"],
            &[BinaryOp::Multiply, BinaryOp::Divide, BinaryOp::Modulo],
        ),
        "CompareOp" => (
            &["<=", ">=", "<>", "!=", "=", "<", ">"],
            &[
                BinaryOp::LessEq,
                BinaryOp::GreaterEq,
                BinaryOp::NotEq,
                BinaryOp::NotEq,
                BinaryOp::Eq,
                BinaryOp::Less,
                BinaryOp::Greater,
            ],
        ),
        _ => return Err(error(n, "binary operator")),
    };
    Ok(ops[token(n, tokens)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node<'s>(rule: &'static str, sql: &'s str) -> Node<'s> {
        Node::new(rule, sql, 0..sql.len(), vec![]).unwrap()
    }

    // Rebuild checked nodes to change grammar shape without violating source invariants.
    fn rewrite<'s>(
        n: &Node<'s>,
        sql: &'s str,
        rule: &str,
        change: &impl Fn(&Node<'s>, Vec<Node<'s>>) -> Node<'s>,
    ) -> Node<'s> {
        let children = n
            .children()
            .iter()
            .map(|n| rewrite(n, sql, rule, change))
            .collect();
        if n.rule() == rule {
            change(n, children)
        } else {
            Node::new(n.rule(), sql, n.span(), children).unwrap()
        }
    }

    #[test]
    fn ast_outlives_parse_tree() {
        let input = String::from("select item, '世界', 123");
        let ast = {
            let tree = crate::grammar::parse(&input).unwrap();
            let wrapper = &tree.children()[0];
            let ast = Statement::parse(wrapper).unwrap();
            assert_eq!(ast, Statement::parse(&wrapper.children()[0]).unwrap());
            drop(tree);
            ast
        };
        let Statement::Select { projection, .. } = ast else {
            panic!("expected SELECT")
        };
        assert!(matches!(
            &projection[0],
            Expr::Identifier(Cow::Borrowed("item"))
        ));
        assert!(matches!(
            &projection[1],
            Expr::Literal(Literal::String(Cow::Borrowed("世界")))
        ));
        let Expr::Literal(Literal::Number(number)) = &projection[2] else {
            panic!("expected number")
        };
        assert_eq!(
            number.as_ptr(),
            input[input.find("123").unwrap()..].as_ptr()
        );

        let sql = String::from("001");
        let n = node("Number", &sql);
        let expr = Expr::parse(&n).unwrap();
        drop(n);
        assert!(matches!(expr, Expr::Literal(Literal::Number("001"))));
    }

    #[test]
    fn wrong_kinds_and_missing_children_are_errors() {
        assert!(Statement::parse(&node("Number", "1")).is_err());
        assert!(Expr::parse(&node("Begin", "BEGIN")).is_err());
        for rule in [
            "Statement",
            "Select",
            "CreateTable",
            "Insert",
            "Update",
            "Delete",
        ] {
            assert!(Statement::parse(&node(rule, "")).is_err(), "{rule}");
        }
        for rule in [
            "Expr",
            "Primary",
            "Or",
            "And",
            "Not",
            "Comparison",
            "Additive",
            "Multiplicative",
            "Unary",
            "Name",
        ] {
            assert!(Expr::parse(&node(rule, "")).is_err(), "{rule}");
        }
        assert!(ColumnDef::parse(&node("ColumnDef", "")).is_err());
        assert!(Assignment::parse(&node("Assignment", "")).is_err());
        assert!(DataType::parse(&node("DataType", "")).is_err());
        assert!(ColumnConstraint::parse(&node("Constraint", "")).is_err());
    }

    #[test]
    fn unexpected_fields_and_incomplete_chains_are_errors() {
        for (sql, rule) in [
            ("SELECT a + 2", "Select"),
            ("UPDATE t SET x = 1", "Update"),
            ("CREATE TABLE t (x INT)", "CreateTable"),
            ("INSERT INTO t VALUES (1)", "Insert"),
            ("DELETE FROM t", "Delete"),
            ("BEGIN", "Begin"),
            ("SELECT a IS NULL", "IsNull"),
        ] {
            let tree = crate::grammar::parse(sql).unwrap();
            let tree = rewrite(&tree, sql, rule, &|n, mut children| {
                let end = n.span().end;
                children.push(Node::new("Unexpected", sql, end..end, vec![]).unwrap());
                Node::new(n.rule(), sql, n.span(), children).unwrap()
            });
            assert!(statements(&tree).is_err(), "{sql}");
        }
        let sql = "SELECT 1 + 2";
        let tree = crate::grammar::parse(sql).unwrap();
        let incomplete = rewrite(&tree, sql, "Additive", &|n, mut children| {
            children.pop();
            Node::new(n.rule(), sql, n.span(), children).unwrap()
        });
        assert!(statements(&incomplete).is_err());
        let wrong_operator = rewrite(&tree, sql, "AddOp", &|n, children| {
            Node::new("MulOp", sql, n.span(), children).unwrap()
        });
        assert!(statements(&wrong_operator).is_err());
    }

    #[test]
    fn bad_spans_and_leaf_tokens_are_errors() {
        let reversed = std::ops::Range { start: 2, end: 1 };
        for span in [reversed, 0..100, 1..2, usize::MAX..usize::MAX] {
            assert!(Node::new("String", "世界", span, vec![]).is_err());
        }
        for s in ["", "'", "'abc", "abc'", "'a'b'", "'a'''b'"] {
            assert!(Literal::parse(&node("String", s)).is_err(), "{s}");
        }
        for s in ["", "1e", "1.", ".", "-1", "1foo", "1e+"] {
            assert!(Literal::parse(&node("Number", s)).is_err(), "{s}");
        }
        for s in ["", "++", "foo", "+junk", "+/*"] {
            assert!(binary_op(&node("AddOp", s)).is_err());
        }
        for s in ["\"", "\"\"", "\"a\"b\"", "\"abc"] {
            let n = Node::new(
                "Identifier",
                s,
                0..s.len(),
                vec![node("QuotedIdentifier", s)],
            )
            .unwrap();
            assert!(identifier(&n).is_err());
        }
        let n = Node::new(
            "Null",
            "NULL",
            0..4,
            vec![Node::new("Unexpected", "NULL", 0..0, vec![]).unwrap()],
        )
        .unwrap();
        assert!(Literal::parse(&n).is_err());
    }
}
