# `dfl_native` — the formula compiler

The Rust implementation of the DataLens formula language: lexer, Pratt parser, structural
analysis and SQL code generation for PostgreSQL, ClickHouse and MySQL. It is a workspace
member of the data-api and is linked as a normal library (`dfl_native::compile_formula`),
with no C ABI and no Python runtime.

```rust
use dfl_native::{compile_formula, DataType, FieldTypes};

let types: FieldTypes = [("Sales".to_string(), DataType::Integer)].into_iter().collect();
let compiled = compile_formula("[Sales] * 2", "postgres", &types).unwrap();
assert_eq!(compiled.sql, r#""Sales" * 2"#);
```

The function and dialect registry (`registry_generated.rs`, `func_props_generated.rs`) is
generated from the `dl_formula` registry of the DataLens project rather than written by
hand, which is what keeps the SQL byte-compatible with the reference implementation.

## CLI

`dfl-cli` is a debugging aid:

```bash
cargo run --bin dfl-cli -- sql postgres "[Sales] * 2" "Sales:INTEGER"
cargo run --bin dfl-cli -- parse "[Sales] * 2"
cargo run --bin dfl-cli -- templates          # number of registry rows
printf 'SUM([Sales])\n' | cargo run --bin dfl-cli -- bench postgres 1000
```

## Tests

```bash
cargo test -p dfl_native
```

`tests/compiler.rs` covers parsing, error positions, type inference and the SQL of every
dialect family.
