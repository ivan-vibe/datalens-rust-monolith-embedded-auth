//! Lookup over the template table generated from the `dl_formula` registry.
//!
//! A template row answers the question "how does this dialect render `NAME(args...)`
//! when the arguments have these types" — the same question `dl_formula` answers with
//! `OPERATION_REGISTRY.get_definition()`, but as data instead of Python callables.

use crate::dtype::DataType;
use crate::registry_generated::TEMPLATES;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
pub struct Entry {
    pub template: &'static str,
    /// Template slot -> argument index in the formula (non-identity for functions that
    /// `dl_formula` rewrites, e.g. `DATEADD('day', 1, [d])`).
    pub slots: &'static str,
    pub ret: DataType,
}

impl Entry {
    /// Argument index for a template placeholder.
    pub fn arg_index(&self, slot: usize) -> usize {
        if self.slots.is_empty() {
            return slot;
        }
        self.slots
            .split(',')
            .nth(slot)
            .and_then(|idx| idx.trim().parse().ok())
            .unwrap_or(slot)
    }
}

type EntryMap = HashMap<&'static str, Entry>; // signature -> entry
type ArgcMap = HashMap<u8, EntryMap>;
type NameMap = HashMap<&'static str, ArgcMap>;
type DialectMap = HashMap<&'static str, NameMap>;

fn build() -> DialectMap {
    let mut map: DialectMap = HashMap::new();
    for (dialect, name, argc, signature, slots, ret, template) in TEMPLATES {
        let ret = DataType::from_name(ret).unwrap_or(DataType::Unsupported);
        map.entry(dialect)
            .or_default()
            .entry(name)
            .or_default()
            .entry(*argc)
            .or_default()
            .entry(signature)
            .or_insert(Entry {
                template,
                slots,
                ret,
            });
    }
    map
}

pub fn table() -> &'static DialectMap {
    static TABLE: OnceLock<DialectMap> = OnceLock::new();
    TABLE.get_or_init(build)
}

/// Lookup by name without allocating when the name fits into the stack buffer.
fn by_name(dialect: &str, name: &str) -> Option<&'static ArgcMap> {
    let per_dialect = table().get(dialect)?;
    if let Some(found) = per_dialect.get(name) {
        return Some(found);
    }
    let ascii = name.as_bytes();
    if ascii.len() <= 32 && ascii.is_ascii() {
        let mut buf = [0u8; 32];
        for (idx, byte) in ascii.iter().enumerate() {
            buf[idx] = byte.to_ascii_lowercase();
        }
        // SAFETY: input was checked to be ASCII and lowercasing keeps it ASCII.
        let lower = unsafe { std::str::from_utf8_unchecked(&buf[..ascii.len()]) };
        return per_dialect.get(lower);
    }
    per_dialect.get(name.to_ascii_lowercase().as_str())
}

/// Find the template for `name(args)` with the given argument signature.
///
/// The signature is a `|`-separated list of tokens: a type name for expressions and
/// `lit:<kind>:<value>` for literals. Lookup degrades gracefully: a literal without a
/// value-specific row falls back to "a constant of this type", and a constant type falls
/// back to its plain type — the implicit casts `dl_formula` also applies.
pub fn lookup(dialect: &str, name: &str, argc: u8, signature: &str) -> Option<&'static Entry> {
    let per_argc = by_name(dialect, name)?.get(&argc)?;
    // fast path: the signature is the key
    if let Some(entry) = per_argc.get(signature) {
        return Some(entry);
    }
    // A signature without literals still needs the implicit casts: `LEN(TREE_STR)` is
    // `LEN(ARRAY_STR)`, because `TREE_STR.casts_to(ARRAY_STR)`. Returning early here used to
    // skip the widening below and report a missing translation.
    for candidate in candidates(signature) {
        if let Some(entry) = per_argc.get(candidate.as_str()) {
            return Some(entry);
        }
    }
    None
}

/// Signature variants from most specific to most generic.
fn candidates(signature: &str) -> Vec<String> {
    let tokens: Vec<&str> = signature.split('|').collect();
    let mut variants: Vec<String> = vec![signature.to_string()];

    // For every literal position choose one of: keep the value, its constant type, its
    // base type. The most specific variants come first.
    let literal_positions: Vec<usize> = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| token.starts_with("lit:"))
        .map(|(idx, _)| idx)
        .collect();
    let choices_per_position = 3usize;
    let total = choices_per_position.pow(literal_positions.len() as u32);
    let mut generated: Vec<(usize, String)> = Vec::new();
    for code in 1..total.min(64) {
        let mut variant = tokens.clone();
        let mut specificity = 0usize;
        let mut rest = code;
        for pos in &literal_positions {
            let choice = rest % choices_per_position;
            rest /= choices_per_position;
            match choice {
                0 => specificity += 1,
                1 => variant[*pos] = const_type_name(tokens[*pos]),
                _ => variant[*pos] = base_type_name(tokens[*pos]),
            }
        }
        generated.push((specificity, variant.join("|")));
    }
    generated.sort_by(|left, right| right.0.cmp(&left.0));
    variants.extend(generated.into_iter().map(|(_, variant)| variant));

    // finally: everything reduced to plain types
    let base: Vec<&str> = tokens.iter().map(|token| base_type_name(token)).collect();
    variants.push(base.join("|"));

    // Last, the implicit casts `dl_formula` applies in `ArgTypeSequence.match_arg_types`:
    // a translation declared for `(GENERICDATETIME, FLOAT)` also accepts `[dt] - 1`, because
    // `CONST_INTEGER.casts_to(FLOAT)`. The declared types are the keys of the table, so the
    // widening has to happen here rather than in the type inference. It is applied to every
    // reduced variant, not only to the request's own signature: `SLICE(TREE_STR, 1, 4)` needs
    // the tree -> array cast *and* the "any constant" reduction at once, and only their
    // combination (`ARRAY_STR|CONST_INTEGER|CONST_INTEGER`) is in the table. One position at
    // a time keeps the list short.
    let mut widened: Vec<String> = Vec::new();
    for original in variants.clone() {
        let original_tokens: Vec<&str> = original.split('|').collect();
        for pos in 0..original_tokens.len() {
            let Some(dtype) = DataType::from_name(base_type_name(original_tokens[pos])) else {
                continue;
            };
            for target in DataType::all() {
                if target == dtype || !dtype.casts_to(target) {
                    continue;
                }
                let mut variant = original_tokens.clone();
                variant[pos] = target.name();
                let variant = variant.join("|");
                if !variants.contains(&variant) && !widened.contains(&variant) {
                    widened.push(variant);
                }
            }
        }
    }
    variants.extend(widened);
    variants
}

fn base_type_name(token: &str) -> &'static str {
    match DataType::from_name(const_type_name(token)) {
        Some(dtype) => dtype.base().name(),
        None => "UNSUPPORTED",
    }
}

fn const_type_name(token: &str) -> &'static str {
    match token.strip_prefix("lit:") {
        Some(rest) => match rest.split(':').next().unwrap_or("") {
            "int" => "CONST_INTEGER",
            "float" => "CONST_FLOAT",
            "bool" => "CONST_BOOLEAN",
            _ => "CONST_STRING",
        },
        None => match DataType::from_name(token) {
            Some(dtype) => dtype.name(),
            None => "UNSUPPORTED",
        },
    }
}

pub fn len() -> usize {
    crate::registry_generated::TEMPLATES.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_entries_for_all_dialects() {
        for dialect in ["postgres", "mysql", "clickhouse"] {
            assert!(
                lookup(dialect, "upper", 1, "STRING").is_some(),
                "UPPER is missing for {dialect}"
            );
        }
    }

    #[test]
    fn literal_arguments_are_part_of_the_key() {
        assert!(lookup("postgres", "dateadd", 3, "lit:str:day|CONST_INTEGER|DATE").is_some());
        assert!(lookup(
            "postgres",
            "dateadd",
            3,
            "lit:str:day|CONST_INTEGER|DATETIME"
        )
        .is_some());
        // the documented argument order (unit, number, date)
        assert!(lookup("postgres", "dateadd", 3, "lit:str:month|lit:int:1|DATE").is_some());
    }

    #[test]
    fn const_and_base_types_fall_back() {
        // `[f] / 2` -> (FLOAT, CONST_INTEGER) is not stored, but (FLOAT, FLOAT) is
        assert!(lookup("postgres", "/", 2, "FLOAT|FLOAT").is_some());
    }

    #[test]
    fn a_tree_is_matched_by_the_array_signature() {
        // `TREE_STR.casts_to(ARRAY_STR)` in `ArgTypeSequence.match_arg_types`, so the tree role
        // can be sliced and measured: `LEN(TREE_STR)` is `LEN(ARRAY_STR)`.
        let len = lookup("postgres", "len", 1, "TREE_STR").expect("len");
        assert!(len.template.contains("ARRAY_LENGTH"));
        let slice = lookup("postgres", "slice", 3, "TREE_STR|lit:int:1|lit:int:2").expect("slice");
        assert_eq!(slice.template, "({0})[1:(1 + 2) - 1]");
        // ... at any depth: a drill-down block asks for `SLICE(tree, 1, level)`. Levels 1..3 are
        // the probed literals, a deeper branch needs the generic row — and finding it takes the
        // tree -> array cast *and* the "any constant" reduction at once
        // (`ARRAY_STR|CONST_INTEGER|CONST_INTEGER`), which is what the widened candidates are
        // for. Without either the row or the combination the wrapper is unsupported past level 3.
        for (level, postgres_sql, clickhouse_sql) in [
            (1, "({0})[1:(1 + 1) - 1]", "arraySlice({0}, 1, 1)"),
            (3, "({0})[1:(1 + 3) - 1]", "arraySlice({0}, 1, 3)"),
            (4, "({0})[{1}:({1} + {2}) - 1]", "arraySlice({0}, {1}, {2})"),
            (7, "({0})[{1}:({1} + {2}) - 1]", "arraySlice({0}, {1}, {2})"),
        ] {
            let signature = format!("TREE_STR|lit:int:1|lit:int:{level}");
            let deep = lookup("postgres", "slice", 3, &signature)
                .unwrap_or_else(|| panic!("no slice for {signature}"));
            assert_eq!(deep.template, postgres_sql);
            let clickhouse = lookup("clickhouse", "slice", 3, &signature)
                .unwrap_or_else(|| panic!("no clickhouse slice for {signature}"));
            assert_eq!(clickhouse.template, clickhouse_sql);
        }
        // MySQL has no array type, and `dl_formula` says so.
        assert!(lookup("mysql", "slice", 3, "TREE_STR|lit:int:1|lit:int:4").is_none());
    }
}
