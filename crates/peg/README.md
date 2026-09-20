# chilidb-peg

A PEG procedural macro with no runtime dependencies. Grammar parsing and
validation happen at compile time; expansion produces Rust matching functions.

```rust
pub mod grammar {
peg::grammar!(r#"
    Root <- _Space Word (_Space ',' _Space Word)* _Space;
    Word <- [a-zA-Z_]+;
    _Space <- [ \t\r\n]*;
"#);
}
let tree = grammar::parse("one, two").unwrap();
assert_eq!(tree.rule(), "Root");
assert_eq!(tree.children().len(), 2);
```

## Dependency

The package is named `chilidb-peg`; its Rust library is named `peg`.
Workspace dependency:
```toml
[dependencies]
peg = { package = "chilidb-peg", path = "../peg" }
```

## Expansion examples

Place the macro in a Rust module of your choice. It emits public `Node`,
`ParseError`, `parse`, and `parse_rule` definitions directly into that module,
plus private state and matching functions. Invoke it once per module; multiple
invocations in the same module would define conflicting items. Module visibility
is ordinary Rust (`mod`, `pub mod`, or `pub(crate) mod`). There is one
`rule_N` function per rule and one `expr_N` function per expression.

### A literal rule

```rust
pub mod greeting {
    peg::grammar!("Root <- 'hello';");
}
let tree = greeting::parse("hello").unwrap();
assert_eq!(tree.rule(), "Root");
assert_eq!(tree.span(), 0..5);
assert_eq!(tree.text(), "hello");
assert!(tree.children().is_empty()); // Terminals do not create named nodes.
```

Abridged expansion:
```rust,ignore
fn rule_0(state: &mut State<'_>) -> bool {
    let start = state.pos;
    let mark = state.nodes.len();
    let ok = expr_0(state);
    if ok {
        let children = state.nodes.split_off(mark);
        state.nodes.push(Node {
            rule: "Root",
            text: &state.input[start..state.pos],
            span: start..state.pos,
            children,
        });
    }
    ok
}

fn expr_0(state: &mut State<'_>) -> bool {
    let literal = "hello";
    let end = state.pos + literal.len();
    if state.input.get(state.pos..end) == Some(literal) {
        state.pos = end;
        true
    } else {
        state.expect("\"hello\"");
        false
    }
}
```

This omits resource accounting, checkpoint wrappers, and the literal's constant
case-sensitivity branch. Helper names are private implementation details.
`parse` calls `parse_rule("Root", input)`, which initializes state, calls
`rule_0`, and requires end of input.

### Sequence and ordered choice

```rust
pub mod choice {
peg::grammar!(r#"
    Root <- ('a' 'x') / ('a' 'b');
"#);
}
assert!(choice::parse("ab").is_ok());
```

The generated expression bodies are equivalent to these short-circuit calls
(shown separately from their checkpoint wrappers):

```rust,ignore
// expr_0: ordered choice
expr_1(state) || expr_4(state)

// expr_1: first sequence, matching 'a' then 'x'
expr_2(state) && expr_3(state)

// expr_4: second sequence, matching 'a' then 'b'
expr_5(state) && expr_6(state)
```

Every expression function wraps its body with a checkpoint:

```rust,ignore
let start = state.pos;
let mark = state.nodes.len();
let ok = /* generated expression body */;
if !ok {
    state.pos = start;
    state.nodes.truncate(mark);
}
ok
```

On `ab`, the first sequence consumes `a`, fails on `x`, then restores the
position and tree before the second sequence runs. Diagnostics retain the
furthest failure; predicates additionally isolate speculative diagnostics.
A successful choice is not retried if a later expression fails.

Inspect expansions with `cargo expand -p chilidb-peg --test parsing`
(requires `cargo-expand`).

## Grammar

`Name <- expression;` defines a rule. The first rule is the entry point.
Names are ASCII identifiers. References may be forward references. Semicolons
are mandatory. Grammar whitespace and `#` through end-of-line comments are
ignored outside literals/classes; **input whitespace is never implicitly skipped**.

From tightest to loosest: atoms, postfix `? * +`, prefix `& !`, sequence,
ordered choice `/`. Atoms are rule references, groups `(expression)`, dot
(one Unicode scalar), quoted strings, or character classes. Both quote styles
are accepted. The immediately adjacent suffix `i` means ASCII-insensitive
comparison (`'select'i`), not Unicode case folding. Strings can be empty (`''`).
Empty alternatives/groups are rejected; use `''` explicitly.

Classes support inclusive ranges, individual Unicode scalars and leading `^`
negation: `[a-zA-Z_]`, `[^'\n]`. A trailing hyphen is literal; escape a hyphen
elsewhere when necessary. Supported escapes in strings/classes are `\\`, `\'`,
`\"`, `\n`, `\r`, `\t`, `\0`, `\[`, `\]`, `\-`, `\^`, `\xHH`, and
`\u{H...}` (1–6 hex digits denoting a Unicode scalar). `\xHH` denotes a
Unicode scalar, not a raw byte. No Rust semantic actions or captures are supported.

## Generated API and trees

The module exposes these entry points, both requiring full consumption:

```rust,ignore
pub fn parse(input: &str) -> Result<Node<'_>, ParseError>;
pub fn parse_rule<'sql>(rule: &str, input: &'sql str) -> Result<Node<'sql>, ParseError>;
```

`Node<'sql>` derives `Clone, Debug, PartialEq, Eq` and owns its tree structure,
but borrows matched text from the input. Its fields are private. Read-only
accessors are `rule() -> &'static str`, `text() -> &'sql str`,
`span() -> Range<usize>` (a cheap clone), and `children() -> &[Node<'sql>]`.
Spans are UTF-8 byte offsets on character boundaries. Named rules create nodes;
terminals do not. Groups, sequences, and repetitions flatten into the parent.
Rules beginning with `_` discard their entire descendant tree. When explicitly
used as an entry point, a silent rule returns a childless node bearing
that rule's name and borrowing all matched input as a result envelope.
The tree cannot outlive the input; text returned by `text()` can outlive the tree.
The entry rule argument does not constrain the tree's lifetime.

For tests and manual tree assembly, the public checked constructor is:

```rust,ignore
pub fn new(
    rule: &'static str,
    input: &'sql str,
    span: std::ops::Range<usize>,
    children: Vec<Node<'sql>>,
) -> Result<Node<'sql>, ParseError>; // Associated function on Node<'sql>.
```

`Node::new` checks the span with `input.get`, requires child spans to be
contained, ordered and non-overlapping, and checks that each child's text has
the same pointer and length as its input span. Equal text from another allocation
is not sufficient. Empty spans and gaps between children are allowed. Invalid
construction returns a normal error with static expectations, without panicking.

`ParseError` has public `offset: usize` and
`expected: Vec<Cow<'static, str>>` fields, derives `Clone, Debug, PartialEq, Eq`,
and implements `Display` and `Error`. Errors do not borrow SQL input or entry
rule arguments and can outlive both. Generated expectations borrow static
strings; only dynamic messages such as unknown entry rule names allocate text.
Diagnostics collect distinct expectations at the furthest attempted byte offset
(including failed alternatives and repetition termination). Literal mismatch is
reported at the literal's starting offset, not its first differing character.
Failed predicates report a predicate expectation. Unknown entry rules return a
normal parse error.

## Rules and limits

Rules are fixed at compile time: extend the grammar string and reference new
rules from an existing rule. Unreferenced rules are available via `parse_rule`.
A second macro invocation cannot append rules; there is no runtime registration.

Expansion rejects duplicate/undefined rules, malformed grammar, nullable
repetitions and direct/indirect nullable-prefix left recursion. Nullability is
conservative: predicates are considered nullable even when unsatisfiable, so
some unreachable grammars are intentionally rejected. Runtime parsing has a
hard bound of 512 nested generated calls (including expression helpers), returning
a normal error on exhaustion, even inside optional branches or predicates.
This is a safety limit, not a supported input nesting guarantee; complex rules
consume more calls per input nesting level. Grammar groups and predicate chains
also have compile-time nesting limits.

No memoization is performed: backtracking can require exponential work.
Each parse has a fixed work budget of
1,000,000 generated expression calls. Exhaustion returns a fatal `ParseError`
with a `work limit` expectation, never swallowed by choices, optional branches,
repetition, or predicates. The recursion limit is likewise fatal. These are
resource errors, not evidence that the input violates the grammar. Each call can
still perform input-sized literal comparisons, tree operations, and diagnostic
allocation: the call budget is not a wall-clock or memory guarantee. Trees and
speculative trees allocate memory.
Ordered choice commits to its first successful branch; repetition is greedy
and does not give characters back to later sequence elements. There is no
left-recursion support, recovery, streaming, automatic whitespace, or guaranteed
linear complexity. Intended for small grammars and inputs.
