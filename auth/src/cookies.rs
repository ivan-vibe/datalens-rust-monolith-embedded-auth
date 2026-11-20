//! `Set-Cookie`/`Cookie` handling with the exact wire format the Node service emits.
//!
//! Observed reference (signin):
//! ```text
//! Set-Cookie: auth={urlencoded JSON {accessToken,refreshToken}}; Max-Age=867600; Path=/;
//!             Expires=...; HttpOnly; SameSite=Strict
//! Set-Cookie: auth_exp={exp}; Max-Age=867600; Path=/; Expires=...; SameSite=Strict
//! ```
//! Attribute order follows the `cookie` npm package (maxAge, domain, path, expires,
//! httpOnly, sameSite, secure) so byte-level diffs stay empty.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthCookie {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
}

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";

/// `encodeURIComponent`, which is what Express `res.cookie` applies to the value.
pub fn encode_cookie_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if UNRESERVED.contains(&byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// `decodeURIComponent`; on malformed input the raw bytes are kept (the Node cookie parser
/// leaves the value undecoded when decoding throws, which then fails JSON parsing the same
/// way it does here).
pub fn decode_cookie_value(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(value) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Cookie header → (name, decoded value) pairs, same splitting rules as `cookie.parse`.
pub fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
    header
        .split(';')
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((name.trim().to_string(), decode_cookie_value(value)))
        })
        .collect()
}

/// Lenient view of the `auth` cookie: the refresh/logout paths only need the refresh token,
/// exactly like the Node code (which reads `authCookie.refreshToken` from arbitrary JSON).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ParsedAuthCookie {
    #[serde(rename = "refreshToken", default)]
    pub refresh_token: Option<String>,
}

/// The parsed `auth` cookie, or `None` when absent/unparseable (Node logs and continues).
pub fn parse_auth_cookie(header: Option<&str>, config: &Config) -> Option<ParsedAuthCookie> {
    let header = header?;
    let value = parse_cookie_header(header)
        .into_iter()
        .find(|(name, _)| name == &config.cookie_name)
        .map(|(_, value)| value)?;
    serde_json::from_str::<ParsedAuthCookie>(&value).ok()
}

fn serialize_cookie(
    name: &str,
    value: &str,
    config: &Config,
    http_only: bool,
    max_age: Option<i64>,
    expires: Option<SystemTime>,
) -> String {
    let mut out = format!("{name}={value}");
    if let Some(max_age) = max_age {
        out.push_str(&format!("; Max-Age={max_age}"));
    }
    if let Some(domain) = &config.cookie_domain {
        out.push_str(&format!("; Domain={domain}"));
    }
    out.push_str("; Path=/");
    if let Some(expires) = expires {
        out.push_str(&format!("; Expires={}", httpdate::fmt_http_date(expires)));
    }
    if http_only {
        out.push_str("; HttpOnly");
    }
    out.push_str(&format!("; SameSite={}", config.cookie_same_site));
    if config.cookie_secure {
        out.push_str("; Secure");
    }
    out
}

/// The two `Set-Cookie` values for a successful signin/refresh.
pub fn auth_set_cookie_headers(
    config: &Config,
    tokens: &AuthCookie,
    access_exp: i64,
) -> Vec<String> {
    let max_age = config.cookie_max_age;
    let expires = SystemTime::now() + Duration::from_secs(max_age.max(0) as u64);
    let payload = serde_json::to_string(tokens).expect("auth cookie is serializable");
    vec![
        serialize_cookie(
            &config.cookie_name,
            &encode_cookie_value(&payload),
            config,
            true,
            Some(max_age),
            Some(expires),
        ),
        serialize_cookie(
            &config.access_token_exp_cookie_name(),
            &access_exp.to_string(),
            config,
            false,
            Some(max_age),
            Some(expires),
        ),
    ]
}

fn serialize_bare_clear(name: &str) -> String {
    // `res.clearCookie(name)` with no options: only path + epoch expiry, no SameSite/HttpOnly.
    format!(
        "{name}=; Path=/; Expires={}",
        httpdate::fmt_http_date(UNIX_EPOCH)
    )
}

/// The four clear headers `res.clearCookie(...)` produces (with and without attributes).
pub fn clear_auth_cookie_headers(config: &Config) -> Vec<String> {
    let epoch = Some(UNIX_EPOCH);
    vec![
        serialize_cookie(&config.cookie_name, "", config, true, None, epoch),
        serialize_cookie(
            &config.access_token_exp_cookie_name(),
            "",
            config,
            false,
            None,
            epoch,
        ),
        serialize_bare_clear(&config.cookie_name),
        serialize_bare_clear(&config.access_token_exp_cookie_name()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            port: 8097,
            dsn: String::new(),
            access_ttl: 900,
            refresh_ttl: 864_000,
            session_ttl: 2_592_000,
            cookie_name: "auth".into(),
            cookie_secure: false,
            cookie_same_site: "Strict".into(),
            cookie_domain: None,
            cookie_max_age: 867_600,
            signup_disabled: true,
            manage_local_users_disabled: false,
            default_role: "datalens.viewer".into(),
            master_tokens: Vec::new(),
            db_pool_size: 1,
        }
    }

    #[test]
    fn percent_encoding_matches_encode_uri_component() {
        let json = r#"{"accessToken":"a-b_c.d~e","refreshToken":"x!"}"#;
        let encoded = encode_cookie_value(json);
        assert_eq!(
            encoded,
            "%7B%22accessToken%22%3A%22a-b_c.d~e%22%2C%22refreshToken%22%3A%22x!%22%7D"
        );
        assert_eq!(decode_cookie_value(&encoded), json);
        // Unreserved characters survive untouched.
        assert_eq!(
            encode_cookie_value("AZaz09-_.!~*'()"),
            "AZaz09-_.!~*'()"
        );
    }

    #[test]
    fn auth_cookie_round_trips_and_invalid_json_is_ignored() {
        let cfg = config();
        let tokens = AuthCookie {
            access_token: "access.token.value".into(),
            refresh_token: "refresh.value".into(),
        };
        let headers = auth_set_cookie_headers(&cfg, &tokens, 1_794_610_311);
        let value = headers[0].split(';').next().unwrap().split_once('=').unwrap().1;
        let header = format!("auth_exp=1794610311; auth={value}");
        let parsed = parse_auth_cookie(Some(&header), &cfg).unwrap();
        assert_eq!(parsed.refresh_token.as_deref(), Some("refresh.value"));
        assert!(parse_auth_cookie(Some("auth=%7Bbad"), &cfg).is_none());
        assert!(parse_auth_cookie(None, &cfg).is_none());
    }

    #[test]
    fn set_cookie_headers_match_reference_attributes() {
        let cfg = config();
        let tokens = AuthCookie {
            access_token: "a".into(),
            refresh_token: "r".into(),
        };
        let headers = auth_set_cookie_headers(&cfg, &tokens, 1_794_610_311);
        assert_eq!(headers.len(), 2);
        assert!(headers[0].starts_with("auth=%7B"));
        assert!(!headers[0].ends_with("Max-Age=867600; Path=/; Expires="));
        assert!(headers[0].contains("; Max-Age=867600; Path=/; Expires="));
        assert!(headers[0].ends_with("; HttpOnly; SameSite=Strict"));
        assert!(headers[1].starts_with("auth_exp=1794610311; Max-Age=867600; Path=/; Expires="));
        assert!(headers[1].ends_with("; SameSite=Strict"));
        assert!(!headers[1].contains("HttpOnly"));
        assert!(!headers[0].contains("Domain="));
    }

    #[test]
    fn clear_headers_match_reference() {
        let headers = clear_auth_cookie_headers(&config());
        assert_eq!(headers.len(), 4);
        assert_eq!(
            headers[0],
            "auth=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT; HttpOnly; SameSite=Strict"
        );
        assert_eq!(
            headers[1],
            "auth_exp=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT; SameSite=Strict"
        );
        assert_eq!(
            headers[2],
            "auth=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
        );
        assert_eq!(
            headers[3],
            "auth_exp=; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT"
        );
    }
}
