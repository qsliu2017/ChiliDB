# Compile-time contracts


```compile_fail
mod bad { peg::grammar!("Root <- Missing;"); }
```

```compile_fail
mod bad { peg::grammar!("Root <- 'a'; Root <- 'b';"); }
```

```compile_fail
mod bad { peg::grammar!("Root <- Root 'a' / 'b';"); }
```

```compile_fail
mod bad { peg::grammar!("A <- '' B; B <- A / 'x';"); }
```

```compile_fail
mod bad { peg::grammar!("Root <- ('a'?)*;"); }
```

```compile_fail
mod bad { peg::grammar!("Root <- [z-a];"); }
```

```compile_fail
mod bad { peg::grammar!("Root <- ('a';"); }
```

Trees cannot outlive their source, and callers cannot mutate their fields:

```compile_fail
mod grammar { peg::grammar!("Root <- 'hello';"); }
let tree = {
    let input = String::from("hello");
    grammar::parse(&input).unwrap()
};
println!("{}", tree.text());
```

```compile_fail
mod grammar { peg::grammar!("Root <- 'hello';"); }
let mut tree = grammar::parse("hello").unwrap();
tree.rule = "Other";
```
