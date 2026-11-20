//! Response bodies: JSON (the contract) and MessagePack (the same body, smaller).
//!
//! `data-api`'s contract is JSON — the UI parses it, and the reference corpus compares it —
//! so JSON stays the default and the only format the reference answers. MessagePack is the
//! second format for a different kind of client: a chart body is a few hundred kilobytes of
//! repetitive keys (`{"data": [...], "legend": [...]}`) and numbers-as-strings, which JSON spends
//! ~5 bytes of syntax per value on. A dashboard that polls many charts pays that per request.
//!
//! The base prepared here:
//!
//! * negotiation is `Accept: application/msgpack`; anything else (including no `Accept`) is JSON,
//!   so nothing changes for the UI, the gateway or the corpus;
//! * the body holds exactly the bytes of the same `Serialize` type, so a client can decode to its
//!   own model without a schema (`rmp-serde`, `msgpack`, `@msgpack/msgpack`, ...);
//! * structs are encoded as maps, not arrays (`to_vec_named`), which is what makes the payload
//!   self-describing across languages;
//! * errors stay JSON (`{"code": ..., "message": ...}`): a client that asks for MessagePack and
//!   gets a 4xx still reads a body it can log.
//!
//! What is deliberately *not* here yet: quality values in `Accept` and a `format` query parameter.
//! Both are one line away when a real client needs them (see [`Format::from_accept`]).

use axum::body::Body;
use axum::extract::FromRequestParts;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::{BufMut, Bytes, BytesMut};
use serde::Serialize;

/// `Content-Type` of the JSON body.
pub const JSON: &str = "application/json";

/// `Content-Type` of the MessagePack body (`application/x-msgpack` is the older alias).
pub const MSGPACK: &str = "application/msgpack";

/// Which body format a request asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// `application/json`: the v2 contract, and what every existing client reads.
    #[default]
    Json,
    /// `application/msgpack`: the same document, MessagePack-encoded.
    MsgPack,
}

impl Format {
    /// The format `Accept` names, if it names MessagePack at all.
    ///
    /// Only an explicit `application/msgpack` (or its `application/x-msgpack` alias) selects it:
    /// `*/*`, `application/*` and a missing header all mean JSON, so a browser or the gateway can
    /// never be surprised by a binary body.
    pub fn from_accept(accept: Option<&str>) -> Self {
        let Some(accept) = accept else {
            return Self::Json;
        };
        let asked = accept
            .split(',')
            .map(|part| part.split(';').next().unwrap_or("").trim())
            .any(|media_type| {
                media_type.eq_ignore_ascii_case(MSGPACK)
                    || media_type.eq_ignore_ascii_case("application/x-msgpack")
            });
        match asked {
            true => Self::MsgPack,
            false => Self::Json,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Json => JSON,
            Self::MsgPack => MSGPACK,
        }
    }

    /// Encode `value` in this format.
    pub fn encode<T: Serialize>(self, value: T) -> Encoded<T> {
        Encoded {
            format: self,
            value,
        }
    }

    fn to_body<T: Serialize>(self, value: &T) -> Result<Bytes, String> {
        let mut buffer = BytesMut::with_capacity(16 * 1024);
        {
            let mut writer = (&mut buffer).writer();
            match self {
                Self::Json => {
                    serde_json::to_writer(&mut writer, value).map_err(|error| error.to_string())?
                }
                // `write_named` keeps field names: the tuple form would make a struct an array,
                // which any non-Rust client would then have to know the field order of.
                Self::MsgPack => rmp_serde::encode::write_named(&mut writer, value)
                    .map_err(|error| error.to_string())?,
            }
        }
        Ok(buffer.freeze())
    }
}

/// The negotiated format, extracted from `Accept`.
///
/// Taken as a handler argument (`format: Output`) instead of as a middleware so the body is
/// encoded straight from the response type: re-serialising a JSON body into MessagePack would
/// mean parsing it back into a `Value` first.
#[derive(Clone, Copy, Debug, Default)]
pub struct Output(pub Format);

impl<S: Send + Sync> FromRequestParts<S> for Output {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(format_of(&parts.headers)))
    }
}

/// A response body in the requested format.
pub struct Encoded<T> {
    pub format: Format,
    pub value: T,
}

impl<T: Serialize> IntoResponse for Encoded<T> {
    fn into_response(self) -> Response {
        let started = std::time::Instant::now();
        let body = match self.format.to_body(&self.value) {
            Ok(body) => body,
            Err(error) => {
                // Encoding our own response structs cannot fail in practice; if it ever does, a
                // JSON error is more useful than an empty 500.
                tracing::error!(%error, format = ?self.format, "cannot encode the response");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, HeaderValue::from_static(JSON))],
                    format!("cannot encode the response: {error}"),
                )
                    .into_response();
            }
        };
        let mut response = Body::from(body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(self.format.content_type()),
        );
        // The body depends on `Accept`; `accept-encoding` is added by the compression layer.
        headers.insert(header::VARY, HeaderValue::from_static("accept"));
        dl_api::trace::record_work("encode", started.elapsed());
        response
    }
}

/// The `Accept`-negotiated format of a header map.
pub fn format_of(headers: &HeaderMap) -> Format {
    Format::from_accept(
        headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_msgpack_accept_selects_msgpack() {
        assert_eq!(Format::from_accept(None), Format::Json);
        assert_eq!(Format::from_accept(Some("*/*")), Format::Json);
        assert_eq!(Format::from_accept(Some("application/json")), Format::Json);
        assert_eq!(
            Format::from_accept(Some("application/msgpack")),
            Format::MsgPack
        );
        assert_eq!(
            Format::from_accept(Some("application/x-msgpack")),
            Format::MsgPack
        );
        assert_eq!(
            Format::from_accept(Some("text/html, application/msgpack;q=0.9")),
            Format::MsgPack
        );
        assert_eq!(
            Format::from_accept(Some("application/msgpackish")),
            Format::Json
        );
    }

    #[test]
    fn a_msgpack_body_round_trips_to_the_json_value() {
        let value = serde_json::json!({
            "rows": [1, 2, 3],
            "name": "sales",
            "ok": true,
            "none": null
        });
        let packed = Format::MsgPack.to_body(&value).unwrap();
        let back: serde_json::Value = rmp_serde::from_slice(&packed).unwrap();
        assert_eq!(back, value);
        // MessagePack of this document is smaller than its JSON: that is the whole point.
        assert!(packed.len() < Format::Json.to_body(&value).unwrap().len());
    }
}
