//! `dl_query_processing.postprocessing.postprocessors.markup`.
//!
//! A `markup` formula is translated into a nested s-expression: `URL(url, title)` becomes
//! `(a "url" "title")`, `STR([x]) + STR([y])` becomes `(c "x" "y")`. The postprocessor turns that
//! string into the JSON the frontend renders, so a markup cell is an object and not the raw
//! text. `MarkupProcessing` is ported as it is, including the `""` escaping of a quote inside a
//! string literal, so the node names and their verbalizations are the Python ones.

use serde_json::{json, Value};

const QUOT: u8 = b'"';
const LPAR: u8 = b'(';
const RPAR: u8 = b')';
const SEP: u8 = b' ';

/// A parsed markup expression: `(funcname arg1 arg2 ...)`, with strings as leaves.
///
/// Python keeps `str | tuple`; the enum makes the same distinction explicit.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Call(String, Vec<Node>),
}

/// `MARKUP_PROCESSING.parse_and_verbalize`.
///
/// `None` where Python raises (`ParseError`/`DumpError`): those become a 500 there, and a
/// malformed markup should not take a working chart down with it.
pub fn postprocess(text: &str) -> Option<Value> {
    verbalize(&parse(text)?)
}

/// `MarkupProcessing.parse`: the value must be a single complete `(...)` expression.
fn parse(data: &str) -> Option<Node> {
    if !data.starts_with('(') || !data.ends_with(')') {
        return None;
    }
    let bytes = data.as_bytes();
    let (node, pos) = parse_i(bytes, 0)?;
    (pos == bytes.len()).then_some(node)
}

/// `MarkupProcessing._parse_i`.
fn parse_i(data: &[u8], pos: usize) -> Option<(Node, usize)> {
    if data.get(pos) == Some(&LPAR) {
        let pos = pos + 1;
        let next_rpar = find(data, RPAR, pos)?;
        let Some(next_sep) = find_before(data, SEP, pos, next_rpar) else {
            // `(funcname)`
            let name = str_at(data, pos, next_rpar)?;
            return Some((Node::Call(name, Vec::new()), next_rpar + 1));
        };
        let name = str_at(data, pos, next_sep)?;
        let mut args = Vec::new();
        let mut pos = next_sep + 1;
        while data.get(pos) != Some(&RPAR) {
            let (node, next) = parse_i(data, pos)?;
            args.push(node);
            pos = next;
            if data.get(pos) == Some(&RPAR) {
                break;
            }
            if data.get(pos) != Some(&SEP) {
                return None;
            }
            pos += 1;
        }
        if data.get(pos) != Some(&RPAR) {
            return None;
        }
        return Some((Node::Call(name, args), pos + 1));
    }

    if data.get(pos) == Some(&QUOT) {
        let mut pos = pos + 1;
        let mut text = String::new();
        loop {
            let next_quot = find(data, QUOT, pos)?;
            text.push_str(str_at(data, pos, next_quot)?.as_str());
            // A quote written twice is the quote itself, which is how a value is embedded.
            if data.get(next_quot + 1) == Some(&QUOT) {
                text.push('"');
                pos = next_quot + 2;
            } else {
                return Some((Node::Text(text), next_quot + 1));
            }
        }
    }

    None
}

fn find(data: &[u8], needle: u8, from: usize) -> Option<usize> {
    data.get(from..)?
        .iter()
        .position(|byte| *byte == needle)
        .map(|offset| from + offset)
}

/// `_find_next(data, substr, start, end)`: `str.index` never looks at or past `end`.
fn find_before(data: &[u8], needle: u8, from: usize, end: usize) -> Option<usize> {
    let end = end.min(data.len());
    if from >= end {
        return None;
    }
    find(data, needle, from).filter(|position| *position < end)
}

fn str_at(data: &[u8], from: usize, to: usize) -> Option<String> {
    std::str::from_utf8(data.get(from..to)?)
        .ok()
        .map(str::to_string)
}

/// `_argcount_mismatch`: a node whose arguments do not fit says nothing at all.
fn empty_markup() -> Value {
    json!({ "type": "concat", "children": Vec::<Value>::new() })
}

/// `MarkupProcessing._verbalize_i`: the serializable form of a node.
fn verbalize(node: &Node) -> Option<Value> {
    let (name, args) = match node {
        Node::Text(text) => return Some(json!({ "type": "text", "content": text })),
        Node::Call(name, args) => (name.as_str(), args),
    };
    let arg = |index: usize| args.get(index).and_then(verbalize);
    let text_arg = |index: usize| match args.get(index) {
        Some(Node::Text(text)) => Some(Value::String(text.clone())),
        _ => None,
    };
    match name {
        // `c` — the SQL concatenation of every piece of the markup.
        "c" => {
            let children: Option<Vec<Value>> = args.iter().map(verbalize).collect();
            Some(json!({ "type": "concat", "children": children? }))
        }
        "a" if args.len() == 2 => Some(json!({
            "type": "url",
            "url": text_arg(0)?,
            "content": arg(1)?,
        })),
        "cl" if args.len() == 2 => Some(json!({
            "type": "color",
            "color": text_arg(1)?,
            "content": arg(0)?,
        })),
        "sz" if args.len() == 2 => Some(json!({
            "type": "size",
            "size": text_arg(1)?,
            "content": arg(0)?,
        })),
        "userinfo" if args.len() == 2 => Some(json!({
            "type": "user_info",
            "user_info": text_arg(1)?,
            "content": arg(0)?,
        })),
        "br" if args.is_empty() => Some(json!({ "type": "br" })),
        "i" if args.len() == 1 => Some(json!({ "type": "italics", "content": arg(0)? })),
        "b" if args.len() == 1 => Some(json!({ "type": "bold", "content": arg(0)? })),
        "img" if args.len() == 4 => Some(json!({
            "type": "img",
            "src": empty_is_null(text_arg(0)?),
            "width": empty_is_null(text_arg(1)?),
            "height": empty_is_null(text_arg(2)?),
            "alt": empty_is_null(text_arg(3)?),
        })),
        "tooltip" if args.len() == 2 || args.len() == 3 => {
            let placement = match args.len() {
                2 => Value::String("auto".to_string()),
                _ => text_arg(2)?,
            };
            if !matches!(
                placement.as_str(),
                Some("top" | "right" | "bottom" | "left" | "auto")
            ) {
                return None;
            }
            Some(json!({
                "type": "tooltip",
                "content": arg(0)?,
                "tooltip": arg(1)?,
                "placement": placement,
            }))
        }
        "a" | "cl" | "sz" | "userinfo" | "br" | "i" | "b" | "img" | "tooltip" => {
            Some(empty_markup())
        }
        // `DumpError("Unknown func", node)`: markup the port does not understand stays as text.
        _ => None,
    }
}

/// `None if src == "" else src`: an empty `src`/`alt` is a JSON `null`.
fn empty_is_null(value: Value) -> Value {
    match &value {
        Value::String(text) if text.is_empty() => Value::Null,
        _ => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_url_markup_becomes_the_frontend_object() {
        // The demo's "Top products" cell: `(a "url" "title")`, built by
        // `(a "https://..." "Model")`.
        assert_eq!(
            postprocess("(a \"https://example.com/x\" \"Umka piano\")").unwrap(),
            json!({
                "type": "url",
                "url": "https://example.com/x",
                "content": {"type": "text", "content": "Umka piano"},
            })
        );
    }

    #[test]
    fn a_quote_is_escaped_by_doubling_it() {
        // `_quote_i`: a `"` inside a value is written twice, so the parser folds it back.
        assert_eq!(
            postprocess("(c \"a\"\"b\" \"c\")").unwrap(),
            json!({
                "type": "concat",
                "children": [
                    {"type": "text", "content": "a\"b"},
                    {"type": "text", "content": "c"},
                ],
            })
        );
    }

    #[test]
    fn the_other_nodes_verbalize_like_dl_formula() {
        assert_eq!(
            postprocess("(c (b (i \"x\")) (br) (sz \"big\" \"12\") (cl \"red\" \"#f00\"))")
                .unwrap(),
            json!({
                "type": "concat",
                "children": [
                    {"type": "bold", "content": {"type": "italics", "content": {"type": "text", "content": "x"}}},
                    {"type": "br"},
                    {"type": "size", "size": "12", "content": {"type": "text", "content": "big"}},
                    {"type": "color", "color": "#f00", "content": {"type": "text", "content": "red"}},
                ],
            })
        );
        assert_eq!(
            postprocess("(img \"\" \"10\" \"20\" \"\")").unwrap(),
            json!({"type": "img", "src": null, "width": "10", "height": "20", "alt": null})
        );
        assert_eq!(
            postprocess("(tooltip \"x\" \"tip\")").unwrap(),
            json!({
                "type": "tooltip",
                "content": {"type": "text", "content": "x"},
                "tooltip": {"type": "text", "content": "tip"},
                "placement": "auto",
            })
        );
    }

    #[test]
    fn malformed_markup_is_not_an_error_here() {
        // `parse`/`_verbalize_i` raise in Python; the port reports `None` so the caller can fall
        // back to the raw text.
        for data in ["", "x", "(a \"x\" \"y\"", "(unknown \"x\")"] {
            assert!(postprocess(data).is_none(), "{data}");
        }
        // A wrong argument count is `_argcount_mismatch`, i.e. an empty concat, not a failure.
        for data in ["(b \"a\" \"b\")", "(a)", "(a \"x\")", "(br \"x\")"] {
            assert_eq!(
                postprocess(data).unwrap(),
                json!({"type": "concat", "children": []}),
                "{data}"
            );
        }
    }

    #[test]
    fn a_non_ascii_value_survives_the_byte_scan() {
        assert_eq!(
            postprocess("(c \"Привет\" \"\u{a0}\")").unwrap(),
            json!({
                "type": "concat",
                "children": [
                    {"type": "text", "content": "Привет"},
                    {"type": "text", "content": "\u{a0}"},
                ],
            })
        );
    }
}
