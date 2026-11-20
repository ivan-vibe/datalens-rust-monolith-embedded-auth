//! Native (Rust) implementation of the DataLens formula compiler.
//!
//! The dialect knowledge is not hand-written: `registry_generated.rs` is generated from
//! the `dl_formula` registry of the DataLens project, so the native table can be
//! regenerated when the upstream registry changes.
//!
//! Public entry points:
//! * [`parse_formula`] - formula text -> AST (analogue of `dl_formula.parser.parse`)
//! * [`compile_formula`] - formula text -> SQL (analogue of `translate_and_compile`)
//! * [`compile_named`] - same, with multipart field names

pub mod analysis;
pub mod ast;
pub mod codegen;
pub mod dtype;
pub mod func_props;
pub mod func_props_generated;
pub mod lexer;
pub mod parser;
pub mod registry;
pub mod registry_generated;

pub use analysis::{analyze, Analysis};
pub use ast::{FieldName, FieldNames, FieldTypes, Grouping, Item, LodKind};
pub use codegen::{
    compile, compile_external, compile_leaves, compile_leaves_from, compile_named,
    fork_nested_windows, infer_type, is_window_sql, Compiled, CompiledLeaves, Dialect,
    ExternalCall, ExternalKind, Leaf, WindowContext,
};
pub use dtype::DataType;
pub use parser::{ParseError, Parser};

/// Parse a formula into an AST (no dialect involved).
pub fn parse_formula(text: &str) -> Result<Item, String> {
    parse_formula_typed(text).map_err(|e| e.message)
}

/// Parse a formula, keeping the structured error of [`ParseError`].
///
/// Same accept/reject decision as [`parse_formula`], but the rejection comes back as
/// `dl_formula`'s `FormulaErrorCtx` fields (code, token, position) instead of a message.
pub fn parse_formula_typed(text: &str) -> Result<Item, ParseError> {
    Parser::new(text).parse_formula()
}

/// Parse + generate SQL for the given dialect.
pub fn compile_formula(text: &str, dialect: &str, types: &FieldTypes) -> Result<Compiled, String> {
    compile_formula_named(text, dialect, types, &FieldNames::new())
}

/// Parse + generate SQL, rendering fields through the given physical names.
pub fn compile_formula_named(
    text: &str,
    dialect: &str,
    types: &FieldTypes,
    names: &FieldNames,
) -> Result<Compiled, String> {
    let dialect = Dialect::parse(dialect).ok_or_else(|| format!("unknown dialect: {dialect}"))?;
    let item = parse_formula(text)?;
    compile_named(&item, dialect, types, names)
}
