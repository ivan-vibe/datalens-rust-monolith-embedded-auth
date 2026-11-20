//! The `us` entry JSON, as `GET /private/entries/{id}` returns it.
//!
//! Only the parts the data path reads are modelled; unknown keys are ignored so a newer `us`
//! can add fields without breaking the service. The shapes come from
//! `dl_us_entries_client.models.entry.Entry` + `models.dataset.DatasetEntry` and from the
//! dataset revision `data` object (`dl_core.us_dataset.Dataset.DataModel`).

use serde::Deserialize;

use crate::error::UsError;

/// `GET /private/entries/{entry_id}` response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsEntry {
    pub entry_id: String,
    #[serde(default)]
    pub rev_id: Option<String>,
    pub scope: String,
    #[serde(default, rename = "type")]
    pub entry_type: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub hidden: bool,
    /// `savedId` / `publishedId`: a connection is often saved but never published, and `us`
    /// then answers `data: null` on the `published` branch (see [`UsClient::get_entry`]).
    #[serde(default)]
    pub saved_id: Option<String>,
    #[serde(default)]
    pub published_id: Option<String>,
    /// The stored object. Its shape depends on `scope`: `Dataset.DataModel` for a dataset, the
    /// connector's `Connection.DataModel` for a connection, so it is kept as JSON and decoded
    /// by [`UsEntry::dataset_data`] / [`UsEntry::connection_data`].
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub unversioned_data: Option<serde_json::Value>,
}

impl UsEntry {
    /// The dataset of a `dataset` entry, or the error `data-api` would return.
    pub fn dataset_data(&self) -> Result<UsDatasetData, UsError> {
        if self.scope != "dataset" {
            return Err(UsError::NotADataset {
                entry_id: self.entry_id.clone(),
                scope: self.scope.clone(),
            });
        }
        self.decode(|raw| UsError::Invalid {
            entry_id: self.entry_id.clone(),
            reason: raw,
        })
    }

    /// The connection of a `connection` entry (`PostgreSQLConnection.DataModel`).
    pub fn connection_data(&self) -> Result<UsConnectionData, UsError> {
        if self.scope != "connection" {
            return Err(UsError::NotAConnection {
                entry_id: self.entry_id.clone(),
                scope: self.scope.clone(),
            });
        }
        self.decode(|raw| UsError::Invalid {
            entry_id: self.entry_id.clone(),
            reason: raw,
        })
    }

    /// `unversioned_data.password`: a fernet-encrypted secret (`dl_core.utils.CryptoKeys`).
    pub fn password_secret(&self) -> Option<&serde_json::Value> {
        self.unversioned_data
            .as_ref()
            .and_then(|data| data.get("password"))
    }

    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        error: impl Fn(String) -> UsError,
    ) -> Result<T, UsError> {
        let raw = self.data.as_ref().ok_or_else(|| UsError::EmptyEntry {
            entry_id: self.entry_id.clone(),
        })?;
        serde_json::from_value(raw.clone()).map_err(|raw| error(raw.to_string()))
    }
}

/// `PostgreSQLConnection.DataModel`: where a dataset's tables live.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsConnectionData {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub db_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    /// `"true"` / `"false"` as strings in the `us` JSON, hence a `Value`. A ClickHouse
    /// connection spells the same idea `secure` (a bool).
    #[serde(default, alias = "secure")]
    pub ssl_enable: Option<serde_json::Value>,
    /// `data.cache_ttl_sec`: the connection's UI "Cache TTL in seconds" setting.
    ///
    /// `Some(0)` disables the query-result cache for this connection, `Some(N)` uses N
    /// seconds and `None` means "no per-connection override" (the data-api fallback applies).
    /// The Python reference reads the same field as
    /// `ConnectionBase.cache_ttl_sec_override` (`us_connection_base.py`).
    #[serde(default)]
    pub cache_ttl_sec: Option<i64>,
}

impl UsConnectionData {
    /// `postgres://user:password@host:port/db`.
    ///
    /// The Rust executor speaks plain TCP (`NoTls`), so `ssl_enable` is not applied yet; a
    /// connection that *requires* TLS is reported by `ssl_required` instead of silently
    /// connecting without it.
    pub fn dsn(&self, password: Option<&str>) -> Option<String> {
        let host = self.host.as_deref()?.trim();
        let db_name = self.db_name.as_deref()?.trim();
        if host.is_empty() || db_name.is_empty() {
            return None;
        }
        let port = self.port.unwrap_or(5432);
        let mut dsn = String::from("postgres://");
        if let Some(username) = self
            .username
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            dsn.push_str(&encode_userinfo(username));
            if let Some(password) = password {
                dsn.push(':');
                dsn.push_str(&encode_userinfo(password));
            }
            dsn.push('@');
        }
        dsn.push_str(host);
        dsn.push(':');
        dsn.push_str(&port.to_string());
        dsn.push('/');
        dsn.push_str(&encode_userinfo(db_name));
        if self.ssl_required() {
            // `tokio-postgres` parses only disable/prefer/require; rustls verifies the
            // certificate chain and hostname for `require`.
            dsn.push_str("?sslmode=require");
        }
        Some(dsn)
    }

    /// `clickhouse://user:password@host:port[/database]`.
    ///
    /// A ClickHouse connection entry may have no `db_name` (the dataset's source can carry
    /// `database.table` instead), so the database is optional: the compiled SQL names the
    /// table as `db.table` and ClickHouse resolves it without a default database.
    pub fn clickhouse_dsn(&self, password: Option<&str>) -> Option<String> {
        let host = self.host.as_deref()?.trim();
        if host.is_empty() {
            return None;
        }
        let port = self.port.unwrap_or(8123);
        let mut dsn = String::from(match self.ssl_required() {
            true => "clickhouses://",
            false => "clickhouse://",
        });
        if let Some(username) = self
            .username
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            dsn.push_str(&encode_userinfo(username));
            if let Some(password) = password {
                dsn.push(':');
                dsn.push_str(&encode_userinfo(password));
            }
            dsn.push('@');
        }
        dsn.push_str(host);
        dsn.push(':');
        dsn.push_str(&port.to_string());
        if let Some(db_name) = self
            .db_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            dsn.push('/');
            dsn.push_str(&encode_userinfo(db_name));
        }
        Some(dsn)
    }

    pub fn ssl_required(&self) -> bool {
        match &self.ssl_enable {
            Some(serde_json::Value::Bool(flag)) => *flag,
            Some(serde_json::Value::String(raw)) => matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "require"
            ),
            _ => false,
        }
    }
}

/// Percent-encode one DSN component (`@`, `:`, `/` and spaces are the ones that break it).
fn encode_userinfo(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `Dataset.DataModel`: the stored dataset.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsDatasetData {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub result_schema: Vec<UsField>,
    #[serde(default)]
    pub source_collections: Vec<UsCollection>,
    #[serde(default)]
    pub source_avatars: Vec<UsAvatar>,
    #[serde(default)]
    pub avatar_relations: Vec<UsAvatarRelation>,
    /// `DatasetContentInternalSchema.data_export_forbidden`.
    #[serde(default)]
    pub data_export_forbidden: bool,
    /// Row-level security rules (`rls`); the query translator turns them into filters.
    #[serde(default)]
    pub rls: dl_core::Rls,
    /// The editor's `rls2` map (`{field_guid: [entries]}`), merged into `rls`.
    #[serde(default)]
    pub rls2: std::collections::BTreeMap<String, Vec<dl_core::RlsEntry>>,
    #[serde(default)]
    pub revision_id: Option<String>,
}

/// One entry of `result_schema` (`dl_core.fields.Field`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsField {
    pub guid: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub hidden: bool,
    /// `DIMENSION` / `MEASURE`.
    #[serde(default, rename = "type")]
    pub field_type: Option<String>,
    /// `direct` / `formula` / `parameter`.
    #[serde(default)]
    pub calc_mode: Option<String>,
    /// For `direct` fields: the physical column name.
    #[serde(default)]
    pub source: String,
    /// For `direct` fields: the avatar the column belongs to.
    #[serde(default)]
    pub avatar_id: Option<String>,
    #[serde(default)]
    pub formula: String,
    #[serde(default)]
    pub guid_formula: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub data_type: Option<String>,
    #[serde(default)]
    pub initial_data_type: Option<String>,
    #[serde(default)]
    pub cast: Option<String>,
    #[serde(default)]
    pub aggregation: Option<String>,
    /// Default of a `parameter` field.
    #[serde(default)]
    pub default_value: Option<UsValue>,
    #[serde(default = "default_true")]
    pub valid: bool,
}

fn default_true() -> bool {
    true
}

/// `TypedValueSchema` / `TypedValue`: `{"type": "string", "value": "sales"}`.
#[derive(Debug, Clone, Deserialize)]
pub struct UsValue {
    #[serde(default, rename = "type")]
    pub data_type: Option<String>,
    #[serde(default)]
    pub value: serde_json::Value,
}

/// One entry of `source_collections` (`DataSourceCollectionSpec`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsCollection {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default, rename = "type")]
    pub source_type: Option<String>,
    #[serde(default)]
    pub origin: Option<UsOrigin>,
    #[serde(default = "default_true")]
    pub valid: bool,
}

/// `DataSourceSpec`: where the source lives.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsOrigin {
    /// `StandardSQLDataSourceSpec`: `table_name` / `schema_name`, plus connection settings.
    #[serde(default)]
    pub parameters: Option<UsParameters>,
    /// The `us` `connection` entry the collection reads from (`gz2tnvb9zdxm5`).
    #[serde(default)]
    pub connection_id: Option<String>,
    /// `DataSourceSpec.raw_schema`: the columns the source has, as the `us` cached them.
    #[serde(default)]
    pub raw_schema: Vec<UsRawColumn>,
}

/// One column of `origin.raw_schema`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsRawColumn {
    /// The physical column name.
    #[serde(default)]
    pub name: String,
    /// The user type (`float`, `date`, ...), the same names the API speaks.
    #[serde(default, rename = "type")]
    pub user_type: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsParameters {
    #[serde(default)]
    pub table_name: Option<String>,
    #[serde(default)]
    pub schema_name: Option<String>,
    #[serde(default)]
    pub db_name: Option<String>,
    /// `SubselectDataSourceSpec.subsql`: the "Custom SQL" text of the source.
    #[serde(default)]
    pub subsql: Option<String>,
}

/// One entry of `source_avatars` (`dl_core.multisource.SourceAvatar`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsAvatar {
    pub id: String,
    #[serde(default)]
    pub title: String,
    /// The `source_collections[].id` this avatar points at.
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub is_root: bool,
    #[serde(default = "default_true")]
    pub valid: bool,
}

/// One entry of `avatar_relations` (`dl_core.multisource.AvatarRelation`).
///
/// The conditions keep the stored shape (`left_part`/`right_part`), which
/// [`dl_core::JoinCondition`] reads through its aliases.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsAvatarRelation {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub left_avatar_id: Option<String>,
    #[serde(default)]
    pub right_avatar_id: Option<String>,
    /// `inner` / `left` / `right` / `full`.
    #[serde(default)]
    pub join_type: Option<String>,
    #[serde(default)]
    pub conditions: Vec<dl_core::JoinCondition>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_parsed_with_the_json_key_names_us_uses() {
        let entry: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "abc",
            "revId": "rev1",
            "scope": "dataset",
            "type": "dataset",
            "key": "demo/name",
            "data": {
                "result_schema": [{"guid": "f1", "title": "A", "type": "MEASURE"}],
                "source_avatars": [{"id": "av1", "source_id": "c1"}],
                "source_collections": [{"id": "c1", "origin": {"parameters": {"table_name": "t"}}}],
            },
        }))
        .unwrap();
        assert_eq!(entry.entry_id, "abc");
        assert_eq!(entry.rev_id.as_deref(), Some("rev1"));
        let data = entry.dataset_data().unwrap();
        assert_eq!(data.result_schema.len(), 1);
        assert_eq!(data.source_avatars[0].source_id.as_deref(), Some("c1"));
    }

    #[test]
    fn a_non_dataset_entry_is_rejected() {
        let entry: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "abc",
            "scope": "connection",
            "data": {},
        }))
        .unwrap();
        let error = entry.dataset_data().unwrap_err();
        assert_eq!(error.status(), 404);
        assert_eq!(error.code(), "ERR.DS_API.DB.DATASET_NOT_FOUND");
    }
}
