//! `dl_query_processing.postprocessing.postprocessors.all.postprocess_array`.
//!
//! A `tree_str` (or `array_*`) column reaches the port as the PostgreSQL array literal
//! (`{Electronics}`), while Python gets a list from the driver and hands the frontend
//! `json.dumps(value, ensure_ascii=False)`. The demo's tree table shows `["Electronics"]`, so
//! the literal has to be decoded and re-encoded the way Python does it.

use dfl_native::DataType;
use serde_json::Value;

/// `postprocess_array`: the value as `json.dumps` spells it, `None` if it is not an array.
pub fn postprocess_array(text: &str, dtype: DataType) -> Option<String> {
    // PostgreSQL hands Python a list and the executor renders it as `{...}`; ClickHouse's
    // `JSONCompact` document already contains a JSON array. Both end up as the same JSON the
    // frontend reads, so the ClickHouse spelling is accepted as well.
    let trimmed = text.trim();
    if trimmed.starts_with('[') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if value.is_array() {
                return Some(json_dumps(&value));
            }
        }
    }
    let mut pos = 0;
    let value = parse_array(text.as_bytes(), &mut pos, dtype)?;
    if pos != text.len() {
        return None;
    }
    Some(json_dumps(&value))
}

/// `{1,"a b",NULL}`, nested for a multidimensional array.
///
/// The quoting rules are PostgreSQL's: a double quote starts an element, `\"` and `\\` are the
/// only escapes inside it, and an unquoted `NULL` (in any case) is a null element.
fn parse_array(data: &[u8], pos: &mut usize, dtype: DataType) -> Option<Value> {
    if data.get(*pos) != Some(&b'{') {
        return None;
    }
    *pos += 1;
    let mut items = Vec::new();
    if data.get(*pos) == Some(&b'}') {
        *pos += 1;
        return Some(Value::Array(items));
    }
    loop {
        if data.get(*pos) == Some(&b'{') {
            items.push(parse_array(data, pos, dtype)?);
        } else {
            items.push(parse_element(data, pos, dtype)?);
        }
        match data.get(*pos) {
            Some(b',') => *pos += 1,
            Some(b'}') => {
                *pos += 1;
                return Some(Value::Array(items));
            }
            _ => return None,
        }
    }
}

fn parse_element(data: &[u8], pos: &mut usize, dtype: DataType) -> Option<Value> {
    let quoted = data.get(*pos) == Some(&b'"');
    let mut text = Vec::new();
    if quoted {
        *pos += 1;
        loop {
            match data.get(*pos) {
                Some(b'"') => {
                    *pos += 1;
                    break;
                }
                Some(b'\\') => {
                    text.push(*data.get(*pos + 1)?);
                    *pos += 2;
                }
                Some(byte) => {
                    text.push(*byte);
                    *pos += 1;
                }
                None => return None,
            }
        }
    } else {
        while !matches!(data.get(*pos), Some(b',') | Some(b'}') | None) {
            text.push(data[*pos]);
            *pos += 1;
        }
    }
    let text = std::str::from_utf8(&text).ok()?;
    // `NULL` is a null element only when it is not quoted, which is why `"NULL"` stays a string.
    if !quoted && text.eq_ignore_ascii_case("NULL") {
        return Some(Value::Null);
    }
    element(text, dtype)
}

/// One element in the Python type the driver would have produced (`int[]` -> `[1, 2]`).
fn element(text: &str, dtype: DataType) -> Option<Value> {
    match dtype.base() {
        DataType::ArrayInt => Some(Value::Number(text.trim().parse::<i64>().ok()?.into())),
        DataType::ArrayFloat => {
            let number = text.trim().parse::<f64>().ok()?;
            serde_json::Number::from_f64(number).map(Value::Number)
        }
        // `array_str` and `tree_str` are `text[]`.
        _ => Some(Value::String(text.to_string())),
    }
}

/// `json.dumps(value, ensure_ascii=False)`: `", "` between items, `": "` after a key.
fn json_dumps(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(json_dumps).collect();
            format!("[{}]", items.join(", "))
        }
        Value::String(text) => json_string(text),
        Value::Null => "null".to_string(),
        Value::Number(number) => match number.as_f64() {
            // `repr(1.0)` is `1.0`, so a float keeps its fractional part.
            Some(_) if number.is_f64() => crate::service::python_float(&number.to_string()),
            _ => number.to_string(),
        },
        other => other.to_string(),
    }
}

/// `json.dumps` of a string: the two mandatory escapes plus the control characters.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_array_becomes_a_json_list() {
        // The demo's tree table: `(["Electronics"])` as `postprocess_array` writes it.
        assert_eq!(
            postprocess_array("{Electronics}", DataType::TreeStr).unwrap(),
            "[\"Electronics\"]"
        );
        assert_eq!(
            postprocess_array("{Auto,\"Cottage and garden\"}", DataType::ArrayStr).unwrap(),
            "[\"Auto\", \"Cottage and garden\"]"
        );
    }

    #[test]
    fn quoting_escaping_and_nulls_follow_postgres() {
        assert_eq!(
            postprocess_array("{\"a,b\",\"c\\\"d\",NULL,\"NULL\"}", DataType::ArrayStr).unwrap(),
            "[\"a,b\", \"c\\\"d\", null, \"NULL\"]"
        );
        assert_eq!(
            postprocess_array("{{1,2},{3,4}}", DataType::ArrayInt).unwrap(),
            "[[1, 2], [3, 4]]"
        );
    }

    #[test]
    fn numbers_keep_their_python_spelling() {
        assert_eq!(
            postprocess_array("{1.5,2}", DataType::ArrayFloat).unwrap(),
            "[1.5, 2.0]"
        );
    }

    #[test]
    fn something_that_is_not_an_array_is_left_alone() {
        assert!(postprocess_array("", DataType::TreeStr).is_none());
        assert!(postprocess_array("{a", DataType::TreeStr).is_none());
        assert!(postprocess_array("{a}b", DataType::TreeStr).is_none());
    }
}
