//! Connector catalog and `connection` entry <-> control-api payload transforms.
//!
//! The Python control-api builds these payloads from per-connector Python classes
//! (`GenericConnectionSchema`, `ConnectionOptionsSchema`, `is_export_import_allowed`).
//! The native monolith has no connector plugins, so this module keeps the small static
//! catalog the UI actually reads (`/info/connectors`, dataset `options`) and a generic
//! connection serialiser: every connector-specific key that the US entry already stores is
//! passed through, and only the known derived keys are computed.
//!
//! Golden answers were captured from the Python reference (`:28183`); the transform tests
//! below pin the shapes that the UI consumes.
//!
//! shortcut: connector capability data (compatible source types, schema-update support,
//! source listing flags, export/import allow-list) is a static approximation of the
//! Postgres/ClickHouse-enabled reference deployment; wire it to connector settings when the
//! native cluster starts serving connectors that need different capabilities.

use serde_json::{json, Map, Value};

/// One UI-visible connector (`/api/v1/info/connectors`).
pub struct ConnectorSpec {
    pub conn_type: &'static str,
    pub alias: &'static str,
    pub title: &'static str,
    /// `DataSource.supports_schema_update`: the dataset editor offers "refresh schema".
    pub supports_schema_update: bool,
    /// `DataSourceType`s this connector can back.
    pub source_types: &'static [&'static str],
    /// The connector stores a password/token secret, so an export asks the user to re-save it.
    pub has_secret: bool,
}

/// The connectors enabled in the reference Python deployment, in `/info/connectors` order.
pub const CONNECTORS: [ConnectorSpec; 11] = [
    ConnectorSpec {
        conn_type: "clickhouse",
        alias: "clickhouse",
        title: "ClickHouse",
        supports_schema_update: true,
        source_types: &["CH_TABLE", "CH_SUBSELECT"],
        has_secret: true,
    },
    ConnectorSpec {
        conn_type: "postgres",
        alias: "postgres",
        title: "PostgreSQL",
        supports_schema_update: true,
        source_types: &["PG_TABLE", "PG_SUBSELECT"],
        has_secret: true,
    },
    ConnectorSpec {
        conn_type: "chyt",
        alias: "chyt",
        title: "YTsaurus CHYT",
        supports_schema_update: true,
        source_types: &["CHYT_TABLE", "CHYT_SUBSELECT"],
        has_secret: false,
    },
    ConnectorSpec {
        conn_type: "ydb",
        alias: "ydb",
        title: "YDB",
        supports_schema_update: true,
        source_types: &["YDB_TABLE", "YDB_SUBSELECT"],
        has_secret: false,
    },
    ConnectorSpec {
        conn_type: "mysql",
        alias: "mysql",
        title: "MySQL",
        supports_schema_update: true,
        source_types: &["MYSQL_TABLE", "MYSQL_SUBSELECT"],
        has_secret: true,
    },
    ConnectorSpec {
        conn_type: "greenplum",
        alias: "greenplum",
        title: "Greenplum",
        supports_schema_update: true,
        source_types: &["GP_TABLE", "GP_SUBSELECT"],
        has_secret: true,
    },
    ConnectorSpec {
        conn_type: "mssql",
        alias: "mssql",
        title: "MS SQL Server",
        supports_schema_update: true,
        source_types: &["MSSQL_TABLE", "MSSQL_SUBSELECT"],
        has_secret: true,
    },
    ConnectorSpec {
        conn_type: "oracle",
        alias: "oracle",
        title: "Oracle Database",
        supports_schema_update: true,
        source_types: &["ORACLE_TABLE", "ORACLE_SUBSELECT"],
        has_secret: true,
    },
    ConnectorSpec {
        conn_type: "trino",
        alias: "trino",
        title: "Trino",
        supports_schema_update: true,
        source_types: &["TRINO_TABLE", "TRINO_SUBSELECT"],
        has_secret: true,
    },
    ConnectorSpec {
        conn_type: "metrika_api",
        alias: "metrica",
        title: "Metrica",
        supports_schema_update: false,
        source_types: &["METRIKA_API"],
        has_secret: false,
    },
    ConnectorSpec {
        conn_type: "appmetrica_api",
        alias: "appmetrica",
        title: "AppMetrica",
        supports_schema_update: false,
        source_types: &["APPMETRICA_API"],
        has_secret: false,
    },
];

/// Golden `options.connections.items[].replacement_types` order (`ConnectionType`).
pub const REPLACEMENT_TYPES: [&str; 11] = [
    "oracle",
    "clickhouse",
    "greenplum",
    "postgres",
    "mssql",
    "chyt",
    "mysql",
    "metrika_api",
    "trino",
    "ydb",
    "appmetrica_api",
];

const CHECK_CREDENTIALS_MESSAGE: &str =
    "After importing, it is necessary to change and re-save secret fields such as passwords and tokens";

/// `ERR.DS_API.UNSUPPORTED`: `UnsupportedForEntityType` in `dl_api_lib.exc`.
pub const UNSUPPORTED_CODE: &str = "ERR.DS_API.UNSUPPORTED";

/// `ERR.DS_API.WB_IMPORT.CONN`: `ConnectionImportError`.
pub const CONNECTION_IMPORT_CODE: &str = "ERR.DS_API.WB_IMPORT.CONN";

pub fn spec(conn_type: &str) -> Option<&'static ConnectorSpec> {
    CONNECTORS.iter().find(|spec| spec.conn_type == conn_type)
}

/// Connectors that may be exported/imported (`is_export_import_allowed`).
pub fn is_export_import_allowed(conn_type: &str) -> bool {
    spec(conn_type).is_some()
}

pub fn supports_schema_update(conn_type: Option<&str>) -> bool {
    conn_type
        .and_then(spec)
        .map(|spec| spec.supports_schema_update)
        .unwrap_or(false)
}

pub fn source_types(conn_type: Option<&str>) -> &'static [&'static str] {
    conn_type.and_then(spec).map(|spec| spec.source_types).unwrap_or(&[])
}

/// `GET /api/v1/info/connectors`. The reference deployment has no connector sections.
pub fn connectors_payload() -> Value {
    let items: Vec<Value> = CONNECTORS
        .iter()
        .map(|spec| {
            json!({
                "visibility_mode": "free",
                "alias": spec.alias,
                "conn_type": spec.conn_type,
                "title": spec.title,
                "backend_driven_form": true,
                "history": true,
            })
        })
        .collect();
    json!({"uncategorized": items, "sections": []})
}

/// `GET /api/v1/info/connectors/icons`: the reference serves the full SVG catalog.
/// Captured from the Python control-api and embedded so the native endpoint is equivalent.
const ICONS_JSON: &str = include_str!("connector_icons.json");

fn icon_catalog() -> &'static Value {
    static CATALOG: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| serde_json::from_str(ICONS_JSON).unwrap_or_else(|_| json!({"icons": []})))
}

pub fn icons_payload() -> Value {
    icon_catalog().clone()
}

pub fn icon_payload(conn_type: &str) -> Value {
    let icon = icon_catalog()
        .get("icons")
        .and_then(Value::as_array)
        .and_then(|icons| {
            icons
                .iter()
                .find(|icon| icon.get("conn_type").and_then(Value::as_str) == Some(conn_type))
        })
        .cloned()
        .unwrap_or(Value::Null);
    json!({"icon": icon})
}

/// `ConnectionOptionsSchema().dump(conn.get_options())`.
///
/// Only the flags the dataset/connection editor reads are computed; the query-type list is
/// the generic one the reference returns for the SQL connectors in this deployment.
pub fn connection_options(conn_type: &str, raw_sql_level: &str) -> Value {
    let dashsql_allowed = matches!(raw_sql_level, "subselect" | "template" | "dashsql");
    let typed_query_allowed = matches!(conn_type, "clickhouse" | "chyt" | "ydb");
    json!({
        "query_types": [{
            "allow_selector": false,
            "query_type_label": "Generic Query",
            "query_type": "generic_query",
            "required_parameters": [],
        }],
        "allow_dashsql_usage": dashsql_allowed,
        "allow_typed_query_usage": typed_query_allowed,
        "allow_dataset_usage": true,
    })
}

/// `Connection.get_listing_options()` (`options.source_listing`).
pub fn listing_options(conn_type: Option<&str>) -> Value {
    // Golden `info/source_listing_options`: Postgres searches/paginates, ClickHouse does not.
    let sql_like = conn_type
        .map(|value| matches!(value, "postgres" | "greenplum" | "mssql" | "oracle" | "trino" | "mysql" | "ydb" | "chyt"))
        .unwrap_or(false);
    json!({
        "db_name_required_for_search": false,
        "supports_db_name_listing": false,
        "supports_source_search": sql_like,
        "db_name_label": Value::Null,
        "supports_source_pagination": sql_like,
    })
}

/// Keys the US connection entry keeps internally and the API does not dump.
const INTERNAL_KEYS: [&str; 5] = ["name", "ssl_ca", "table_name", "schema_version", "sample_table_name"];

fn entry_data(entry: &Value) -> &Map<String, Value> {
    match entry.get("data").and_then(Value::as_object) {
        Some(data) => data,
        None => empty_map(),
    }
}

fn empty_map() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

fn key_name(entry: &Value) -> &str {
    entry
        .get("key")
        .and_then(Value::as_str)
        .and_then(|key| key.rsplit('/').next())
        .unwrap_or("")
}

fn annotation_description(entry: &Value) -> &str {
    entry
        .pointer("/annotation/description")
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Copy one connection-data key into an API payload, folding the Python `on`/`off` enums
/// (`SSLEnable`, `DataExportForbidden`) back to the strings the schema dumps.
fn insert_api_value(out: &mut Map<String, Value>, key: &str, value: &Value) {
    match (key, value) {
        ("ssl_enable" | "data_export_forbidden", Value::Bool(enabled)) => {
            out.insert(key.to_string(), json!(if *enabled { "on" } else { "off" }));
        }
        // Legacy demo entries store the boolean as the strings "true"/"false"; the reference
        // schema parses them into the on/off enum and dumps `off`, so fold them here too.
        ("ssl_enable" | "data_export_forbidden", Value::String(text)) => {
            let normalized = match text.as_str() {
                "true" => "on",
                "false" => "off",
                other => other,
            };
            out.insert(key.to_string(), json!(normalized));
        }
        _ => {
            out.insert(key.to_string(), value.clone());
        }
    }
}

/// `GET /api/v1/connections/{id}`: `GenericConnectionSchema.dump()` plus options.
pub fn connection_get_payload(entry: &Value) -> Value {
    let data = entry_data(entry);
    let mut out = Map::with_capacity(data.len() + 8);
    for (key, value) in data {
        if INTERNAL_KEYS.contains(&key.as_str()) || key == "password" {
            continue;
        }
        insert_api_value(&mut out, key, value);
    }
    let conn_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let raw_sql_level = data.get("raw_sql_level").and_then(Value::as_str).unwrap_or("off");
    out.insert("id".to_string(), entry.get("entryId").cloned().unwrap_or(Value::Null));
    out.insert("db_type".to_string(), json!(conn_type));
    out.insert("created_at".to_string(), entry.get("createdAt").cloned().unwrap_or(Value::Null));
    out.insert("updated_at".to_string(), entry.get("updatedAt").cloned().unwrap_or(Value::Null));
    out.insert("workbook_id".to_string(), entry.get("workbookId").cloned().unwrap_or(Value::Null));
    out.insert(
        "description".to_string(),
        json!(match data.get("description") {
            Some(Value::String(text)) => text.as_str(),
            _ => annotation_description(entry),
        }),
    );
    out.insert(
        "name".to_string(),
        json!(match data.get("name") {
            Some(Value::String(text)) if !text.is_empty() => text.as_str(),
            _ => key_name(entry),
        }),
    );
    out.insert("key".to_string(), entry.get("key").cloned().unwrap_or(Value::Null));
    out.insert(
        "meta".to_string(),
        entry.get("meta").cloned().unwrap_or_else(|| json!({})),
    );
    out.insert("db_type".to_string(), json!(conn_type));
    out.insert("options".to_string(), connection_options(conn_type, raw_sql_level));
    Value::Object(out)
}

/// `GET /api/v1/connections/export/{id}`: the portable connection with secrets masked.
pub fn connection_export_payload(entry: &Value) -> Value {
    let conn_type = entry.get("type").and_then(Value::as_str).unwrap_or("");
    let Some(spec) = spec(conn_type) else {
        return json!({"notifications": [{
            "message": format!("Connector {conn_type} is not available for export/import in current environment"),
            "level": "critical",
            "code": UNSUPPORTED_CODE,
        }]});
    };
    let mut connection = connection_get_payload(entry);
    let Some(object) = connection.as_object_mut() else {
        return json!({"notifications": []});
    };
    for key in ["id", "created_at", "updated_at", "key", "meta", "options"] {
        object.remove(key);
    }
    if spec.has_secret {
        object.insert("password".to_string(), json!("******"));
    }
    object.insert("db_type".to_string(), json!(spec.conn_type));
    let notifications = if spec.has_secret {
        json!([{
            "code": "NOTIF.WB_EXPORT.CONN.CHECK_CREDENTIALS",
            "level": "info",
            "message": CHECK_CREDENTIALS_MESSAGE,
        }])
    } else {
        json!([])
    };
    json!({"connection": connection, "notifications": notifications})
}

/// The connection fields of an API create/edit body -> the `us` `data` object.
///
/// `password` never belongs in `data` (it is stored in `unversionedData`); the on/off strings
/// of the create form go back to the booleans the US entry keeps.
pub fn connection_stored_data(conn_type: &str, source: &Map<String, Value>) -> Map<String, Value> {
    let mut raw = Map::with_capacity(source.len() + 5);
    for (key, value) in source {
        match (key.as_str(), value) {
            ("type" | "name" | "description" | "workbook_id" | "collection_id" | "db_type", _) => {}
            ("ssl_enable" | "data_export_forbidden", Value::String(text)) => {
                raw.insert(key.clone(), json!(text == "on"));
            }
            ("password", _) => {}
            _ => {
                raw.insert(key.clone(), value.clone());
            }
        }
    }
    // The US storage schema always carries these connector fields, even when unset.
    raw.entry("name".to_string()).or_insert(Value::Null);
    raw.entry("ssl_ca".to_string()).or_insert(json!("null"));
    raw.entry("table_name".to_string()).or_insert(Value::Null);
    raw.entry("sample_table_name".to_string()).or_insert(Value::Null);
    raw.entry("schema_version".to_string()).or_insert(json!("1"));
    if conn_type == "postgres" || conn_type == "greenplum" {
        raw.entry("enforce_collate".to_string()).or_insert(json!("auto"));
        raw.entry("raw_sql_level".to_string()).or_insert(json!("off"));
    }
    raw
}

fn connection_secret(password: &str) -> Result<Value, String> {
    // Store the same fernet envelope Python's control-api uses. Production requires a
    // configured crypto key; only unit tests keep the plain fallback for fixtures.
    let key_id = std::env::var("DL_CRY_ACTUAL_KEY_ID").unwrap_or_else(|_| "KEY".to_string());
    let key = std::env::var("DL_CRY_KEY_VAL_ID_KEY")
        .or_else(|_| std::env::var("CONTROL_API_CRYPTO_KEY"))
        .ok()
        .and_then(|value| dl_us::FernetKey::from_base64(&value));
    if let Some(key) = key {
        if let Ok(token) = key.encrypt(password) {
            return Ok(json!({"password": {
                "key_kind": "local_fernet",
                "key_id": key_id,
                "cypher_text": token,
            }}));
        }
    }
    #[cfg(test)]
    {
        return Ok(json!({"password": {"key_kind": "plain", "key_id": "", "cypher_text": password}}));
    }
    #[cfg(not(test))]
    {
        Err("connection secrets require DL_CRY_KEY_VAL_ID_KEY".to_string())
    }
}

/// `POST /api/v1/connections`: create-form body -> the US `connection` entry create body.
pub fn connection_create_body(body: &Value) -> Result<Value, String> {
    let object = body.as_object().ok_or("Unexpected request schema")?;
    let conn_type = object.get("type").and_then(Value::as_str).unwrap_or("");
    if conn_type.is_empty() {
        return Err("Invalid connection type value: {}".to_string());
    }
    if !is_export_import_allowed(conn_type) {
        return Err(format!("Invalid connection type value: {conn_type}"));
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(conn_type);
    let description = object.get("description").and_then(Value::as_str).unwrap_or("");
    let data = connection_stored_data(conn_type, object);

    let mut create = Map::with_capacity(10);
    if let Some(workbook_id) = object.get("workbook_id").filter(|value| !value.is_null()) {
        create.insert("workbookId".to_string(), workbook_id.clone());
    } else if let Some(collection_id) = object.get("collection_id").filter(|value| !value.is_null()) {
        create.insert("collectionId".to_string(), collection_id.clone());
    }
    create.insert("name".to_string(), json!(name));
    create.insert("scope".to_string(), json!("connection"));
    create.insert("type".to_string(), json!(conn_type));
    create.insert("mode".to_string(), json!("publish"));
    create.insert("links".to_string(), json!({}));
    create.insert("meta".to_string(), json!({"state": "saved"}));
    create.insert("data".to_string(), Value::Object(data));
    if let Some(password) = object.get("password").and_then(Value::as_str) {
        if !password.is_empty() && password != "******" {
            create.insert("unversionedData".to_string(), connection_secret(password)?);
        }
    }
    create.insert("annotation".to_string(), json!({"description": description}));
    Ok(Value::Object(create))
}

/// `PUT /api/v1/connections/{id}`: edit-form changes -> the US `connection` entry update body.
pub fn connection_update_body(entry: &Value, changes: &Value) -> Result<Value, String> {
    let Some(changes) = changes.as_object() else {
        return Err("Unexpected request schema".to_string());
    };
    let existing = entry.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut merged = existing.clone();
    for (key, value) in changes {
        match key.as_str() {
            "type" | "name" | "description" | "workbook_id" | "collection_id" | "db_type" | "password" => {}
            "ssl_enable" | "data_export_forbidden" => {
                let bool_value = match value {
                    Value::String(text) => json!(text == "on"),
                    other => other.clone(),
                };
                merged.insert(key.clone(), bool_value);
            }
            _ => {
                merged.insert(key.clone(), value.clone());
            }
        }
    }
    let mut update = Map::with_capacity(4);
    update.insert("data".to_string(), Value::Object(merged));
    update.insert("mode".to_string(), json!("publish"));
    if let Some(description) = changes.get("description").and_then(Value::as_str) {
        update.insert("annotation".to_string(), json!({"description": description}));
    }
    match changes.get("password").and_then(Value::as_str) {
        Some(password) if !password.is_empty() && password != "******" => {
            update.insert("unversionedData".to_string(), connection_secret(password)?);
        }
        _ => {
            if let Some(unversioned) = entry.get("unversionedData").filter(|value| !value.is_null()) {
                update.insert("unversionedData".to_string(), unversioned.clone());
            }
        }
    }
    Ok(Value::Object(update))
}

/// `POST /api/v1/connections/import`: an exported connection -> the US `connection` entry
/// create body (`data` in the storage shape).
pub fn connection_import_body(body: &Value) -> Result<Value, String> {
    let data = body.get("data").ok_or("Missing data for required field: data")?;
    let connection = data
        .get("connection")
        .and_then(Value::as_object)
        .ok_or("Missing data for required field: connection")?;
    let db_type = connection
        .get("db_type")
        .and_then(Value::as_str)
        .ok_or("Missing data for required field: db_type")?;
    if !is_export_import_allowed(db_type) {
        return Err(format!(
            "Not a valid connection type for this environment: {db_type}"
        ));
    }

    let name = connection
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(db_type);
    let description = connection
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut source = connection.clone();
    source.insert("type".to_string(), json!(db_type));
    let raw = connection_stored_data(db_type, &source);

    let mut create = Map::with_capacity(8);
    if let Some(workbook_id) = data.get("workbook_id").filter(|value| !value.is_null()) {
        create.insert("workbookId".to_string(), workbook_id.clone());
    }
    create.insert("name".to_string(), json!(name));
    create.insert("scope".to_string(), json!("connection"));
    create.insert("type".to_string(), json!(db_type));
    create.insert("mode".to_string(), json!("publish"));
    create.insert("links".to_string(), json!({}));
    create.insert("meta".to_string(), json!({"state": "saved"}));
    create.insert("data".to_string(), Value::Object(raw));
    create.insert("annotation".to_string(), json!({"description": description}));
    Ok(Value::Object(create))
}

// ---------------------------------------------------------------------------------------------
// Connection editor: test / introspection
//
// The Python control-api asks the connector plugins: test_connection opens a real session,
// `/info/sources` lists tables, `/info/source/schema` reads a schema, `/db_names` lists
// databases. The native monolith has Postgres, ClickHouse and MySQL executors for query
// execution; for the connection editor only Postgres is wired here (the demo deployment's
// tables live there). The other connectors answer with the same 4xx envelope Python uses for
// a failed connection instead of inventing a response.
//
// A connection's DSN is resolved through `dl_us`: the US entry carries the connector data and
// the fernet-encrypted password, and `DL_CONNECTION_HOST_MAP` rewrites compose service names
// to host ports (the monolith runs on the host, not in the compose network).
// ---------------------------------------------------------------------------------------------

use std::time::Duration;

use dl_api::Executor as _;
use dl_us::{ConnectorKind, ConnectionSecrets, FernetKey, UsClient, UsConnection, UsConnectionCache, UsEntry};

use crate::control_schemas;

/// `ERR.DS_API.DB.SOURCE_CONNECT_ERROR`: the connector refused / could not reach the database.
pub const DB_SOURCE_CONNECT_ERROR: &str = "ERR.DS_API.DB.SOURCE_CONNECT_ERROR";
/// `ERR.DS_API.UNSUPPORTED`: operation not available for this connector.
pub const UNSUPPORTED_ENTITY_CODE: &str = "ERR.DS_API.UNSUPPORTED";
/// `ERR.DS_API.BAD_CONN_TYPE`: `/info/connectors/forms/{unknown}`.
pub const BAD_CONN_TYPE_CODE: &str = "ERR.DS_API.BAD_CONN_TYPE";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const UNSUPPORTED_DB_MESSAGE: &str =
    "The native control-api has no executor for this connector; only PostgreSQL connection introspection is wired.";

fn crypto_secrets() -> ConnectionSecrets {
    let key_id = std::env::var("DL_CRY_ACTUAL_KEY_ID").ok();
    let key = std::env::var("DL_CRY_KEY_VAL_ID_KEY")
        .or_else(|_| std::env::var("CONTROL_API_CRYPTO_KEY"))
        .ok()
        .and_then(|value| FernetKey::from_base64(&value));
    ConnectionSecrets::new(key_id, key)
}

fn us_client() -> Result<UsClient, String> {
    // `resolve` never calls the client; it only needs the type for the cache.
    UsClient::new("http://127.0.0.1").map_err(|error| error.to_string())
}

/// A US `connection` entry (with `unversionedData`) -> the database DSN + connector kind.
pub fn resolved_connection(entry: &Value) -> Result<UsConnection, String> {
    let parsed: UsEntry = serde_json::from_value(entry.clone()).map_err(|error| error.to_string())?;
    let client = us_client()?;
    let cache = UsConnectionCache::new(client, Duration::ZERO).with_secrets(crypto_secrets());
    cache
        .resolve(&parsed.entry_id, &parsed)
        .map_err(|error| error.to_string())
}

/// A create-form connection body -> the database DSN (`test_connection_params`).
pub fn resolved_connection_params(body: &Value) -> Result<UsConnection, String> {
    let object = body.as_object().ok_or("Unexpected request schema")?;
    let conn_type = object.get("type").and_then(Value::as_str).unwrap_or("");
    if conn_type.is_empty() {
        return Err("Missing data for required field: type".to_string());
    }
    let mut data = Map::new();
    let mut password = None;
    for (key, value) in object {
        match key.as_str() {
            "type" | "name" | "description" => {}
            "password" => match value.as_str() {
                Some("******") | None => {}
                Some(text) => password = Some(text),
            },
            _ => {
                data.insert(key.clone(), value.clone());
            }
        }
    }
    let unversioned = match password {
        Some(password) => json!({"password": password}),
        None => Value::Null,
    };
    let entry = json!({
        "entryId": "test-connection",
        "scope": "connection",
        "type": conn_type,
        "data": data,
        "unversionedData": unversioned,
    });
    resolved_connection(&entry)
}

fn requires_postgres(connection: &UsConnection) -> Result<(), String> {
    match connection.kind {
        ConnectorKind::Postgres => Ok(()),
        _ => Err(UNSUPPORTED_DB_MESSAGE.to_string()),
    }
}

/// Open and immediately close a session (`POST /connections/test_connection`).
pub async fn test_connection(connection: &UsConnection) -> Result<(), String> {
    requires_postgres(connection)?;
    let executor = tokio::time::timeout(
        CONNECT_TIMEOUT,
        dl_postgres::PostgresExecutor::connect(&connection.dsn),
    )
    .await
    .map_err(|_| "PostgreSQL: connection timed out".to_string())?
    .map_err(|error| error.to_string())?;
    drop(executor);
    Ok(())
}

async fn postgres_fetch(connection: &UsConnection, sql: &str) -> Result<Vec<Vec<Option<String>>>, String> {
    requires_postgres(connection)?;
    let executor = tokio::time::timeout(
        CONNECT_TIMEOUT,
        dl_postgres::PostgresExecutor::connect(&connection.dsn),
    )
    .await
    .map_err(|_| "PostgreSQL: connection timed out".to_string())?
    .map_err(|error| error.to_string())?;
    tokio::time::timeout(CONNECT_TIMEOUT, executor.fetch(sql, None))
        .await
        .map_err(|_| "PostgreSQL: query timed out".to_string())?
        .map_err(|error| error.to_string())
}

async fn postgres_fetch_meta(
    connection: &UsConnection,
    sql: &str,
) -> Result<(Vec<Vec<Option<String>>>, dl_api::QueryMetadata), String> {
    requires_postgres(connection)?;
    let executor = tokio::time::timeout(
        CONNECT_TIMEOUT,
        dl_postgres::PostgresExecutor::connect(&connection.dsn),
    )
    .await
    .map_err(|_| "PostgreSQL: connection timed out".to_string())?
    .map_err(|error| error.to_string())?;
    tokio::time::timeout(CONNECT_TIMEOUT, executor.fetch_with_meta(sql, None))
        .await
        .map_err(|_| "PostgreSQL: query timed out".to_string())?
        .map_err(|error| error.to_string())
}

fn sql_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// `PostgreSQL` `udt_name` -> the DataLens user type (`dl_connector_postgresql` type map).
pub fn postgres_user_type(udt_name: &str) -> &'static str {
    match udt_name.trim().to_ascii_lowercase().as_str() {
        "bool" | "boolean" => "boolean",
        "int2" | "int4" | "int8" | "smallint" | "integer" | "bigint" | "serial" | "bigserial" => "integer",
        "numeric" | "decimal" | "float4" | "float8" | "real" | "double precision" => "float",
        "timestamp" | "timestamptz" | "timestamp without time zone" | "timestamp with time zone" => {
            "genericdatetime"
        }
        "date" => "date",
        "uuid" => "uuid",
        "geometry" | "geography" => "geopolygon",
        "json" | "jsonb" | "text" | "varchar" | "character varying" | "bpchar" | "char" | "name"
        | "citext" => "string",
        _ => "string",
    }
}

/// `PostgreSQL` `udt_name` -> the *normalized* native type name the Python connector stores
/// in `raw_schema` (`int4` -> `integer`, `timestamptz` -> `timestamp`, `bool` -> `boolean`,
/// `varchar` -> `text`), i.e. the name a round-trip through the connector produces.
pub fn postgres_native_type_name(udt_name: &str) -> &'static str {
    match udt_name.trim().to_ascii_lowercase().as_str() {
        "int2" | "int4" | "int8" | "smallint" | "integer" | "bigint" | "serial" | "bigserial" => "integer",
        "bool" | "boolean" => "boolean",
        "timestamp" | "timestamptz" | "timestamp without time zone" | "timestamp with time zone" => "timestamp",
        "float4" | "float8" | "real" | "double precision" => "float",
        "numeric" | "decimal" => "numeric",
        "date" => "date",
        "uuid" => "uuid",
        "varchar" | "character varying" | "bpchar" | "char" | "name" | "citext" => "text",
        "json" | "jsonb" => "json",
        _ => "text",
    }
}

/// One `information_schema.columns` row in the dump `raw_schema` shape.
pub fn postgres_column(name: &str, udt_name: &str, nullable: bool) -> Value {
    json!({
        "name": name,
        "title": name,
        "nullable": nullable,
        "description": "",
        "native_type": {
            "name": postgres_native_type_name(udt_name),
            "nullable": nullable,
            "native_type_class_name": "common_native_type",
        },
        "user_type": postgres_user_type(udt_name),
        "has_auto_aggregation": false,
        "lock_aggregation": false,
    })
}

/// `GET /connections/{id}/info/sources`: table templates from the database.
///
/// `search_text` filters on the qualified `schema.table` name, `limit`/`offset` page the
/// result — the same UI contract Python's `get_data_source_templates_paginated` implements.
pub async fn list_sources(
    connection: &UsConnection,
    connection_id: &str,
    search_text: Option<&str>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Vec<Value>, String> {
    let rows = postgres_fetch(
        connection,
        "SELECT table_schema, table_name FROM information_schema.tables \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY table_schema, table_name",
    )
    .await?;
    let needle = search_text.map(str::to_ascii_lowercase).filter(|text| !text.is_empty());
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let schema = row.first().cloned().flatten().unwrap_or_default();
        let table = row.get(1).cloned().flatten().unwrap_or_default();
        if table.is_empty() {
            continue;
        }
        if let Some(needle) = &needle {
            let qualified = format!("{schema}.{table}").to_ascii_lowercase();
            if !qualified.contains(needle) {
                continue;
            }
        }
        items.push(source_item(connection_id, &schema, &table));
    }
    let offset = offset.unwrap_or(0).min(items.len());
    let mut items = items.split_off(offset);
    if let Some(limit) = limit {
        items.truncate(limit);
    }
    Ok(items)
}

/// One table template (`DataSourceTemplate` dump) of `/info/sources`.
pub fn source_item(connection_id: &str, schema: &str, table: &str) -> Value {
    let parameters = json!({"schema_name": schema, "table_name": table});
    let parameters_map = match parameters.as_object() {
        Some(map) => map,
        None => empty_map(),
    };
    let parameter_hash = control_schemas::parameter_hash("PG_TABLE", connection_id, parameters_map);
    json!({
        "title": table,
        "group": [schema],
        "source_type": "PG_TABLE",
        "connection_id": connection_id,
        "parameters": parameters,
        "tab_title": Value::Null,
        "form": Value::Null,
        "disabled": false,
        "parameter_hash": parameter_hash,
    })
}

/// `POST /connections/{id}/info/source/schema`: `raw_schema` for a table or custom SQL.
pub async fn source_schema(
    connection: &UsConnection,
    source_type: &str,
    parameters: &Map<String, Value>,
) -> Result<Vec<Value>, String> {
    match source_type {
        "PG_TABLE" => {
            let schema = parameters.get("schema_name").and_then(Value::as_str).unwrap_or("public");
            let table = parameters
                .get("table_name")
                .and_then(Value::as_str)
                .ok_or("Missing data for required field: table_name")?;
            let sql = format!(
                "SELECT udt_name, column_name, is_nullable FROM information_schema.columns \
                 WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
                sql_literal(schema),
                sql_literal(table),
            );
            let rows = postgres_fetch(connection, &sql).await?;
            Ok(rows
                .iter()
                .filter_map(|row| {
                    let udt = row.first().cloned().flatten()?;
                    let name = row.get(1).cloned().flatten()?;
                    let nullable = row.get(2).cloned().flatten().unwrap_or_default() == "YES";
                    Some(postgres_column(&name, &udt, nullable))
                })
                .collect())
        }
        "PG_SUBSELECT" => {
            let subsql = parameters
                .get("subsql")
                .and_then(Value::as_str)
                .ok_or("Missing data for required field: subsql")?;
            // The custom-SQL schema comes from the server's own type description of the query.
            let (_, meta) = postgres_fetch_meta(
                connection,
                &format!("SELECT * FROM ({subsql}) AS _dl_source LIMIT 0"),
            )
            .await?;
            Ok(meta
                .names
                .iter()
                .zip(meta.db_types.iter())
                .map(|(name, db_type)| postgres_column(name, db_type, true))
                .collect())
        }
        other => Err(format!("Unsupported source type for the native control-api: {other}")),
    }
}

// ---------------------------------------------------------------------------------------------
// connector form configs (`/info/connectors/forms/{conn_type}/{mode}`)
// ---------------------------------------------------------------------------------------------

/// Why a connector form could not be answered (`ConnectionForm` in the Python control-api).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormLookupError {
    BadConnType(String),
    UnknownMode(String),
}

fn form_catalog() -> &'static Value {
    static CATALOG: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| serde_json::from_str(CONNECTOR_FORMS_JSON).unwrap_or(Value::Null))
}

/// `GET /api/v1/info/connectors/forms/{conn_type}/{form_mode}`.
///
/// The reference has `create` and `edit` forms per connector (some connectors have no form,
/// which Python answers with `{"form": null}`). Only Postgres and ClickHouse `edit` forms were
/// captured; another connector's edit form falls back to its create form.
pub fn connector_form_payload(conn_type: &str, form_mode: &str) -> Result<Value, FormLookupError> {
    if spec(conn_type).is_none() {
        return Err(FormLookupError::BadConnType(conn_type.to_string()));
    }
    if !matches!(form_mode, "create" | "edit") {
        return Err(FormLookupError::UnknownMode(form_mode.to_string()));
    }
    let catalog = form_catalog();
    let form = catalog
        .pointer(&format!("/{form_mode}/{conn_type}"))
        .or_else(|| catalog.pointer(&format!("/create/{conn_type}")))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({"form": form}))
}

// ---------------------------------------------------------------------------------------------
// source templates (`/info/metadata_sources`, `freeform_sources` of `/info/sources`)
// ---------------------------------------------------------------------------------------------

fn form_input(
    name: &str,
    input_type: &str,
    default: &str,
    title: &str,
    field_doc_key: Option<&str>,
    template_enabled: bool,
) -> Value {
    let mut input = Map::new();
    input.insert("name".to_string(), json!(name));
    input.insert("input_type".to_string(), json!(input_type));
    input.insert("default".to_string(), json!(default));
    input.insert("required".to_string(), json!(true));
    input.insert("title".to_string(), json!(title));
    if let Some(field_doc_key) = field_doc_key {
        input.insert("field_doc_key".to_string(), json!(field_doc_key));
    }
    input.insert("template_enabled".to_string(), json!(template_enabled));
    Value::Object(input)
}

/// The free-form templates a connector offers (Python `get_data_source_template_templates`).
///
/// Postgres and ClickHouse are the captured reference; the other SQL connectors reuse the
/// schema-name/table-name shape, and the API connectors have no templates.
fn source_templates(conn_type: &str) -> Vec<(&'static str, &'static str, &'static str, Value)> {
    match conn_type {
        "postgres" | "greenplum" => vec![
            (
                "Table",
                "PG_TABLE",
                "Table",
                json!([
                    form_input("schema_name", "text", "", "Schema name", None, false),
                    form_input("table_name", "text", "", "Table name", Some("ANY_TABLE/table_name"), true),
                ]),
            ),
            (
                "SQL",
                "PG_SUBSELECT",
                "SQL",
                json!([form_input("subsql", "sql", "select 1 as a", "Subquery", Some("PG_SUBSELECT/subsql"), true)]),
            ),
        ],
        "clickhouse" | "chyt" => vec![
            (
                "Table",
                "CH_TABLE",
                "Table",
                json!([
                    form_input("db_name", "text", "", "Database name", None, false),
                    form_input("table_name", "text", "", "Table name", Some("ANY_TABLE/table_name"), true),
                ]),
            ),
            (
                "SQL",
                "CH_SUBSELECT",
                "SQL",
                json!([form_input("subsql", "sql", "select 1 as a", "Subquery", Some("ANY_SUBSELECT/subsql"), true)]),
            ),
        ],
        "mysql" | "mssql" | "oracle" | "trino" | "ydb" => vec![
            (
                "Table",
                source_types(Some(conn_type)).first().copied().unwrap_or("TABLE"),
                "Table",
                json!([
                    form_input("schema_name", "text", "", "Schema name", None, false),
                    form_input("table_name", "text", "", "Table name", Some("ANY_TABLE/table_name"), true),
                ]),
            ),
            (
                "SQL",
                source_types(Some(conn_type)).get(1).copied().unwrap_or("SUBSELECT"),
                "SQL",
                json!([form_input("subsql", "sql", "select 1 as a", "Subquery", None, true)]),
            ),
        ],
        _ => Vec::new(),
    }
}

fn template_payload(template: (&'static str, &'static str, &'static str, Value), connection_id: &str) -> Value {
    let (title, source_type, tab_title, form) = template;
    json!({
        "title": title,
        "group": [],
        "source_type": source_type,
        "connection_id": connection_id,
        "parameters": {},
        "tab_title": tab_title,
        "form": form,
        "disabled": false,
        "parameter_hash": control_schemas::parameter_hash(source_type, connection_id, &Map::new()),
    })
}

/// `freeform_sources` of `/info/sources` and the whole `/info/metadata_sources` body.
pub fn metadata_sources_payload(conn_type: &str, connection_id: &str) -> Value {
    let freeform: Vec<Value> = source_templates(conn_type)
        .into_iter()
        .map(|template| template_payload(template, connection_id))
        .collect();
    json!({"freeform_sources": freeform, "sources": Value::Null})
}

/// `{"sources": [...tables], "freeform_sources": [...templates]}` of `/info/sources`.
pub fn info_sources_payload(conn_type: &str, connection_id: &str, sources: Vec<Value>) -> Value {
    let freeform: Vec<Value> = source_templates(conn_type)
        .into_iter()
        .map(|template| template_payload(template, connection_id))
        .collect();
    json!({"sources": sources, "freeform_sources": freeform})
}

// Generated from golden `/info/connectors/forms/...` responses of the Python reference.
const CONNECTOR_FORMS_JSON: &str = r##"{"create":{"postgres":{"title":"PostgreSQL","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","defaultValue":"6432","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Path to database"},{"id":"input","name":"db_name","width":"l"}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","text":"Password"},{"id":"input","name":"password","defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Specifying collate in the request"},{"id":"radio_button","name":"enforce_collate","displayConditions":{"advanced_settings":"opened"},"defaultValue":"auto","options":[{"text":"Auto","value":"auto"},{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"TLS","helpText":"When the option is enabled, the sslmode parameter takes the value required, and when disabled \u2014 prefer"},{"id":"radio_button","name":"ssl_enable","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"CA Certificate"},{"id":"file-input","name":"ssl_ca","displayConditions":{"advanced_settings":"opened"}}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"enforce_collate","required":false,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"}],"conditions":[]}}},"greenplum":{"title":"Greenplum","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","defaultValue":"5432","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Path to database"},{"id":"input","name":"db_name","width":"l"}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","text":"Password"},{"id":"input","name":"password","defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Specifying collate in the request"},{"id":"radio_button","name":"enforce_collate","displayConditions":{"advanced_settings":"opened"},"defaultValue":"auto","options":[{"text":"Auto","value":"auto"},{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"enforce_collate","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"}],"conditions":[]}}},"ydb":{"title":"YDB","rows":[{"items":[{"id":"label","text":"Authentication type"},{"id":"radio_button","name":"auth_type","defaultValue":"oauth","options":[{"text":"Anonymous","value":"anonymous"},{"text":"Password","value":"password"},{"text":"OAuth","value":"oauth"}]}]},{"items":[{"id":"label","displayConditions":{"auth_type":"oauth"},"text":"OAuth-token"},{"id":"input","name":"token","displayConditions":{"auth_type":"oauth"},"width":"l","controlProps":{"type":"password"}}]},{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","defaultValue":"2135","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Path to database"},{"id":"input","name":"db_name","width":"l"}]},{"items":[{"id":"label","displayConditions":{"auth_type":"password"},"text":"Username"},{"id":"input","name":"username","displayConditions":{"auth_type":"password"},"width":"m"}]},{"items":[{"id":"label","displayConditions":{"auth_type":"password"},"text":"Password"},{"id":"input","name":"token","displayConditions":{"auth_type":"password"},"defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"TLS","helpText":"When the option is enabled, it ensures mandatory use of ssl for connection."},{"id":"radio_button","name":"ssl_enable","displayConditions":{"advanced_settings":"opened"},"defaultValue":"on","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"CA Certificate"},{"id":"file-input","name":"ssl_ca","displayConditions":{"advanced_settings":"opened"}}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"token","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"skip"},{"name":"auth_type","required":true,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[{"when":{"name":"auth_type"},"equals":"password","then":[{"selector":{"name":"username"},"action":"include"}]},{"when":{"name":"auth_type"},"equals":"anonymous","then":[{"selector":{"name":"token"},"action":"skip"}]}]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"token","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"skip"},{"name":"auth_type","required":true,"defaultAction":"include"}],"conditions":[{"when":{"name":"auth_type"},"equals":"password","then":[{"selector":{"name":"username"},"action":"include"}]},{"when":{"name":"auth_type"},"equals":"anonymous","then":[{"selector":{"name":"token"},"action":"skip"}]}]}}},"clickhouse":{"title":"ClickHouse","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"HTTP interface port"},{"id":"input","name":"port","defaultValue":"8443","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","text":"Password"},{"id":"input","name":"password","defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"TLS","helpText":"When the option is enabled, the HTTPS protocol is used when interacting with the database, and when disabled \u2014 HTTP"},{"id":"radio_button","name":"secure","displayConditions":{"advanced_settings":"opened"},"defaultValue":"on","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"CA Certificate"},{"id":"file-input","name":"ssl_ca","displayConditions":{"advanced_settings":"opened"}}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Readonly","align":"start"},{"id":"select","name":"readonly","displayConditions":{"advanced_settings":"opened"},"defaultValue":"2","width":"s","availableValues":[{"content":"0","value":"0"},{"content":"1","value":"1"},{"content":"2","value":"2"}],"controlProps":{"showSearch":false}}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":false,"defaultAction":"include"},{"name":"password","required":false,"defaultAction":"include"},{"name":"secure","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"readonly","required":false,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":false,"defaultAction":"include"},{"name":"password","required":false,"defaultAction":"include"},{"name":"secure","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"readonly","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"}],"conditions":[]}}},"chyt":{"title":"YTsaurus CHYT","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Clique alias"},{"id":"input","name":"alias","defaultValue":"*ch_public","width":"l"}]},{"items":[{"id":"label","text":"YTsaurus token"},{"id":"input","name":"token","defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"items":[{"id":"checkbox","name":"secure","defaultValue":false,"text":"HTTPS"}]},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"alias","required":true,"defaultAction":"include"},{"name":"token","required":true,"defaultAction":"include"},{"name":"secure","type":"boolean","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"alias","required":true,"defaultAction":"include"},{"name":"token","required":true,"defaultAction":"include"},{"name":"secure","type":"boolean","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"}],"conditions":[]}}},"oracle":{"title":"Oracle Database","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","defaultValue":"1521","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Path to database"},{"id":"input","name":"db_name","width":"l"},{"id":"radio_button","name":"db_connect_method","defaultValue":"service_name","options":[{"text":"Service name","value":"service_name"},{"text":"SID","value":"sid"}]}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","text":"Password"},{"id":"input","name":"password","defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"TLS","helpText":"When the option is enabled, the TCPS protocol is used when interacting with the database, and when disabled \u2014 TCP"},{"id":"radio_button","name":"ssl_enable","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"CA Certificate"},{"id":"file-input","name":"ssl_ca","displayConditions":{"advanced_settings":"opened"}}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_connect_method","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_connect_method","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"}],"conditions":[]}}},"mssql":{"title":"MS SQL Server","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","defaultValue":"1433","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Path to database"},{"id":"input","name":"db_name","width":"l"}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","text":"Password"},{"id":"input","name":"password","defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"}],"conditions":[]}}},"mysql":{"title":"MySQL","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","defaultValue":"3306","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Path to database"},{"id":"input","name":"db_name","width":"l"}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","text":"Password"},{"id":"input","name":"password","defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"TLS","helpText":"When the option is enabled, it ensures mandatory use of ssl for connection."},{"id":"radio_button","name":"ssl_enable","displayConditions":{"advanced_settings":"opened"},"defaultValue":"on","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"CA Certificate"},{"id":"file-input","name":"ssl_ca","displayConditions":{"advanced_settings":"opened"}}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"}],"conditions":[]}}},"metrika_api":{"title":"Metrica","rows":[{"items":[{"id":"label","text":"OAuth token"},{"id":"input","name":"token","width":"l","controlProps":{"type":"password"}}]},{"items":[{"id":"label","text":"Counter"},{"id":"input","name":"counter_id","width":"l","placeholder":"Counter ID"}]},{"type":"metrica_accuracy","name":"accuracy"},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"counter_id","required":true,"defaultAction":"include"},{"name":"token","required":true,"defaultAction":"include"},{"name":"accuracy","required":false,"nullable":true,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]}},"templateName":"metrica_api"},"trino":{"title":"Trino","rows":[{"items":[{"id":"label","text":"Authentication type"},{"id":"radio_button","name":"auth_type","defaultValue":"password","options":[{"text":"No auth","value":"none"},{"text":"Password","value":"password"},{"text":"JWT","value":"jwt"}]}]},{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","defaultValue":"8443","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","displayConditions":{"auth_type":"password"},"text":"Password"},{"id":"input","name":"password","displayConditions":{"auth_type":"password"},"defaultValue":"","width":"m","controlProps":{"type":"password"}}]},{"items":[{"id":"label","displayConditions":{"auth_type":"jwt"},"text":"JWT"},{"id":"input","name":"jwt","displayConditions":{"auth_type":"jwt"},"defaultValue":"","width":"l","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"TLS","helpText":"When this option is enabled, connections to Trino use the HTTPS protocol; when disabled - HTTP."},{"id":"radio_button","name":"ssl_enable","displayConditions":{"advanced_settings":"opened"},"defaultValue":"on","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"CA Certificate"},{"id":"file-input","name":"ssl_ca","displayConditions":{"advanced_settings":"opened"}}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Request table list","helpText":"Disable if loading the table list from the source takes too long or causes errors \u2014 in this case, add tables via SQL queries"},{"id":"radio_button","name":"listing_sources","displayConditions":{"advanced_settings":"opened"},"defaultValue":"on","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"auth_type","required":true,"defaultAction":"include"},{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"skip"},{"name":"jwt","required":true,"defaultAction":"skip"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"listing_sources","required":false,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[{"when":{"name":"auth_type"},"equals":"password","then":[{"selector":{"name":"password"},"action":"include"}]},{"when":{"name":"auth_type"},"equals":"jwt","then":[{"selector":{"name":"jwt"},"action":"include"}]}]},"check":{"items":[{"name":"auth_type","required":true,"defaultAction":"include"},{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":true,"defaultAction":"skip"},{"name":"jwt","required":true,"defaultAction":"skip"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"listing_sources","required":false,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"}],"conditions":[{"when":{"name":"auth_type"},"equals":"password","then":[{"selector":{"name":"password"},"action":"include"}]},{"when":{"name":"auth_type"},"equals":"jwt","then":[{"selector":{"name":"jwt"},"action":"include"}]}]}}},"appmetrica_api":{"title":"AppMetrica","rows":[{"items":[{"id":"label","text":"OAuth token"},{"id":"input","name":"token","width":"l","controlProps":{"type":"password"}}]},{"items":[{"id":"label","text":"Application"},{"id":"input","name":"counter_id","width":"l","placeholder":"Application ID"}]},{"type":"metrica_accuracy","name":"accuracy"},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"create":{"items":[{"name":"counter_id","required":true,"defaultAction":"include"},{"name":"token","required":true,"defaultAction":"include"},{"name":"accuracy","required":false,"nullable":true,"defaultAction":"include"},{"name":"type","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]}},"templateName":"appmetrica_api"}},"edit":{"postgres":{"title":"PostgreSQL","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"Port"},{"id":"input","name":"port","defaultValue":"6432","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Path to database"},{"id":"input","name":"db_name","width":"l"}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","text":"Password"},{"id":"input","name":"password","width":"m","fakeValue":"******","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Specifying collate in the request"},{"id":"radio_button","name":"enforce_collate","displayConditions":{"advanced_settings":"opened"},"defaultValue":"auto","options":[{"text":"Auto","value":"auto"},{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"TLS","helpText":"When the option is enabled, the sslmode parameter takes the value required, and when disabled \u2014 prefer"},{"id":"radio_button","name":"ssl_enable","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"CA Certificate"},{"id":"file-input","name":"ssl_ca","displayConditions":{"advanced_settings":"opened"}}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"edit":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"password","required":false,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"enforce_collate","required":false,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"db_name","required":true,"defaultAction":"include"},{"name":"username","required":true,"defaultAction":"include"},{"name":"password","required":false,"defaultAction":"include"},{"name":"ssl_enable","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"}],"conditions":[]}}},"clickhouse":{"title":"ClickHouse","rows":[{"items":[{"id":"label","text":"Hostname"},{"id":"input","name":"host","width":"l","controlProps":{"type":"text"}}]},{"items":[{"id":"label","text":"HTTP interface port"},{"id":"input","name":"port","defaultValue":"8443","width":"s","controlProps":{"type":"number"}}]},{"items":[{"id":"label","text":"Username"},{"id":"input","name":"username","width":"m"}]},{"items":[{"id":"label","text":"Password"},{"id":"input","name":"password","width":"m","fakeValue":"******","controlProps":{"type":"password"}}]},{"type":"cache_ttl_sec","name":"cache_ttl_sec"},{"type":"raw_sql_level","name":"raw_sql_level","defaultValue":"off","switchOffValue":"off","label":{"id":"label","text":"Raw SQL level","align":"start","helpText":"- We do not recommend combining subqueries in datasets with RLS or restrictions on viewing tables.\n- SQL queries from charts enter the connection directly and do not take into account the RLS configured in the datasets."},"radioGroup":{"id":"radio_group","name":"raw_sql_level","options":[{"content":{"text":"Allow subselects in datasets","hintText":"This option allows you to describe dataset sources using SQL queries"},"value":"subselect"},{"content":{"text":"Allow subselects in datasets and source parametrization","hintText":"This option allows you to describe dataset sources using SQL queries and use templates in dataset sources"},"value":"template"},{"content":{"text":"Allow subselects in datasets, source parametrization and QL-charts","hintText":"This option allows you to describe dataset sources using SQL queries, use templates in dataset sources and create QL charts","textEndIcon":{"name":"CircleExclamation","view":"error"}},"value":"dashsql"}]}},{"type":"collapse","name":"advanced_settings","inner":true,"text":"Advanced connection settings"},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"TLS","helpText":"When the option is enabled, the HTTPS protocol is used when interacting with the database, and when disabled \u2014 HTTP"},{"id":"radio_button","name":"secure","displayConditions":{"advanced_settings":"opened"},"defaultValue":"on","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"CA Certificate"},{"id":"file-input","name":"ssl_ca","displayConditions":{"advanced_settings":"opened"}}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Disable data export","helpText":"Disabling data export will hide the data export button in the charts, though you will still be able to extract data from charts and take screenshots."},{"id":"radio_button","name":"data_export_forbidden","displayConditions":{"advanced_settings":"opened"},"defaultValue":"off","options":[{"text":"Off","value":"off"},{"text":"On","value":"on"}]}]},{"items":[{"id":"label","displayConditions":{"advanced_settings":"opened"},"text":"Readonly","align":"start"},{"id":"select","name":"readonly","displayConditions":{"advanced_settings":"opened"},"defaultValue":"2","width":"s","availableValues":[{"content":"0","value":"0"},{"content":"1","value":"1"},{"content":"2","value":"2"}],"controlProps":{"showSearch":false}}]},{"items":[{"id":"hidden","name":"description","defaultValue":""}]}],"apiSchema":{"edit":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":false,"defaultAction":"include"},{"name":"password","required":false,"defaultAction":"include"},{"name":"secure","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"readonly","required":false,"defaultAction":"include"},{"name":"cache_ttl_sec","required":false,"nullable":true,"defaultAction":"include"},{"name":"raw_sql_level","required":false,"defaultAction":"include"},{"name":"description","type":"string","required":false,"nullable":false,"defaultAction":"include"}],"conditions":[]},"check":{"items":[{"name":"host","required":true,"defaultAction":"include"},{"name":"port","required":true,"defaultAction":"include"},{"name":"username","required":false,"defaultAction":"include"},{"name":"password","required":false,"defaultAction":"include"},{"name":"secure","required":false,"defaultAction":"include"},{"name":"ssl_ca","required":false,"defaultAction":"include"},{"name":"data_export_forbidden","required":false,"defaultAction":"include"},{"name":"readonly","required":false,"defaultAction":"include"}],"conditions":[]}}}}}"##;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn postgres_entry() -> Value {
        json!({
            "entryId": "gz2tnvb9zdxm5",
            "scope": "connection",
            "type": "postgres",
            "key": "1560944148196885505/pg",
            "createdAt": "2023-11-24T13:37:22.834Z",
            "updatedAt": "2026-11-13T21:54:30.694Z",
            "workbookId": "z4wtz6tg5194o",
            "meta": {"state": "saved"},
            "annotation": {"description": ""},
            "data": {
                "host": "postgres",
                "name": null,
                "port": 5432,
                "ssl_ca": "null",
                "db_name": "pg-demo-db",
                "username": "pg-user",
                "ssl_enable": false,
                "table_name": null,
                "cache_ttl_sec": null,
                "raw_sql_level": "dashsql",
                "schema_version": "1",
                "enforce_collate": "auto",
                "sample_table_name": null,
                "data_export_forbidden": false,
            },
        })
    }

    #[test]
    fn connectors_payload_has_the_reference_catalog() {
        let payload = connectors_payload();
        let items = payload["uncategorized"].as_array().unwrap();
        assert_eq!(items.len(), 11);
        assert_eq!(items[0]["conn_type"], "clickhouse");
        assert_eq!(items[1]["title"], "PostgreSQL");
        assert_eq!(items[1]["visibility_mode"], "free");
        assert!(payload["sections"].as_array().unwrap().is_empty());
        assert!(icons_payload().get("icons").and_then(Value::as_array).is_some_and(|icons| !icons.is_empty()));
    }

    #[test]
    fn connection_get_matches_the_reference_shape() {
        let payload = connection_get_payload(&postgres_entry());
        let object = payload.as_object().unwrap();
        for key in [
            "cache_ttl_sec",
            "updated_at",
            "data_export_forbidden",
            "id",
            "db_type",
            "created_at",
            "host",
            "db_name",
            "workbook_id",
            "port",
            "description",
            "enforce_collate",
            "name",
            "raw_sql_level",
            "ssl_enable",
            "username",
            "key",
            "meta",
            "options",
        ] {
            assert!(object.contains_key(key), "missing {key}: {payload}");
        }
        assert!(!object.contains_key("password"), "secret leaked");
        assert!(!object.contains_key("ssl_ca"), "internal key leaked");
        // The Python schema dumps the on/off enums as strings, not booleans.
        assert_eq!(payload["ssl_enable"], "off");
        assert_eq!(payload["data_export_forbidden"], "off");
        assert_eq!(payload["name"], "pg");
        assert_eq!(payload["options"]["allow_dashsql_usage"], true);
    }

    #[test]
    fn connection_export_masks_the_secret_and_adds_the_warning() {
        let payload = connection_export_payload(&postgres_entry());
        let connection = &payload["connection"];
        assert_eq!(connection["password"], "******");
        assert_eq!(connection["db_type"], "postgres");
        assert_eq!(connection["name"], "pg");
        for removed in ["id", "key", "meta", "options", "created_at"] {
            assert!(connection.get(removed).is_none(), "{removed} leaked into export");
        }
        assert_eq!(payload["notifications"][0]["code"], "NOTIF.WB_EXPORT.CONN.CHECK_CREDENTIALS");
    }

    #[test]
    fn connection_export_rejects_unknown_connectors() {
        let mut entry = postgres_entry();
        entry["type"] = json!("file");
        let payload = connection_export_payload(&entry);
        assert_eq!(payload["notifications"][0]["level"], "critical");
        assert_eq!(payload["notifications"][0]["code"], UNSUPPORTED_CODE);
        assert!(payload.get("connection").is_none());
    }

    #[test]
    fn connection_import_reverses_the_export_shape() {
        let export = connection_export_payload(&postgres_entry());
        let body = json!({"data": {"workbook_id": "z4wtz6tg5194o", "connection": export["connection"]}});
        let create = connection_import_body(&body).unwrap();
        assert_eq!(create["scope"], "connection");
        assert_eq!(create["type"], "postgres");
        assert_eq!(create["name"], "pg");
        assert_eq!(create["workbookId"], "z4wtz6tg5194o");
        let data = &create["data"];
        // on/off strings go back to the booleans the US entry stores.
        assert_eq!(data["ssl_enable"], false);
        assert_eq!(data["data_export_forbidden"], false);
        assert_eq!(data["schema_version"], "1");
        assert!(data.get("db_type").is_none());
        assert!(data.get("password").is_none(), "masked secret must not be stored");
    }

    #[test]
    fn connection_import_rejects_unknown_connectors() {
        let body = json!({"data": {"workbook_id": "w", "connection": {"db_type": "file"}}});
        let error = connection_import_body(&body).unwrap_err();
        assert!(error.contains("file"), "{error}");
    }

    #[test]
    fn connector_forms_match_the_reference_catalog() {
        let postgres = connector_form_payload("postgres", "create").unwrap();
        assert_eq!(postgres["form"]["title"], "PostgreSQL");
        assert!(postgres["form"]["rows"].is_array());
        let clickhouse = connector_form_payload("clickhouse", "create").unwrap();
        assert_eq!(clickhouse["form"]["title"], "ClickHouse");
        // The demo deployment has exactly two editable connectors; their edit forms are
        // captured, the rest fall back to the create form.
        let edit = connector_form_payload("postgres", "edit").unwrap();
        assert_ne!(edit["form"], postgres["form"]);
        let fallback = connector_form_payload("mysql", "edit").unwrap();
        assert_eq!(fallback["form"], connector_form_payload("mysql", "create").unwrap()["form"]);
        assert_eq!(
            connector_form_payload("nope", "create"),
            Err(FormLookupError::BadConnType("nope".to_string()))
        );
        assert_eq!(
            connector_form_payload("postgres", "bogus"),
            Err(FormLookupError::UnknownMode("bogus".to_string()))
        );
    }

    #[test]
    fn source_templates_match_the_reference_hashes() {
        let postgres = metadata_sources_payload("postgres", "gz2tnvb9zdxm5");
        let freeform = postgres["freeform_sources"].as_array().unwrap();
        assert_eq!(freeform.len(), 2);
        assert_eq!(freeform[0]["title"], "Table");
        assert_eq!(freeform[0]["source_type"], "PG_TABLE");
        assert_eq!(freeform[0]["parameter_hash"], "12ad1570b5aa41a7");
        assert_eq!(freeform[1]["source_type"], "PG_SUBSELECT");
        assert_eq!(freeform[1]["parameter_hash"], "3778cd78a74c8762");
        assert_eq!(postgres["sources"], Value::Null);
        let clickhouse = metadata_sources_payload("clickhouse", "lv0y1z1bcxy84");
        assert_eq!(clickhouse["freeform_sources"][0]["source_type"], "CH_TABLE");
        assert_eq!(clickhouse["freeform_sources"][0]["form"][0]["name"], "db_name");
    }

    #[test]
    fn source_item_matches_the_reference_hash() {
        let item = source_item("gz2tnvb9zdxm5", "public", "geo_polygons");
        assert_eq!(item["title"], "geo_polygons");
        assert_eq!(item["group"], json!(["public"]));
        assert_eq!(item["source_type"], "PG_TABLE");
        assert_eq!(item["parameter_hash"], "1f65b985df23c9b3");
        assert_eq!(
            item["parameters"],
            json!({"schema_name": "public", "table_name": "geo_polygons"})
        );
    }

    #[test]
    fn postgres_type_mapping_matches_the_reference() {
        let cases = [
            ("numeric", "float"),
            ("text", "string"),
            ("timestamp", "genericdatetime"),
            ("bool", "boolean"),
            ("int4", "integer"),
            ("uuid", "uuid"),
            ("jsonb", "string"),
            ("geometry", "geopolygon"),
        ];
        for (udt, expected) in cases {
            assert_eq!(postgres_user_type(udt), expected, "udt: {udt}");
        }
        let native = [
            ("numeric", "numeric"),
            ("int4", "integer"),
            ("bool", "boolean"),
            ("timestamptz", "timestamp"),
            ("varchar", "text"),
        ];
        for (udt, expected) in native {
            assert_eq!(postgres_native_type_name(udt), expected, "udt: {udt}");
        }
    }

    #[test]
    fn postgres_column_shape_matches_the_reference() {
        let column = postgres_column("amount", "numeric", false);
        assert_eq!(
            column,
            json!({
                "name": "amount",
                "title": "amount",
                "nullable": false,
                "description": "",
                "native_type": {
                    "name": "numeric",
                    "nullable": false,
                    "native_type_class_name": "common_native_type",
                },
                "user_type": "float",
                "has_auto_aggregation": false,
                "lock_aggregation": false,
            })
        );
    }

    #[test]
    fn info_sources_payload_has_both_lists() {
        let payload = info_sources_payload(
            "postgres",
            "gz2tnvb9zdxm5",
            vec![source_item("gz2tnvb9zdxm5", "public", "orders")],
        );
        assert_eq!(payload["sources"].as_array().unwrap().len(), 1);
        assert_eq!(payload["freeform_sources"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn source_listing_options_match_the_goldens() {
        let postgres = listing_options(Some("postgres"));
        assert_eq!(postgres["supports_source_search"], true);
        assert_eq!(postgres["supports_source_pagination"], true);
        assert_eq!(postgres["supports_db_name_listing"], false);
        let clickhouse = listing_options(Some("clickhouse"));
        assert_eq!(clickhouse["supports_source_search"], false);
        assert_eq!(clickhouse["supports_source_pagination"], false);
    }

    #[test]
    fn test_connection_params_rejects_incomplete_bodies() {
        let error = resolved_connection_params(&json!({"host": "postgres"})).unwrap_err();
        assert!(error.contains("type"), "{error}");
        let error = resolved_connection_params(&json!({"type": "postgres"})).unwrap_err();
        assert!(!error.is_empty());
    }

    #[test]
    fn connection_create_body_matches_the_reference_shape() {
        let body = json!({
            "type": "postgres",
            "name": "scratch",
            "host": "postgres",
            "port": 5432,
            "db_name": "pg-demo-db",
            "username": "pg-user",
            "password": "secret",
            "ssl_enable": "off",
            "workbook_id": "z4wtz6tg5194o",
        });
        let create = connection_create_body(&body).unwrap();
        assert_eq!(create["scope"], "connection");
        assert_eq!(create["type"], "postgres");
        assert_eq!(create["name"], "scratch");
        assert_eq!(create["workbookId"], "z4wtz6tg5194o");
        assert_eq!(create["data"]["ssl_enable"], false);
        assert_eq!(create["data"]["raw_sql_level"], "off");
        assert!(create["data"].get("password").is_none(), "password leaked into data");
        assert_eq!(
            create["unversionedData"],
            json!({"password": {"key_kind": "plain", "key_id": "", "cypher_text": "secret"}})
        );
        assert_eq!(
            connection_create_body(&json!({})).unwrap_err(),
            "Invalid connection type value: {}"
        );
        assert_eq!(
            connection_create_body(&json!({"type": "nope"})).unwrap_err(),
            "Invalid connection type value: nope"
        );
    }

    #[test]
    fn connection_update_body_merges_changes_and_keeps_the_secret() {
        let entry = json!({
            "entryId": "con",
            "type": "postgres",
            "data": {"host": "postgres", "port": 5432, "db_name": "db", "username": "u", "ssl_enable": false},
            "unversionedData": {"password": {"key_kind": "local_fernet", "key_id": "KEY", "cypher_text": "token"}},
            "annotation": {"description": "conn"},
        });
        let update = connection_update_body(&entry, &json!({"username": "renamed", "ssl_enable": "on"})).unwrap();
        assert_eq!(update["mode"], "publish");
        assert_eq!(update["data"]["username"], "renamed");
        assert_eq!(update["data"]["ssl_enable"], true);
        assert_eq!(update["data"]["host"], "postgres");
        // A masked password keeps the stored encrypted secret.
        let keep = connection_update_body(&entry, &json!({"password": "******"})).unwrap();
        assert_eq!(keep["unversionedData"], entry["unversionedData"]);
        // A new password is stored as a plain secret (the port has no fernet encryptor).
        let replace = connection_update_body(&entry, &json!({"password": "new"})).unwrap();
        assert_eq!(replace["unversionedData"]["password"]["cypher_text"], "new");
    }

    #[test]
    fn connection_stored_data_fills_the_connector_defaults() {
        let mut source = Map::new();
        source.insert("host".to_string(), json!("postgres"));
        source.insert("port".to_string(), json!(5432));
        source.insert("db_name".to_string(), json!("db"));
        source.insert("ssl_ca".to_string(), json!("null"));
        let raw = connection_stored_data("postgres", &source);
        assert_eq!(raw["host"], "postgres");
        assert_eq!(raw["schema_version"], "1");
        assert_eq!(raw["enforce_collate"], "auto");
        assert_eq!(raw["raw_sql_level"], "off");
        assert_eq!(raw["name"], Value::Null);
    }
}
