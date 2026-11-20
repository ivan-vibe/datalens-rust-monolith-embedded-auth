//! `mysql://user:password@host:port/database` — the DSN `dl_us` builds from a MySQL
//! `connection` entry, parsed without pulling a URL crate into the executor.
//!
//! The user and password are percent-encoded by `dl_us` (the same rule ClickHouse uses), so
//! this module decodes them; everything else (`host`, `port`, `database`) is passed through.
//! `mysqls://` means TLS (`data.ssl_enable`), the spelling the resolver puts in front of the DSN.

use sqlx::mysql::{MySqlConnectOptions, MySqlSslMode};

/// Where one MySQL dataset lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub tls: bool,
}

impl MysqlConfig {
    /// Parse the DSN `dl_us` produced. `mysqls://` means TLS.
    ///
    /// # Errors
    /// Returns [`DsnError`] when the scheme is not MySQL, the host is missing or a component
    /// is not valid percent-encoded UTF-8.
    pub fn from_dsn(dsn: &str) -> Result<Self, DsnError> {
        let (tls, rest) = match dsn.split_once("://") {
            Some(("mysql", rest)) => (false, rest),
            Some(("mysqls", rest)) => (true, rest),
            Some((scheme, _)) => return Err(DsnError::OtherScheme(scheme.to_string())),
            None => return Err(DsnError::NoScheme),
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
            tls,
        })
    }

    /// The sqlx connection settings this config names.
    ///
    /// The custom `data.ssl_ca` PEM of the Python connector has no equivalent here yet:
    /// `sqlx` takes a CA *file* path, and the DSN carries no path, so TLS uses the compiled-in
    /// WebPKI roots. `shortcut: custom MySQL ssl_ca, add a resolver-side CA store when a
    /// connection needs a private CA`.
    pub fn connect_options(&self) -> MySqlConnectOptions {
        let mut options = MySqlConnectOptions::new().host(&self.host).port(self.port);
        if let Some(user) = &self.user {
            options = options.username(user);
        }
        if let Some(password) = &self.password {
            options = options.password(password);
        }
        if let Some(database) = &self.database {
            options = options.database(database);
        }
        if self.tls {
            // Python: `ssl_enable` builds an `ssl.SSLContext` with hostname verification;
            // `Required` is the sqlx spelling that verifies the certificate and the host.
            options = options.ssl_mode(MySqlSslMode::Required);
        }
        options
    }
}

/// A DSN that is not a MySQL DSN.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DsnError {
    #[error("the DSN has no scheme")]
    NoScheme,
    #[error("not a MySQL DSN: {0}")]
    OtherScheme(String),
    #[error("the DSN has no host")]
    NoHost,
    #[error("bad port: {0}")]
    BadPort(String),
    #[error("unterminated IPv6 literal")]
    UnterminatedIpv6,
    #[error("truncated percent-escape")]
    TruncatedEscape,
    #[error("bad percent-escape: {0}")]
    BadEscape(String),
    #[error("invalid utf-8: {0}")]
    InvalidUtf8(String),
}

fn split_host_port(authority: &str) -> Result<(String, u16), DsnError> {
    if let Some(rest) = authority.strip_prefix('[') {
        // `[::1]:3306`
        let (host, rest) = rest.split_once(']').ok_or(DsnError::UnterminatedIpv6)?;
        let port = match rest.strip_prefix(':') {
            Some(port) if !port.is_empty() => port
                .parse::<u16>()
                .map_err(|error| DsnError::BadPort(error.to_string()))?,
            _ => 3306,
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => {
            if host.is_empty() {
                return Err(DsnError::NoHost);
            }
            Ok((
                host.to_string(),
                port.parse::<u16>()
                    .map_err(|error| DsnError::BadPort(error.to_string()))?,
            ))
        }
        None => {
            if authority.is_empty() {
                return Err(DsnError::NoHost);
            }
            Ok((authority.to_string(), 3306))
        }
    }
}

/// `%XX` decoding for the userinfo and database part (`dl_us` encodes everything outside
/// `A-Za-z0-9-_.~`).
fn decode(raw: &str) -> Result<String, DsnError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(DsnError::TruncatedEscape);
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|error| DsnError::BadEscape(error.to_string()))?;
            let byte = u8::from_str_radix(hex, 16)
                .map_err(|error| DsnError::BadEscape(error.to_string()))?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|error| DsnError::InvalidUtf8(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_dsn_is_split_into_its_parts() {
        let config =
            MysqlConfig::from_dsn("mysql://demo:p%40ss%2Fword@mysql.example.com:3307/analytics")
                .unwrap();
        assert_eq!(config.host, "mysql.example.com");
        assert_eq!(config.port, 3307);
        assert_eq!(config.user.as_deref(), Some("demo"));
        assert_eq!(config.password.as_deref(), Some("p@ss/word"));
        assert_eq!(config.database.as_deref(), Some("analytics"));
        assert!(!config.tls);
    }

    #[test]
    fn a_dsn_without_port_user_or_database_is_accepted() {
        let config = MysqlConfig::from_dsn("mysql://db.local").unwrap();
        assert_eq!(config.port, 3306);
        assert!(config.user.is_none());
        assert!(config.database.is_none());
        assert!(!config.tls);
    }

    #[test]
    fn a_secure_dsn_asks_for_tls() {
        let config = MysqlConfig::from_dsn("mysqls://user:secret@db.local:3306/app").unwrap();
        assert!(config.tls);
        assert_eq!(config.port, 3306);
    }

    #[test]
    fn ipv6_literals_keep_their_brackets_off_the_host() {
        let config = MysqlConfig::from_dsn("mysql://user@[::1]:3307/app").unwrap();
        assert_eq!(config.host, "::1");
        assert_eq!(config.port, 3307);
    }

    #[test]
    fn another_scheme_and_a_missing_host_are_refused() {
        assert_eq!(
            MysqlConfig::from_dsn("postgres://host/db").unwrap_err(),
            DsnError::OtherScheme("postgres".to_string())
        );
        assert_eq!(
            MysqlConfig::from_dsn("mysql://:3306/db").unwrap_err(),
            DsnError::NoHost
        );
        assert_eq!(
            MysqlConfig::from_dsn("db.local:3306").unwrap_err(),
            DsnError::NoScheme
        );
    }

    #[test]
    fn a_bad_escape_is_an_error_not_garbage() {
        assert_eq!(
            MysqlConfig::from_dsn("mysql://user:p%2@host/db").unwrap_err(),
            DsnError::TruncatedEscape
        );
    }
}
