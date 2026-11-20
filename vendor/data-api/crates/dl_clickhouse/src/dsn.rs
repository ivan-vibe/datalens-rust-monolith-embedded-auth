//! `clickhouse://user:password@host:port/database` — the DSN `dl_us` builds from a `connection`
//! entry, parsed without pulling a URL crate into the executor.
//!
//! The user and password are percent-encoded by `dl_us::entry::encode_userinfo`, so this module
//! decodes them; everything else (`host`, `port`, `database`) is passed through.

use std::collections::BTreeMap;

/// Where one ClickHouse dataset lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickHouseConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub secure: bool,
}

impl ClickHouseConfig {
    /// Parse the DSN `dl_us` produced. `clickhouses://` means TLS.
    pub fn from_dsn(dsn: &str) -> Result<Self, String> {
        let (secure, rest) = match dsn.split_once("://") {
            Some(("clickhouse", rest)) => (false, rest),
            Some(("clickhouses", rest)) => (true, rest),
            Some((scheme, _)) => return Err(format!("not a ClickHouse DSN: {scheme}")),
            None => return Err("the DSN has no scheme".to_string()),
        };
        // `user:password@` is everything before the last `@`: an encoded password may contain
        // `%40`, never a raw `@`, but a host cannot contain one at all.
        let (userinfo, hostpart) = match rest.rsplit_once('@') {
            Some((userinfo, hostpart)) => (Some(userinfo), hostpart),
            None => (None, rest),
        };
        let (user, password) = match userinfo {
            Some(userinfo) => match userinfo.split_once(':') {
                Some((user, password)) => (Some(decode(user)?), Some(decode(password)?)),
                None => (Some(decode(userinfo)?), None),
            },
            None => (None, None),
        };
        let (authority, database) = match hostpart.split_once('/') {
            Some((authority, database)) if !database.is_empty() => {
                (authority, Some(decode(database)?))
            }
            Some((authority, _)) => (authority, None),
            None => (hostpart, None),
        };
        let (host, port) = split_host_port(authority)?;
        Ok(Self {
            host,
            port,
            user,
            password,
            database,
            secure,
        })
    }

    /// The base URL of the HTTP interface.
    pub fn base_url(&self) -> String {
        let scheme = match self.secure {
            true => "https",
            false => "http",
        };
        // An IPv6 literal has to be bracketed in a URL.
        let host = match self.host.contains(':') && !self.host.starts_with('[') {
            true => format!("[{}]", self.host),
            false => self.host.clone(),
        };
        format!("{scheme}://{host}:{}/", self.port)
    }

    /// The query the Python adapter sends with every statement.
    ///
    /// `join_use_nulls=1` is what makes a `LEFT JOIN` produce `NULL`s instead of default values,
    /// `readonly=2` forbids writes and DDL, and `default_format=JSONCompact` is the document
    /// [`crate::parse_json_compact`] reads.
    pub fn params(&self) -> BTreeMap<String, String> {
        let mut params = BTreeMap::new();
        params.insert("join_use_nulls".to_string(), "1".to_string());
        params.insert("readonly".to_string(), "2".to_string());
        params.insert("send_progress_in_http_headers".to_string(), "0".to_string());
        params.insert("default_format".to_string(), "JSONCompact".to_string());
        if let Some(database) = &self.database {
            params.insert("database".to_string(), database.clone());
        }
        params
    }

    pub fn url(&self) -> String {
        self.base_url()
    }
}

fn split_host_port(authority: &str) -> Result<(String, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        // `[::1]:9000`
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| "unterminated IPv6 literal".to_string())?;
        let port = match rest.strip_prefix(':') {
            Some(port) if !port.is_empty() => port
                .parse::<u16>()
                .map_err(|error| format!("bad port: {error}"))?,
            _ => 8123,
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => Ok((
            host.to_string(),
            port.parse::<u16>()
                .map_err(|error| format!("bad port: {error}"))?,
        )),
        _ => {
            if authority.is_empty() {
                return Err("the DSN has no host".to_string());
            }
            Ok((authority.to_string(), 8123))
        }
    }
}

/// `%XX` decoding for the userinfo part (`dl_us::entry::encode_userinfo` encodes everything
/// outside `A-Za-z0-9-_.~`).
fn decode(raw: &str) -> Result<String, String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err("truncated percent-escape".to_string());
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|error| format!("bad percent-escape: {error}"))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|error| format!("bad percent-escape: {error}"))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|error| format!("invalid utf-8: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_dsn_is_split_into_its_parts() {
        let config = ClickHouseConfig::from_dsn(
            "clickhouse://demo:p%40ss%2Fword@clickhouse.example.com:4123/analytics",
        )
        .unwrap();
        assert_eq!(config.host, "clickhouse.example.com");
        assert_eq!(config.port, 4123);
        assert_eq!(config.user.as_deref(), Some("demo"));
        assert_eq!(config.password.as_deref(), Some("p@ss/word"));
        assert_eq!(config.database.as_deref(), Some("analytics"));
        assert!(!config.secure);
        assert_eq!(config.base_url(), "http://clickhouse.example.com:4123/");
        let params = config.params();
        assert_eq!(
            params.get("database").map(String::as_str),
            Some("analytics")
        );
    }

    #[test]
    fn a_dsn_without_database_or_password_is_accepted() {
        let config = ClickHouseConfig::from_dsn("clickhouse://user@db.local:9000").unwrap();
        assert_eq!(config.port, 9000);
        assert_eq!(config.user.as_deref(), Some("user"));
        assert!(config.password.is_none());
        assert!(config.database.is_none());
        assert!(!config.params().contains_key("database"));
    }

    #[test]
    fn a_secure_dsn_uses_https() {
        let config = ClickHouseConfig::from_dsn("clickhouses://db.local").unwrap();
        assert!(config.secure);
        assert_eq!(config.port, 8123);
        assert_eq!(config.base_url(), "https://db.local:8123/");
    }

    #[test]
    fn another_scheme_is_refused() {
        assert!(ClickHouseConfig::from_dsn("postgres://host/db").is_err());
        assert!(ClickHouseConfig::from_dsn("host:9000").is_err());
    }
}
