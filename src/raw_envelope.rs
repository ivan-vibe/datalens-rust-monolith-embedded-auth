//! Single-pass chart response envelope.
//!
//! The four Rust `/api/run` builders used to assemble the envelope with the `json!` macro and then
//! serialize the resulting `Value` tree into a `BytesMut`:
//!
//! * `data` (the biggest member, up to hundreds of KB) was materialised inside yet another `Value`
//!   tree before being serialized;
//! * `config`/`highchartsConfig` were serialized to a `String` first and then escaped **again** by
//!   the envelope serializer (two passes over the same JSON text, one extra `String` allocation);
//! * the body was built in a growable `BytesMut` with a `serde_json::to_writer` writer adapter.
//!
//! This module writes the final response body directly into one `Vec<u8>` with the final capacity:
//!
//! * `member_value` serializes a `Value` straight into the buffer (one pass, no envelope tree);
//! * `member_json_text` streams `Value` -> JSON into an escaping writer, i.e. the inner JSON text
//!   is escaped into the buffer as it is produced — no intermediate `String` copy;
//! * `finish` hands the buffer to `axum::body::Body` (one write, no `BytesMut` growth/copy).
//!
//! The wire format is unchanged: `config`/`highchartsConfig` remain JSON **strings**, the member
//! order is the same, and the escaping matches `serde_json`'s (the inner text produced by the
//! serializer contains no raw control bytes, so escaping `"` and `\` reproduces it exactly).

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::Value;
use std::io::{self, Write};

/// One response body being built; call [`Envelope::finish`] to turn it into an axum response.
pub struct Envelope {
    buffer: Vec<u8>,
    first_member: bool,
}

impl Envelope {
    /// `capacity` is a hint: chart responses are usually 16-64 KB, so this avoids `Vec` growth.
    pub fn with_capacity(capacity: usize) -> Self {
        let mut buffer = Vec::with_capacity(capacity);
        buffer.push(b'{');
        Self {
            buffer,
            first_member: true,
        }
    }

    fn key(&mut self, key: &str) {
        if !self.first_member {
            self.buffer.push(b',');
        }
        self.first_member = false;
        self.buffer.push(b'"');
        escape_into(&mut self.buffer, key.as_bytes());
        self.buffer.extend_from_slice(b"\":");
    }

    /// `"key": <value>` — `serde_json` serializes the value directly into the body.
    pub fn member_value(&mut self, key: &str, value: &Value) -> io::Result<()> {
        self.key(key);
        serde_json::to_writer(&mut self.buffer, value).map_err(to_io)
    }

    /// `"key": <serializable value>` — the typed equivalent of [`Self::member_value`].
    pub fn member_serialize<T: serde::Serialize>(&mut self, key: &str, value: &T) -> io::Result<()> {
        self.key(key);
        serde_json::to_writer(&mut self.buffer, value).map_err(to_io)
    }

    /// `"key": "<json text>"` for a typed value (the typed [`Self::member_json_text`]).
    pub fn member_json_text_serialize<T: serde::Serialize>(
        &mut self,
        key: &str,
        value: &T,
    ) -> io::Result<()> {
        self.key(key);
        self.buffer.push(b'"');
        {
            let mut writer = EscapeWriter {
                out: &mut self.buffer,
            };
            serde_json::to_writer(&mut writer, value).map_err(to_io)?;
        }
        self.buffer.push(b'"');
        Ok(())
    }

    /// `"key": "<json text>"` — the value is serialized straight into an escaping writer.
    ///
    /// This is the `config` / `highchartsConfig` path: the response contract stores these members
    /// as JSON strings, so the inner JSON has to be escaped, but it no longer needs a `String`.
    pub fn member_json_text(&mut self, key: &str, value: &Value) -> io::Result<()> {
        self.key(key);
        self.buffer.push(b'"');
        {
            let mut writer = EscapeWriter {
                out: &mut self.buffer,
            };
            serde_json::to_writer(&mut writer, value).map_err(to_io)?;
        }
        self.buffer.push(b'"');
        Ok(())
    }

    /// `"key": "<json text>"` for a JSON fragment that is already a `&str`
    /// (`metric`'s config is a static literal, `"{}"` for `highchartsConfig`).
    pub fn member_json_literal(&mut self, key: &str, text: &str) -> io::Result<()> {
        self.key(key);
        self.buffer.push(b'"');
        escape_into(&mut self.buffer, text.as_bytes());
        self.buffer.push(b'"');
        Ok(())
    }

    /// `"key": <raw JSON token>` for `null` / `{}` without building a `Value`.
    pub fn member_raw(&mut self, key: &str, raw: &str) -> io::Result<()> {
        self.key(key);
        self.buffer.extend_from_slice(raw.as_bytes());
        Ok(())
    }

    pub fn member_str(&mut self, key: &str, value: &str) -> io::Result<()> {
        self.key(key);
        self.buffer.push(b'"');
        escape_into(&mut self.buffer, value.as_bytes());
        self.buffer.push(b'"');
        Ok(())
    }

    /// Close the object and return the response; the buffer is moved into the body (no copy).
    pub fn finish(mut self) -> Result<Response, (StatusCode, String)> {
        self.buffer.push(b'}');
        Ok(Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(Bytes::from(self.buffer)))
            .expect("envelope response"))
    }
}

/// Serialize a complete `Value` response in one pass (`pivot` still returns a `Value` envelope).
pub fn write_response(value: &Value, capacity: usize) -> Result<Response, (StatusCode, String)> {
    let mut buffer = Vec::with_capacity(capacity);
    serde_json::to_writer(&mut buffer, value)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(Bytes::from(buffer)))
        .expect("envelope response"))
}

fn to_io(error: serde_json::Error) -> io::Error {
    io::Error::other(error)
}

/// `io::Write` adapter that JSON-escapes everything written to it into `out`.
struct EscapeWriter<'a> {
    out: &'a mut Vec<u8>,
}

impl Write for EscapeWriter<'_> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        escape_into(self.out, data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Escape bytes for a JSON string body. `serde_json`'s output only needs `"` and `\` escaped (it
/// already emits control characters as `\u00XX`), but the full table is kept for correctness.
fn escape_into(out: &mut Vec<u8>, bytes: &[u8]) {
    for &byte in bytes {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x0a => out.extend_from_slice(b"\\n"),
            0x0d => out.extend_from_slice(b"\\r"),
            0x09 => out.extend_from_slice(b"\\t"),
            byte if byte < 0x20 => {
                out.extend_from_slice(b"\\u00");
                out.push(hex(byte >> 4));
                out.push(hex(byte & 0x0f));
            }
            byte => out.push(byte),
        }
    }
}

fn hex(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        _ => b'a' + (nibble - 10),
    }
}
