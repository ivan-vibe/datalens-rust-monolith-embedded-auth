//! Control-api payload transforms: `us` entries and data-api answers -> the JSON the
//! DataLens UI's BI API client expects.
//!
//! The Python control-api builds these payloads from `dl_api_lib` schema dumps. The native
//! monolith keeps the same wire shapes but derives them from the already-stored `us` entry
//! (plus the in-process data-api for `/fields`), so most of this module is pure data
//! transformation — which is also what the tests below pin against golden responses captured
//! from the Python reference.
//!
//! `options` is the one part that cannot be derived from the entry alone: the Python
//! `dump_option_data` asks connector/dialect plugins for supported functions, casts,
//! aggregations and listing capabilities. Here it is assembled from the Postgres-dialect
//! catalog of the reference deployment and the current connection types; see the `shortcut:`
//! comments where that differs per connector.
//!
//! shortcut: RLS bodies and source `index_info_set` are carried through verbatim/empty instead
//! of being re-serialised by the Python RLS/schema serialisers; upgrade when the dataset
//! editor has to edit RLS rules through the native control-api.

use std::collections::{BTreeSet, HashMap};

use dl_core::{CalcMode, Dataset, Field, Source, UserDataType};
use dl_query::{check_formulas, Dialect, QueryError};
use serde_json::{json, Map, Value};

use crate::connections;

/// `OK` / validation error codes (`dl_constants.exc`).
pub const CODE_OK: &str = "OK";
pub const CODE_VALIDATION_ERROR: &str = "ERR.DS_API.VALIDATION.ERROR";
pub const CODE_OBJ_NOT_FOUND: &str = "ERR.DS_API.US.OBJ_NOT_FOUND";
pub const CODE_VALIDATION_FAILED: &str = "ERR.DS_API.US.VALIDATION_FAILED";

const VALIDATION_OK_MESSAGE: &str = "Validation was successful";
const VALIDATION_ERROR_MESSAGE: &str = "Validation finished with errors.";

/// `GET /api/v1/datasets/{id}/fields`: the control-api schema of `DatasetFieldsResponseSchema`.
///
/// The in-process data-api already answers `/api/data/v1/datasets/{id}/fields`; the control
/// API exposes the same fields with six keys only (the data-api answer carries the internal
/// `cast`/`formula`/`aggregation`/... the UI does not read here).
pub fn fields_payload(data_api: &Value) -> Value {
    let fields: Vec<Value> = data_api
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(Value::as_object)
                .map(|field| {
                    json!({
                        "title": field.get("title").cloned().unwrap_or(Value::Null),
                        "guid": field.get("guid").cloned().unwrap_or(Value::Null),
                        "data_type": field.get("data_type").cloned().unwrap_or(Value::Null),
                        "hidden": field.get("hidden").cloned().unwrap_or(json!(false)),
                        "type": field.get("type").cloned().unwrap_or(Value::Null),
                        "calc_mode": field.get("calc_mode").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({"fields": fields})
}

// ---------------------------------------------------------------------------------------------
// dataset content (`DatasetContentInternalSchema` dump)
// ---------------------------------------------------------------------------------------------

fn object(value: Option<&Value>) -> &Map<String, Value> {
    match value.and_then(Value::as_object) {
        Some(map) => map,
        None => empty_map(),
    }
}

fn empty_map() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

fn array(value: Option<&Value>) -> &[Value] {
    value.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Last segment of a US key (`1507164767695933207/Cohorts` -> `Cohorts`).
pub fn key_name(entry: &Value) -> &str {
    entry
        .get("key")
        .and_then(Value::as_str)
        .and_then(|key| key.rsplit('/').next())
        .unwrap_or("")
}

/// One `result_schema` field in the shape `dump_dataset_data` returns: the stored field plus
/// the properties the Python `Field` object computes (`aggregation_locked`,
/// `autoaggregated`, `virtual`, `ui_settings`, parameter `template_enabled`).
/// Public wrapper of the stored-field normalizer: an `add_field` action field gets the same
/// defaults (`hidden`, `has_auto_aggregation`, `aggregation_locked`, `virtual`, ...) as a field
/// read from the stored dataset, which is what the Python validation response returns.
pub fn normalized_dataset_field(field: &Map<String, Value>) -> Value {
    dataset_field(field)
}

fn dataset_field(field: &Map<String, Value>) -> Value {
    let mut out = field.clone();
    let has_auto = field.get("has_auto_aggregation").and_then(Value::as_bool).unwrap_or(false);
    let lock_aggregation = field.get("lock_aggregation").and_then(Value::as_bool).unwrap_or(false);
    let calc_mode = field.get("calc_mode").and_then(Value::as_str).unwrap_or("direct");
    let is_measure = field.get("type").and_then(Value::as_str) == Some("MEASURE");
    let aggregation = field.get("aggregation").and_then(Value::as_str).unwrap_or("none");
    // `BIField.autoaggregated` / `.aggregation_locked` (`dl_core.fields`).
    let autoaggregated = has_auto || (is_measure && aggregation == "none");
    let aggregation_locked = matches!(calc_mode, "formula" | "parameter") || autoaggregated || lock_aggregation;
    out.insert("virtual".to_string(), json!(false));
    out.insert("ui_settings".to_string(), json!(""));
    out.insert("autoaggregated".to_string(), json!(autoaggregated));
    out.insert("aggregation_locked".to_string(), json!(aggregation_locked));
    if let Some(Value::Object(typed)) = field.get("default_value") {
        if let Some(value) = typed.get("value") {
            out.insert("default_value".to_string(), value.clone());
        }
    }
    if field.get("calc_mode").and_then(Value::as_str) == Some("parameter") {
        out.insert("template_enabled".to_string(), json!(false));
    }
    Value::Object(out)
}

/// `origin.raw_schema` -> the dump shape: `type` is `user_type`, the connection type is not
/// part of the dumped native type.
fn dataset_source_column(column: &Map<String, Value>) -> Value {
    let mut native = column
        .get("native_type")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    native.remove("conn_type");
    json!({
        "name": column.get("name").cloned().unwrap_or(Value::Null),
        "title": column.get("title").cloned().unwrap_or(Value::Null),
        "nullable": column.get("nullable").cloned().unwrap_or(json!(true)),
        "description": column.get("description").cloned().unwrap_or(json!("")),
        "native_type": Value::Object(native),
        "user_type": column.get("type").cloned().unwrap_or(Value::Null),
        "has_auto_aggregation": column.get("has_auto_aggregation").cloned().unwrap_or(json!(false)),
        "lock_aggregation": column.get("lock_aggregation").cloned().unwrap_or(json!(false)),
    })
}

/// `source_type` of a stored source: `origin.created_from` when present, else inferred from
/// the parameters (`PG_TABLE` for a table, `PG_SUBSELECT` for custom SQL).
fn source_type_of(origin: &Map<String, Value>, conn_type: Option<&str>) -> String {
    if let Some(created_from) = origin.get("created_from").and_then(Value::as_str) {
        return created_from.to_string();
    }
    let parameters = object(origin.get("parameters"));
    let prefix = conn_type.and_then(|conn| connections::source_types(Some(conn)).first().copied());
    match (parameters.get("subsql"), parameters.get("table_name")) {
        (Some(Value::String(_)), _) => prefix.map(|value| value.replace("_TABLE", "_SUBSELECT")).unwrap_or_else(|| "PG_SUBSELECT".to_string()),
        _ => prefix.unwrap_or("PG_TABLE").to_string(),
    }
}

/// One `source_collections` entry -> one `sources` item of the API dump.
fn dataset_source(collection: &Map<String, Value>, conn_types: &HashMap<String, String>) -> Value {
    let origin = object(collection.get("origin"));
    let connection_id = origin.get("connection_id").and_then(Value::as_str).unwrap_or("");
    let conn_type = conn_types.get(connection_id).map(String::as_str);
    let source_type = source_type_of(origin, conn_type);
    let mut parameters = origin.get("parameters").cloned().unwrap_or_else(|| json!({}));
    // `TableSQLDataSource.get_parameters()` always reports `db_name`, even when unset.
    if source_type.ends_with("_TABLE") {
        if let Some(object) = parameters.as_object_mut() {
            object.entry("db_name".to_string()).or_insert(Value::Null);
        }
    }
    let raw_schema: Vec<Value> = array(origin.get("raw_schema"))
        .iter()
        .filter_map(Value::as_object)
        .map(dataset_source_column)
        .collect();
    json!({
        "id": collection.get("id").cloned().unwrap_or(Value::Null),
        "title": collection.get("title").cloned().unwrap_or(Value::Null),
        "connection_id": json!(connection_id),
        "managed_by": collection.get("managed_by").cloned().unwrap_or(json!("user")),
        "valid": collection.get("valid").cloned().unwrap_or(json!(true)),
        "virtual": json!(false),
        "source_type": json!(source_type),
        "raw_schema": raw_schema,
        "index_info_set": origin.get("index_info_set").cloned().unwrap_or(Value::Null),
        "parameters": parameters,
        "parameter_hash": json!(parameter_hash(&source_type, connection_id, &object(origin.get("parameters")).clone())),
    })
}

/// `avatar_relations` in the dump shape: the US storage calls the condition ends
/// `left_part`/`right_part` and their kind `condition_type`; `BIField`-era dumps rename them.
fn dataset_relation(relation: &Map<String, Value>) -> Value {
    let conditions: Vec<Value> = array(relation.get("conditions"))
        .iter()
        .filter_map(Value::as_object)
        .map(|condition| {
            // The API (and the 1.39 responses, `DatasetContentInternalSchema`) spells the parts
            // `left`/`right`/`type`; the US storage schema spells them `left_part`/`right_part`/
            // `condition_type`. Both are accepted on input here, and `stored_dataset_data` does
            // the reverse mapping on save.
            json!({
                "operator": condition.get("operator").cloned().unwrap_or(Value::Null),
                "left": condition.get("left").or_else(|| condition.get("left_part")).cloned().unwrap_or(Value::Null),
                "right": condition.get("right").or_else(|| condition.get("right_part")).cloned().unwrap_or(Value::Null),
                "type": condition.get("type").or_else(|| condition.get("condition_type")).cloned().unwrap_or(json!("binary")),
            })
        })
        .collect();
    json!({
        "id": relation.get("id").cloned().unwrap_or(Value::Null),
        "conditions": conditions,
        "required": relation.get("required").cloned().unwrap_or(json!(false)),
        "join_type": relation.get("join_type").cloned().unwrap_or(Value::Null),
        "left_avatar_id": relation.get("left_avatar_id").cloned().unwrap_or(Value::Null),
        "right_avatar_id": relation.get("right_avatar_id").cloned().unwrap_or(Value::Null),
        "virtual": json!(false),
        "managed_by": relation.get("managed_by").cloned().unwrap_or(json!("user")),
    })
}

fn dataset_relations(relations: Option<&Value>) -> Value {
    Value::Array(
        array(relations)
            .iter()
            .filter_map(Value::as_object)
            .map(dataset_relation)
            .collect(),
    )
}

fn dataset_avatar(avatar: &Map<String, Value>) -> Value {
    json!({
        "source_id": avatar.get("source_id").cloned().unwrap_or(Value::Null),
        "valid": avatar.get("valid").cloned().unwrap_or(json!(true)),
        "title": avatar.get("title").cloned().unwrap_or(Value::Null),
        "is_root": avatar.get("is_root").cloned().unwrap_or(json!(false)),
        "virtual": json!(false),
        "managed_by": avatar.get("managed_by").cloned().unwrap_or(json!("user")),
        "id": avatar.get("id").cloned().unwrap_or(Value::Null),
    })
}

/// Build `dataset` in the shape `DatasetResource.make_dataset_response_data` returns.
pub fn dataset_content(entry: &Value) -> Value {
    let data = object(entry.get("data"));
    let result_schema: Vec<Value> = array(data.get("result_schema"))
        .iter()
        .filter_map(Value::as_object)
        .map(dataset_field)
        .collect();
    let source_avatars: Vec<Value> = array(data.get("source_avatars"))
        .iter()
        .filter_map(Value::as_object)
        .map(dataset_avatar)
        .collect();
    // Sources need their connection types only for the dumped `source_type` prefix; the
    // stored `created_from` is authoritative when present.
    let conn_types = HashMap::new();
    let sources: Vec<Value> = array(data.get("source_collections"))
        .iter()
        .filter_map(Value::as_object)
        .map(|collection| dataset_source(collection, &conn_types))
        .collect();
    let rls = match data.get("rls") {
        Some(Value::Object(rls)) => Value::Object(rls.clone()),
        _ => json!({}),
    };
    let preview_enabled = data
        .get("preview_enabled")
        .cloned()
        .unwrap_or(json!(true));
    let description = match data.get("description") {
        Some(Value::String(text)) => json!(text.as_str()),
        _ => entry
            .pointer("/annotation/description")
            .cloned()
            .unwrap_or_else(|| json!("")),
    };
    json!({
        "component_errors": data.get("component_errors").cloned().unwrap_or_else(|| json!({"items": []})),
        "template_enabled": data.get("template_enabled").cloned().unwrap_or(json!(false)),
        "result_schema": result_schema,
        "source_avatars": source_avatars,
        "load_preview_by_default": data.get("load_preview_by_default").cloned().unwrap_or(json!(true)),
        "data_export_forbidden": data.get("data_export_forbidden").cloned().unwrap_or(json!(false)),
        "description": description,
        "rls": rls,
        "result_schema_aux": data.get("result_schema_aux").cloned().unwrap_or_else(|| json!({"inter_dependencies": {"deps": []}})),
        "obligatory_filters": data.get("obligatory_filters").cloned().unwrap_or_else(|| json!([])),
        "revision_id": data.get("revision_id").cloned().unwrap_or(Value::Null),
        "preview_enabled": preview_enabled,
        "rls2": data.get("rls2").cloned().unwrap_or_else(|| json!({})),
        "sources": sources,
        "avatar_relations": dataset_relations(data.get("avatar_relations")),
    })
}

/// `source_collections[].origin.connection_id`, in first-seen order.
pub fn source_connection_ids(entry: &Value) -> Vec<String> {
    let mut seen = BTreeSet::new();
    array(entry.pointer("/data/source_collections"))
        .iter()
        .filter_map(|collection| collection.pointer("/origin/connection_id"))
        .filter_map(Value::as_str)
        .filter(|id| !id.is_empty())
        .for_each(|id| {
            seen.insert(id.to_string());
        });
    seen.into_iter().collect()
}

/// The connection a field validator has to compile formulas for.
pub fn connection_type_of(entry: &Value, conn_types: &HashMap<String, String>) -> Option<String> {
    source_connection_ids(entry)
        .first()
        .and_then(|id| conn_types.get(id).cloned())
}

/// Formula-validation dialect: the connectors this deployment serves map onto the three
/// dialects the in-tree compiler knows.
fn dialect_of(conn_type: Option<&str>) -> Dialect {
    match conn_type {
        Some("clickhouse") | Some("chyt") => Dialect::ClickHouse,
        Some("mysql") => Dialect::Mysql,
        _ => Dialect::Postgres,
    }
}

/// `dump_option_data` for the parts the editor reads.
///
/// `conn_types` maps connection ids to their `db_type` (the handler resolves it from US).
/// shortcut: data_types / supported_functions are the Postgres-dialect catalogs captured from
/// the reference; per-connector dialect differences are not resolved yet.
pub fn dataset_options(content: &Value, conn_types: &HashMap<String, String>) -> Value {
    let sources = array(content.get("sources"));
    if sources.is_empty() && conn_types.is_empty() {
        // A dataset without sources is compiled with the dummy dialect in Python, so its
        // capabilities differ from the Postgres catalog; keep the captured reference body.
        let mut options = dummy_dataset_options().clone();
        options["preview"] = json!({
            "enabled": content.get("preview_enabled").cloned().unwrap_or(json!(true)),
        });
        return options;
    }
    let avatars = array(content.get("source_avatars"));
    let fields = array(content.get("result_schema"));

    let mut connection_ids = BTreeSet::new();
    let mut source_types_seen: Vec<&'static str> = Vec::new();
    let mut source_items = Vec::with_capacity(sources.len());
    let mut schema_update_enabled = false;
    for source in sources.iter().filter_map(Value::as_object) {
        let connection_id = source.get("connection_id").and_then(Value::as_str).unwrap_or("");
        let conn_type = conn_types.get(connection_id).map(String::as_str);
        let supports_schema_update = connections::supports_schema_update(conn_type);
        schema_update_enabled |= supports_schema_update;
        source_items.push(json!({
            "id": source.get("id").cloned().unwrap_or(Value::Null),
            "schema_update_enabled": supports_schema_update,
        }));
        if !connection_id.is_empty() {
            connection_ids.insert(connection_id.to_string());
        }
        for source_type in connections::source_types(conn_type) {
            if !source_types_seen.contains(source_type) {
                source_types_seen.push(source_type);
            }
        }
    }

    let mut avatar_items = Vec::with_capacity(avatars.len());
    for avatar in avatars.iter().filter_map(Value::as_object) {
        let source_id = avatar.get("source_id").and_then(Value::as_str).unwrap_or("");
        let conn_type = sources
            .iter()
            .filter_map(Value::as_object)
            .find(|source| source.get("id").and_then(Value::as_str) == Some(source_id))
            .and_then(|source| source.get("connection_id").and_then(Value::as_str))
            .and_then(|connection_id| conn_types.get(connection_id))
            .map(String::as_str);
        avatar_items.push(json!({
            "id": avatar.get("id").cloned().unwrap_or(Value::Null),
            "schema_update_enabled": connections::supports_schema_update(conn_type),
        }));
    }

    let replacement_types: Vec<Value> = connections::REPLACEMENT_TYPES
        .iter()
        .map(|conn_type| json!({"conn_type": conn_type}))
        .collect();
    let connection_items: Vec<Value> = connection_ids
        .iter()
        .map(|id| json!({"id": id, "replacement_types": replacement_types}))
        .collect();
    let compatible_connection_types: Vec<Value> = if sources.is_empty() {
        connections::REPLACEMENT_TYPES
            .iter()
            .map(|conn_type| json!({"conn_type": conn_type}))
            .collect()
    } else {
        Vec::new()
    };
    let compatible_source_types: Vec<Value> = source_types_seen
        .into_iter()
        .map(|source_type| json!({"source_type": source_type}))
        .collect();
    let field_items: Vec<Value> = fields
        .iter()
        .filter_map(Value::as_object)
        .map(|field| {
            let has_auto = field.get("has_auto_aggregation").and_then(Value::as_bool).unwrap_or(false);
            let lock_aggregation = field
                .get("lock_aggregation")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            // `dump_option_data`: casts come from `initial_data_type`, aggregations from
            // `cast` (`CASTS_BY_TYPE` vs `BI_TYPE_AGGREGATIONS` in `dl_api_lib.enums`).
            let initial_type = field
                .get("initial_data_type")
                .or_else(|| field.get("data_type"))
                .and_then(Value::as_str)
                .unwrap_or("string");
            let cast = field
                .get("cast")
                .or_else(|| field.get("data_type"))
                .and_then(Value::as_str)
                .unwrap_or(initial_type);
            let aggregations = if has_auto || lock_aggregation {
                json!(["none"])
            } else {
                json!(aggregations_for(cast))
            };
            json!({
                "guid": field.get("guid").cloned().unwrap_or(Value::Null),
                "casts": casts_for(initial_type),
                "aggregations": aggregations,
            })
        })
        .collect();

    let source_listing = if conn_types.len() == 1 {
        let conn_type = conn_types.values().next().map(String::as_str);
        connections::listing_options(conn_type)
    } else {
        Value::Null
    };
    let mut options = json!({
        "schema_update_enabled": schema_update_enabled,
        "connections": {
            "items": connection_items,
            "compatible_types": compatible_connection_types,
            "max": if sources.is_empty() { 20 } else { 1 },
        },
        "source_avatars": {
            "items": avatar_items,
            "max": if sources.is_empty() { 1 } else { 32 },
        },
        "supports_offset": true,
        "supported_functions": supported_functions(),
        "fields": {"items": field_items},
        "join": {"types": ["inner", "left", "right", "full"], "operators": ["eq"]},
        "preview": {"enabled": content.get("preview_enabled").cloned().unwrap_or(json!(true))},
        "sources": {
            "items": source_items,
            "compatible_types": compatible_source_types,
        },
        "data_types": data_types(),
    });
    if !source_listing.is_null() {
        options["source_listing"] = source_listing;
    }
    options
}

/// Top-level `GET /datasets/{id}` / `GET /datasets/{id}/versions/{version}` body.
pub fn dataset_response(entry: &Value, options: Value) -> Value {
    let mut out = Map::with_capacity(10);
    out.insert("options".to_string(), options);
    if let Some(collection_id) = entry.get("collectionId").filter(|value| !value.is_null()) {
        out.insert("collection_id".to_string(), collection_id.clone());
    } else {
        out.insert(
            "workbook_id".to_string(),
            entry.get("workbookId").cloned().unwrap_or(Value::Null),
        );
    }
    out.insert(
        "is_favorite".to_string(),
        entry.get("isFavorite").cloned().unwrap_or(json!(false)),
    );
    out.insert(
        "full_permissions".to_string(),
        entry.get("fullPermissions").cloned().unwrap_or_else(|| json!({})),
    );
    out.insert("dataset".to_string(), dataset_content(entry));
    out.insert("key".to_string(), entry.get("key").cloned().unwrap_or(Value::Null));
    out.insert(
        "permissions".to_string(),
        entry.get("permissions").cloned().unwrap_or_else(|| json!({})),
    );
    out.insert("name".to_string(), json!(key_name(entry)));
    out.insert("id".to_string(), entry.get("entryId").cloned().unwrap_or(Value::Null));
    Value::Object(out)
}

// ---------------------------------------------------------------------------------------------
// export / import
// ---------------------------------------------------------------------------------------------

const EXPORT_MISSING_CONNECTION: &str = "Can not export dataset without a connection";
const IMPORT_MISSING_CONNECTION: &str = "Can not import dataset without a connection";
pub const DS_EXPORT_CODE: &str = "ERR.DS_API.WB_EXPORT.DS";
pub const DS_IMPORT_CODE: &str = "ERR.DS_API.WB_IMPORT.DS";

fn critical_notification(code: &str, message: &str) -> Value {
    json!({"notifications": [{"message": message, "level": "critical", "code": code}]})
}

/// `POST /api/v1/datasets/export/{id}` (`DatasetExportItem.post`).
///
/// On a missing connection mapping the Python wrapper answers 200 with a critical
/// notification and no dataset, so the caller sends that body unchanged.
pub fn dataset_export_payload(entry: &Value, id_mapping: &Map<String, Value>) -> Value {
    let mut content = dataset_content(entry);
    content["name"] = json!(key_name(entry));
    content["revision_id"] = Value::Null;
    if let Some(dataset) = content.as_object_mut() {
        dataset.remove("rls");
    }
    if let Some(sources) = content.get_mut("sources").and_then(Value::as_array_mut) {
        for source in sources.iter_mut() {
            let Some(connection_id) = source.get("connection_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(mapped) = id_mapping.get(connection_id) else {
                return critical_notification(DS_EXPORT_CODE, EXPORT_MISSING_CONNECTION);
            };
            source["connection_id"] = mapped.clone();
        }
    }
    json!({"dataset": content, "notifications": []})
}

/// A fresh `revision_id` for an imported dataset. The value is opaque to the API (Python
/// generates a random token), so a process-unique value is enough.
pub fn fresh_revision_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("RV{:016x}{:04x}", nanos, counter & 0xffff)
}

/// The API content a field had before `dump_dataset_data` added its computed keys.
fn stored_field(field: &Map<String, Value>) -> Value {
    let mut out = field.clone();
    for key in [
        "autoaggregated",
        "aggregation_locked",
        "ui_settings",
        "virtual",
        "template_enabled",
    ] {
        out.remove(key);
    }
    let data_type = out
        .get("initial_data_type")
        .or_else(|| out.get("data_type"))
        .and_then(Value::as_str)
        .unwrap_or("string")
        .to_string();
    if let Some(default_value) = out.get("default_value").cloned() {
        if !default_value.is_null() && !default_value.is_object() {
            out.insert(
                "default_value".to_string(),
                json!({"type": data_type, "value": default_value}),
            );
        }
    }
    out.entry("guid_formula".to_string()).or_insert_with(|| json!(""));
    out.entry("source".to_string()).or_insert_with(|| json!(""));
    out.entry("formula".to_string()).or_insert_with(|| json!(""));
    out.entry("avatar_id".to_string()).or_insert(Value::Null);
    out.entry("managed_by".to_string()).or_insert_with(|| json!("user"));
    out.entry("aggregation".to_string()).or_insert_with(|| json!("none"));
    out.entry("valid".to_string()).or_insert_with(|| json!(true));
    out.entry("hidden".to_string()).or_insert_with(|| json!(false));
    out.entry("description".to_string()).or_insert_with(|| json!(""));
    out.entry("initial_data_type".to_string())
        .or_insert_with(|| json!(data_type));
    out.entry("cast".to_string()).or_insert_with(|| json!(data_type));
    out.entry("lock_aggregation".to_string()).or_insert_with(|| json!(false));
    out.entry("has_auto_aggregation".to_string())
        .or_insert_with(|| json!(false));
    Value::Object(out)
}

fn stored_source_column(column: &Map<String, Value>, conn_type: Option<&str>) -> Value {
    let mut native = column
        .get("native_type")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(conn_type) = conn_type {
        native.insert("conn_type".to_string(), json!(conn_type));
    }
    json!({
        "name": column.get("name").cloned().unwrap_or(Value::Null),
        "title": column.get("title").cloned().unwrap_or_else(|| column.get("name").cloned().unwrap_or(Value::Null)),
        "nullable": column.get("nullable").cloned().unwrap_or(json!(true)),
        "description": column.get("description").cloned().unwrap_or(json!("")),
        "native_type": Value::Object(native),
        "type": column.get("user_type").cloned().unwrap_or(Value::Null),
        "has_auto_aggregation": column.get("has_auto_aggregation").cloned().unwrap_or(json!(false)),
        "lock_aggregation": column.get("lock_aggregation").cloned().unwrap_or(json!(false)),
    })
}

fn stored_source(source: &Map<String, Value>, conn_types: &HashMap<String, String>) -> Value {
    let connection_id = source.get("connection_id").and_then(Value::as_str).unwrap_or("");
    let conn_type = conn_types.get(connection_id).map(String::as_str);
    let raw_schema: Vec<Value> = array(source.get("raw_schema"))
        .iter()
        .filter_map(Value::as_object)
        .map(|column| stored_source_column(column, conn_type))
        .collect();
    json!({
        "id": source.get("id").cloned().unwrap_or(Value::Null),
        "type": "collection",
        "title": source.get("title").cloned().unwrap_or(Value::Null),
        "valid": source.get("valid").cloned().unwrap_or(json!(true)),
        "origin": {
            "parameters": source.get("parameters").cloned().unwrap_or_else(|| json!({})),
            "raw_schema": raw_schema,
            "created_from": source.get("source_type").cloned().unwrap_or(Value::Null),
            "connection_id": json!(connection_id),
        },
        "sample": Value::Null,
        "managed_by": source.get("managed_by").cloned().unwrap_or(json!("user")),
        "materialization": Value::Null,
    })
}

/// The API dataset content -> the `us` entry `data` object (the storage shape).
///
/// `id_mapping` rewrites source connection ids (`export` fake -> `import` real); `None` means
/// the content already names the real connections (create/update from the editor).
pub fn stored_dataset_data(
    dataset: &Map<String, Value>,
    id_mapping: Option<&Map<String, Value>>,
    conn_types: &HashMap<String, String>,
) -> Result<Value, String> {
    let mut sources = Vec::new();
    for source in array(dataset.get("sources")).iter().filter_map(Value::as_object) {
        let mut source = source.clone();
        let connection_id = source.get("connection_id").and_then(Value::as_str).unwrap_or("");
        if connection_id.is_empty() {
            sources.push(stored_source(&source, conn_types));
            continue;
        }
        let mapped = match id_mapping {
            Some(mapping) => mapping
                .get(connection_id)
                .and_then(Value::as_str)
                .ok_or_else(|| IMPORT_MISSING_CONNECTION.to_string())?,
            None => connection_id,
        };
        source.insert("connection_id".to_string(), json!(mapped));
        sources.push(stored_source(&source, conn_types));
    }

    let mut avatars = Vec::new();
    for avatar in array(dataset.get("source_avatars")).iter().filter_map(Value::as_object) {
        let mut avatar = avatar.clone();
        avatar.remove("virtual");
        avatars.push(Value::Object(avatar));
    }

    let result_schema: Vec<Value> = array(dataset.get("result_schema"))
        .iter()
        .filter_map(Value::as_object)
        .map(stored_field)
        .collect();

    Ok(json!({
        "rls": [],
        "name": "",
        "revision_id": Value::Null,
        "result_schema": result_schema,
        "source_avatars": avatars,
        "avatar_relations": stored_relations(dataset.get("avatar_relations")),
        "component_errors": {"items": []},
        "result_schema_aux": dataset.get("result_schema_aux").cloned().unwrap_or_else(|| json!({"inter_dependencies": {"deps": []}})),
        "obligatory_filters": dataset.get("obligatory_filters").cloned().unwrap_or_else(|| json!([])),
        "source_collections": sources,
    }))
}

/// API `avatar_relations` -> the US storage shape (`AvatarRelationSchema`): the condition parts
/// are `left_part`/`right_part` and the discriminator is `condition_type`. Without this the
/// Python control-api cannot deserialize the saved dataset (`BinaryCondition.__init__() missing
/// left_part`).
fn stored_relations(relations: Option<&Value>) -> Value {
    Value::Array(
        array(relations)
            .iter()
            .filter_map(Value::as_object)
            .map(|relation| {
                let conditions: Vec<Value> = array(relation.get("conditions"))
                    .iter()
                    .filter_map(Value::as_object)
                    .map(|condition| {
                        json!({
                            "operator": condition.get("operator").cloned().unwrap_or(Value::Null),
                            "left_part": condition.get("left").or_else(|| condition.get("left_part")).cloned().unwrap_or(Value::Null),
                            "right_part": condition.get("right").or_else(|| condition.get("right_part")).cloned().unwrap_or(Value::Null),
                            "condition_type": condition.get("type").or_else(|| condition.get("condition_type")).cloned().unwrap_or(json!("binary")),
                        })
                    })
                    .collect();
                let mut stored = relation.clone();
                stored.remove("virtual");
                stored.insert("conditions".to_string(), Value::Array(conditions));
                Value::Object(stored)
            })
            .collect(),
    )
}

/// Keep the stored-only keys of an existing dataset (its `name`, `created_via`, ...) while
/// replacing every key the editor content carries — the storage equivalent of the Python
/// loader's in-place update.
pub fn merge_stored_dataset(existing: &Value, incoming: Value) -> Value {
    let mut merged = existing.clone();
    if let (Some(merged), Some(incoming)) = (merged.as_object_mut(), incoming.as_object()) {
        for (key, value) in incoming {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

/// The `dataset` content + `options` of a write response (`CreateDatasetResponseSchema` /
/// `DatasetContentSchema`). `entry_id` is only present on create/copy responses.
pub fn dataset_write_response(
    entry_id: Option<&str>,
    stored: &Value,
    description: &str,
    conn_types: &HashMap<String, String>,
) -> Value {
    let entry = json!({
        "entryId": entry_id.unwrap_or(""),
        "data": stored,
        "annotation": {"description": description},
    });
    let content = dataset_content(&entry);
    let options = dataset_options(&content, conn_types);
    let mut response = Map::with_capacity(3);
    response.insert("dataset".to_string(), content);
    response.insert("options".to_string(), options);
    if let Some(entry_id) = entry_id {
        response.insert("id".to_string(), json!(entry_id));
    }
    Value::Object(response)
}

/// `POST /api/v1/datasets/import`: an exported dataset -> the US `dataset` entry create body.
///
/// `conn_types` maps the *mapped* (real) connection ids to `db_type`, which the stored
/// `origin.raw_schema[].native_type.conn_type` needs.
pub fn dataset_import_body(body: &Value, conn_types: &HashMap<String, String>) -> Result<Value, String> {
    let data = body.get("data").ok_or("Missing data for required field: data")?;
    let dataset = data
        .get("dataset")
        .and_then(Value::as_object)
        .ok_or("Missing data for required field: dataset")?;
    let id_mapping = body.get("id_mapping").and_then(Value::as_object);
    let name = dataset
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| data.get("name").and_then(Value::as_str))
        .unwrap_or("Dataset");
    let stored = stored_dataset_data(dataset, id_mapping, conn_types)?;

    let mut create = Map::with_capacity(9);
    if let Some(workbook_id) = data.get("workbook_id").filter(|value| !value.is_null()) {
        create.insert("workbookId".to_string(), workbook_id.clone());
    } else if let Some(collection_id) = data
        .get("collection_id")
        .filter(|value| !value.is_null())
        .or_else(|| dataset.get("collection_id").filter(|value| !value.is_null()))
    {
        create.insert("collectionId".to_string(), collection_id.clone());
    }
    create.insert("name".to_string(), json!(name));
    create.insert("scope".to_string(), json!("dataset"));
    create.insert("type".to_string(), json!(""));
    create.insert("mode".to_string(), json!("publish"));
    create.insert("links".to_string(), json!({}));
    create.insert("data".to_string(), stored);
    create.insert(
        "annotation".to_string(),
        json!({"description": dataset.get("description").and_then(Value::as_str).unwrap_or("")}),
    );
    Ok(Value::Object(create))
}

// ---------------------------------------------------------------------------------------------
// validators
// ---------------------------------------------------------------------------------------------

/// One formula parse problem, in the shape the field validator reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaIssue {
    pub code: &'static str,
    pub message: String,
    pub token: Option<String>,
    pub column: Option<u32>,
    pub row: Option<u32>,
}

/// Build the `dl_core` dataset the formula validator compiles against.
///
/// `content` is the API dump (`sources` + `result_schema`), which `dl_core::Dataset`
/// deserialises directly; without content the validator runs parse-only against a synthetic
/// single-source dataset, in which a `[Field]` reference cannot resolve (and is not an error
/// at this stage).
fn validation_dataset(content: Option<&Value>) -> Dataset {
    if let Some(content) = content {
        if let Ok(dataset) = serde_json::from_value::<Dataset>(content.clone()) {
            return dataset;
        }
    }
    let mut field = Field::default();
    field.guid = "validation-field".to_string();
    field.title = "Validation field".to_string();
    field.calc_mode = CalcMode::Formula;
    field.data_type = Some(UserDataType::Float);
    field.initial_data_type = Some(UserDataType::Float);
    field.cast = Some(UserDataType::Float);
    Dataset {
        sources: vec![Source {
            id: "validation-avatar".to_string(),
            table: "public.validation_source".to_string(),
            connection_id: None,
            subselect: None,
            columns: Vec::new(),
        }],
        fields: vec![field],
        ..Dataset::default()
    }
}

/// Parse `formula` with the in-tree `dl_query` compiler (which wraps `dfl_native`).
///
/// With `content`, field references are resolved against the dataset's fields, the way the
/// Python `DatasetValidator` compiles a formula; without it the check is parse-only.
///
/// shortcut: `dfl_native::parse_formula_typed` is not a direct dependency of the monolith
/// crate, so the structured parse error (exact token and column) is not reachable yet; the
/// compiler reports a message, and only the unexpected-EOF case reconstructs the column. Add
/// `dfl_native = { path = "vendor/data-api/dfl_native" }` to `Cargo.toml` and call
/// `parse_formula_typed` here for byte-identical `FORMULA.PARSE.*` payloads.
pub fn formula_issue(
    formula: &str,
    content: Option<&Value>,
    conn_type: Option<&str>,
) -> Option<FormulaIssue> {
    if formula.trim().is_empty() {
        return None;
    }
    let mut dataset = validation_dataset(content);
    let mut field = Field::default();
    field.guid = "validation-field".to_string();
    field.title = "Validation field".to_string();
    field.formula = formula.to_string();
    field.calc_mode = CalcMode::Formula;
    field.data_type = Some(UserDataType::Float);
    field.initial_data_type = Some(UserDataType::Float);
    field.cast = Some(UserDataType::Float);
    dataset.fields.push(field);
    let (_, error) = check_formulas(&dataset, dialect_of(conn_type))
        .into_iter()
        .find(|(guid, _)| guid == "validation-field")?;
    let chars = formula.chars().count() as u32;
    Some(match error {
        QueryError::Formula { reason, .. } => {
            if reason.contains("unexpected end") {
                FormulaIssue {
                    code: "FORMULA.PARSE.UNEXPECTED_EOF",
                    message: "Failed to parse: unexpected end of formula".to_string(),
                    token: None,
                    column: Some(chars),
                    row: Some(0),
                }
            } else {
                FormulaIssue {
                    code: "FORMULA.PARSE.UNEXPECTED_TOKEN",
                    message: "Failed to parse formula: unexpected token".to_string(),
                    token: None,
                    column: None,
                    row: None,
                }
            }
        }
        // Parse-only mode has no dataset fields, so a resolved-looking reference is not a
        // dataset error; the caller can only report syntax.
        QueryError::UnknownFieldInFormula(_) if content.is_none() => return None,
        QueryError::UnknownFieldInFormula(name) => FormulaIssue {
            code: "FORMULA.UNKNOWN_FIELD_IN_FORMULA",
            message: format!("Unknown field found in formula: {name}"),
            token: Some(name),
            column: None,
            row: None,
        },
        other => FormulaIssue {
            code: "FORMULA.UNKNOWN",
            message: other.to_string(),
            token: None,
            column: None,
            row: None,
        },
    })
}

/// `POST /api/v1/datasets/validators/field` (`DatasetVersionFieldValidator.post`).
pub fn field_validation_payload(
    title: &str,
    guid: &str,
    issue: Option<&FormulaIssue>,
    has_sources: bool,
) -> (u16, Value) {
    if !has_sources {
        // `get_single_formula_errors` with no sources: a single, code-less ERROR.
        return (
            400,
            json!({
                "code": CODE_VALIDATION_ERROR,
                "message": VALIDATION_ERROR_MESSAGE,
                "field_errors": [{
                    "title": title,
                    "guid": guid,
                    "errors": [{
                        "code": "",
                        "token": Value::Null,
                        "level": "ERROR",
                        "message": "No data sources have been defined",
                    }],
                }],
            }),
        );
    }
    let Some(issue) = issue else {
        return (
            200,
            json!({
                "code": CODE_OK,
                "message": VALIDATION_OK_MESSAGE,
                "field_errors": [],
            }),
        );
    };
    let mut error = Map::new();
    error.insert("code".to_string(), json!(issue.code));
    if let (Some(row), Some(column)) = (issue.row, issue.column) {
        error.insert("column".to_string(), json!(column));
        error.insert("row".to_string(), json!(row));
    }
    error.insert("token".to_string(), json!(issue.token));
    error.insert("level".to_string(), json!("error"));
    error.insert("message".to_string(), json!(issue.message));
    (
        400,
        json!({
            "code": CODE_VALIDATION_ERROR,
            "message": VALIDATION_ERROR_MESSAGE,
            "field_errors": [{"title": title, "guid": guid, "errors": [Value::Object(error)]}],
        }),
    )
}

/// `POST /datasets/validators/dataset` (`DatasetVersionValidator.post`) for the checks this
/// port can perform (formula parse of the provided/stored content).
pub fn dataset_validation_payload(content: &Value, options: Value, issues: &[Value]) -> (u16, Value) {
    let code = if issues.is_empty() { CODE_OK } else { CODE_VALIDATION_ERROR };
    let message = if issues.is_empty() {
        VALIDATION_OK_MESSAGE
    } else {
        VALIDATION_ERROR_MESSAGE
    };
    (
        if issues.is_empty() { 200 } else { 400 },
        json!({
            "dataset": content,
            "options": options,
            "code": code,
            "message": message,
            "dataset_errors": issues,
        }),
    )
}

/// `source_collections` of a US entry describe at least one source (the Python validator's
/// `_has_sources`).
pub fn has_sources(entry: &Value) -> bool {
    !array(entry.pointer("/data/source_collections")).is_empty()
}

pub fn has_content_sources(content: &Value) -> bool {
    !array(content.get("sources")).is_empty()
}

/// Parse every formula field of `content` (the dataset validator's formula pass).
pub fn content_formula_issues(content: &Value, conn_type: Option<&str>) -> Vec<Value> {
    array(content.get("result_schema"))
        .iter()
        .filter_map(Value::as_object)
        .filter(|field| field.get("calc_mode").and_then(Value::as_str) == Some("formula"))
        .filter_map(|field| {
            let formula = field.get("formula").and_then(Value::as_str).unwrap_or("");
            let issue = formula_issue(formula, Some(content), conn_type)?;
            Some(json!({
                "title": field.get("title").cloned().unwrap_or(Value::Null),
                "guid": field.get("guid").cloned().unwrap_or(Value::Null),
                "errors": [{
                    "code": issue.code,
                    "level": "error",
                    "token": issue.token,
                    "message": issue.message,
                }],
            }))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// parameter hash (xxhash64 over Python's tuple repr)
// ---------------------------------------------------------------------------------------------

/// `dl_core.data_source.utils.get_parameters_hash`: xxh64 of Python's `str()` of
/// `(source_type, connection_id, *(k, v) for sorted non-None parameters except db_version)`.
pub fn parameter_hash(source_type: &str, connection_id: &str, parameters: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = parameters
        .iter()
        .filter(|(key, value)| !value.is_null() && key.as_str() != "db_version")
        .map(|(key, _)| key)
        .collect();
    keys.sort();
    let mut parts = Vec::with_capacity(2 + keys.len() * 2);
    // The Python tuple holds the `DataSourceType` enum member; `str(tuple)` renders it as
    // `DataSourceType('PG_TABLE')` (unquoted), which is part of the hash input.
    parts.push(format!("DataSourceType('{source_type}')"));
    parts.push(python_repr_str(connection_id));
    for key in keys {
        parts.push(python_repr_str(key));
        parts.push(python_repr(parameters.get(key).unwrap_or(&Value::Null)));
    }
    let tuple = format!("({})", parts.join(", "));
    format!("{:016x}", xxh64(tuple.as_bytes(), 0))
}

fn python_repr_str(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                use std::fmt::Write;
                let _ = write!(out, "\\x{:02x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('\'');
    out
}

/// `repr()` of a decoded JSON value, as Python's `str(tuple)` renders it.
///
/// shortcut: nested objects/arrays fall back to compact JSON (single- vs double-quoted keys
/// differ from Python's `repr`); source parameters in this deployment are scalars.
fn python_repr(value: &Value) -> String {
    match value {
        Value::String(text) => python_repr_str(text),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

const XXH_P1: u64 = 0x9E37_79B1_85EB_CA87;
const XXH_P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const XXH_P3: u64 = 0x1656_67B1_9E37_79F9;
const XXH_P4: u64 = 0x85EB_CA77_C2B2_AE63;
const XXH_P5: u64 = 0x27D4_EB2F_1656_67C5;

fn xxh_round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(XXH_P2))
        .rotate_left(31)
        .wrapping_mul(XXH_P1)
}

fn xxh_merge(acc: u64, value: u64) -> u64 {
    (acc ^ xxh_round(0, value))
        .wrapping_mul(XXH_P1)
        .wrapping_add(XXH_P4)
}

fn read_u64(bytes: &[u8]) -> u64 {
    let mut array = [0u8; 8];
    array.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(array)
}

fn read_u32(bytes: &[u8]) -> u32 {
    let mut array = [0u8; 4];
    array.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(array)
}

/// xxHash64 (seed 0) — the hash `dl_core` computes with `xxhash.xxh64`.
fn xxh64(bytes: &[u8], seed: u64) -> u64 {
    let length = bytes.len();
    let mut index = 0;
    let mut hash;
    if length >= 32 {
        let mut v1 = seed.wrapping_add(XXH_P1).wrapping_add(XXH_P2);
        let mut v2 = seed.wrapping_add(XXH_P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(XXH_P1);
        while index + 32 <= length {
            v1 = xxh_round(v1, read_u64(&bytes[index..]));
            v2 = xxh_round(v2, read_u64(&bytes[index + 8..]));
            v3 = xxh_round(v3, read_u64(&bytes[index + 16..]));
            v4 = xxh_round(v4, read_u64(&bytes[index + 24..]));
            index += 32;
        }
        hash = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        hash = xxh_merge(hash, v1);
        hash = xxh_merge(hash, v2);
        hash = xxh_merge(hash, v3);
        hash = xxh_merge(hash, v4);
    } else {
        hash = seed.wrapping_add(XXH_P5);
    }
    hash = hash.wrapping_add(length as u64);
    while index + 8 <= length {
        hash ^= xxh_round(0, read_u64(&bytes[index..]));
        hash = hash
            .rotate_left(27)
            .wrapping_mul(XXH_P1)
            .wrapping_add(XXH_P4);
        index += 8;
    }
    if index + 4 <= length {
        hash ^= (read_u32(&bytes[index..]) as u64).wrapping_mul(XXH_P1);
        hash = hash
            .rotate_left(23)
            .wrapping_mul(XXH_P2)
            .wrapping_add(XXH_P3);
        index += 4;
    }
    while index < length {
        hash ^= (bytes[index] as u64).wrapping_mul(XXH_P5);
        hash = hash.rotate_left(11).wrapping_mul(XXH_P1);
        index += 1;
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(XXH_P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(XXH_P3);
    hash ^= hash >> 32;
    hash
}

// ---------------------------------------------------------------------------------------------
// static catalogs (captured from the reference deployment's Postgres dialect)
// ---------------------------------------------------------------------------------------------

fn parse_static(value: &'static str) -> Value {
    serde_json::from_str(value).unwrap_or(Value::Null)
}

pub fn supported_functions() -> Value {
    static CATALOG: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CATALOG
        .get_or_init(|| {
            parse_static(
                r#"["abs","acos","ago","all_concat","any","arr_distinct","arr_index_of","arr_intersect","arr_remove","arr_str","array","ascii","asin","at_date","atan","atan2","avg","avg_if","bold","bool","br","case","cast_arr_float","cast_arr_int","cast_arr_str","ceiling","char","color","concat","contains","contains_all","contains_any","cos","cot","count","count_if","count_item","countd","countd_if","date","dateadd","datepart","datetime","datetrunc","day","dayofweek","db_call_agg_float","db_call_agg_int","db_call_agg_string","db_call_array_float","db_call_array_int","db_call_array_string","db_call_bool","db_call_float","db_call_int","db_call_string","db_cast","degrees","div","div_safe","endswith","exp","fdiv_safe","find","first","float","floor","geopoint","geopolygon","get_item","greatest","hour","icontains","iendswith","if","ifnan","ifnull","iif","image","int","isnan","isnull","istartswith","italic","lag","last","least","left","len","ln","log","log10","lower","ltrim","markup","mavg","max","mcount","md5","median","min","minute","mmax","mmin","month","msum","notcontains","now","pi","power","quantile","quarter","radians","rank","rank_dense","rank_percentile","rank_unique","ravg","rcount","regexp_extract","regexp_extract_all","regexp_extract_nth","regexp_match","regexp_replace","replace","right","rmax","rmin","round","rsum","rtrim","second","sha1","sha256","sign","sin","size","slice","space","split","sqrt","square","startswith","stdev","stdevp","str","substr","sum","sum_if","tan","today","tooltip","tree","trim","unnest","upper","url","user_info","var","varp","week","year","zn"]"#,
            )
        })
        .clone()
}

pub fn data_types() -> Value {
    static CATALOG: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| parse_static(DATA_TYPES_JSON)).clone()
}

const DATA_TYPES_JSON: &str = r#"{"items":[{"aggregations":["count","countunique"],"type":"string","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","STARTSWITH","ISTARTSWITH","ENDSWITH","IENDSWITH","CONTAINS","ICONTAINS","NOTCONTAINS","NOTICONTAINS","NOTICONTAINS","IN","NIN","BETWEEN"],"casts":["string","date","genericdatetime","boolean","integer","float","geopoint","geopolygon"]},{"aggregations":["count","sum","avg","min","max","countunique"],"type":"integer","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","STARTSWITH","ISTARTSWITH","ENDSWITH","IENDSWITH","CONTAINS","ICONTAINS","NOTCONTAINS","NOTICONTAINS","NOTICONTAINS","IN","NIN","BETWEEN"],"casts":["integer","float","string","boolean","date","genericdatetime"]},{"aggregations":["count","sum","avg","min","max","countunique"],"type":"float","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["float","integer","string","boolean","date","genericdatetime"]},{"aggregations":["count","min","max","countunique","avg"],"type":"date","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["date","genericdatetime","string","integer","float","boolean"]},{"aggregations":["count","min","max","countunique","avg"],"type":"datetime","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["genericdatetime","datetime","date","string","integer","float","boolean"]},{"aggregations":["count"],"type":"boolean","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","IN","NIN"],"casts":["boolean","integer","float","string"]},{"aggregations":["count"],"type":"geopoint","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","IN","NIN"],"casts":["geopoint","string"]},{"aggregations":["count"],"type":"geopolygon","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","IN","NIN"],"casts":["geopolygon","string"]},{"aggregations":["count","countunique"],"type":"uuid","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","STARTSWITH","ISTARTSWITH","ENDSWITH","IENDSWITH","CONTAINS","ICONTAINS","NOTCONTAINS","NOTICONTAINS","NOTICONTAINS","IN","NIN","BETWEEN"],"casts":["uuid","string"]},{"aggregations":["count"],"type":"markup","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL"],"casts":["markup"]},{"aggregations":["count","min","max","countunique","avg"],"type":"datetimetz","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["datetimetz","genericdatetime","date","string","integer","float","boolean"]},{"aggregations":[],"type":"unsupported","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL"],"casts":["unsupported"]},{"aggregations":["count","countunique"],"type":"array_str","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","STARTSWITH","CONTAINS","NOTCONTAINS","LENEQ","LENEQ","LENNE","LENNE","LENGT","LENGT","LENGTE","LENGTE","LENLT","LENLT","LENLTE","LENLTE"],"casts":["array_str","string"]},{"aggregations":["count","countunique"],"type":"array_int","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","STARTSWITH","CONTAINS","NOTCONTAINS","LENEQ","LENEQ","LENNE","LENNE","LENGT","LENGT","LENGTE","LENGTE","LENLT","LENLT","LENLTE","LENLTE"],"casts":["array_int","string"]},{"aggregations":["count","countunique"],"type":"array_float","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","STARTSWITH","CONTAINS","NOTCONTAINS","LENEQ","LENEQ","LENNE","LENNE","LENGT","LENGT","LENGTE","LENGTE","LENLT","LENLT","LENLTE","LENLTE"],"casts":["array_float","string"]},{"aggregations":["count","countunique"],"type":"tree_str","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","STARTSWITH","CONTAINS","NOTCONTAINS","LENEQ","LENEQ","LENNE","LENNE","LENGT","LENGT","LENGTE","LENGTE","LENLT","LENLT","LENLTE","LENLTE"],"casts":["tree_str"]},{"aggregations":["count","min","max","countunique"],"type":"genericdatetime","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["genericdatetime","date","string","integer","float","boolean"]}]}"#;

const _: () = ();

/// `/datasets` create/update of a source-less dataset: the reference options body for the
/// dummy dialect (captured from the Python control-api).
fn dummy_dataset_options() -> &'static Value {
    static CATALOG: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| parse_static(DUMMY_DATASET_OPTIONS_JSON))
}

const DUMMY_DATASET_OPTIONS_JSON: &str = r##"{"schema_update_enabled":false,"connections":{"items":[],"compatible_types":[{"conn_type":"oracle"},{"conn_type":"clickhouse"},{"conn_type":"greenplum"},{"conn_type":"postgres"},{"conn_type":"mssql"},{"conn_type":"chyt"},{"conn_type":"mysql"},{"conn_type":"metrika_api"},{"conn_type":"trino"},{"conn_type":"ydb"},{"conn_type":"appmetrica_api"}],"max":20},"source_avatars":{"items":[],"max":32},"supports_offset":true,"supported_functions":["abs","acos","ago","all_concat","any","arr_distinct","arr_index_of","arr_intersect","arr_remove","arr_str","array","ascii","asin","at_date","atan","atan2","avg","avg_if","bold","bool","br","case","cast_arr_float","cast_arr_int","cast_arr_str","ceiling","char","cityhash64","color","concat","contains","contains_all","contains_any","cos","cot","count","count_if","count_item","countd","countd_if","date","dateadd","datepart","datetime","datetrunc","day","dayofweek","db_call_agg_float","db_call_agg_int","db_call_agg_string","db_call_array_float","db_call_array_int","db_call_array_string","db_call_bool","db_call_float","db_call_int","db_call_string","db_cast","degrees","div","div_safe","endswith","exp","fdiv_safe","find","first","float","floor","geopoint","geopolygon","get_item","greatest","hour","icontains","iendswith","if","ifnan","ifnull","iif","image","int","inthash64","isnan","isnull","istartswith","italic","lag","last","least","left","len","ln","log","log10","lower","ltrim","markup","mavg","max","mcount","md5","median","min","minute","mmax","mmin","month","msum","murmurhash2_64","notcontains","now","pi","power","quantile","quarter","radians","rank","rank_dense","rank_percentile","rank_unique","ravg","rcount","regexp_extract","regexp_extract_all","regexp_extract_nth","regexp_match","regexp_replace","replace","right","rmax","rmin","round","rsum","rtrim","second","sha1","sha256","sign","sin","siphash64","size","slice","space","split","sqrt","square","startswith","stdev","stdevp","str","substr","sum","sum_if","tan","today","tooltip","tree","trim","unnest","upper","url","user_info","var","varp","week","year","zn"],"fields":{"items":[]},"join":{"operators":["eq"],"types":["inner","left","right","full"]},"preview":{"enabled":true},"sources":{"items":[],"compatible_types":[{"source_type":"MYSQL_SUBSELECT"},{"source_type":"TRINO_TABLE"},{"source_type":"YDB_SUBSELECT"},{"source_type":"ORACLE_TABLE"},{"source_type":"APPMETRICA_API"},{"source_type":"CHYT_YTSAURUS_SUBSELECT"},{"source_type":"CH_SUBSELECT"},{"source_type":"METRIKA_API"},{"source_type":"PG_SUBSELECT"},{"source_type":"ORACLE_SUBSELECT"},{"source_type":"CH_TABLE"},{"source_type":"MSSQL_SUBSELECT"},{"source_type":"PG_TABLE"},{"source_type":"YDB_TABLE"},{"source_type":"CHYT_YTSAURUS_TABLE_LIST"},{"source_type":"TRINO_SUBSELECT"},{"source_type":"CHYT_YTSAURUS_TABLE"},{"source_type":"CHYT_YTSAURUS_TABLE_RANGE"},{"source_type":"GP_SUBSELECT"},{"source_type":"MYSQL_TABLE"},{"source_type":"TESTING"},{"source_type":"GP_TABLE"},{"source_type":"MSSQL_TABLE"}]},"data_types":{"items":[{"aggregations":["count","countunique"],"type":"string","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","STARTSWITH","ISTARTSWITH","ENDSWITH","IENDSWITH","CONTAINS","ICONTAINS","NOTCONTAINS","NOTICONTAINS","NOTICONTAINS","IN","NIN","BETWEEN"],"casts":["string","date","genericdatetime","boolean","integer","float","geopoint","geopolygon"]},{"aggregations":["count","sum","avg","min","max","countunique"],"type":"integer","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","STARTSWITH","ISTARTSWITH","ENDSWITH","IENDSWITH","CONTAINS","ICONTAINS","NOTCONTAINS","NOTICONTAINS","NOTICONTAINS","IN","NIN","BETWEEN"],"casts":["integer","float","string","boolean","date","genericdatetime"]},{"aggregations":["count","sum","avg","min","max","countunique"],"type":"float","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["float","integer","string","boolean","date","genericdatetime"]},{"aggregations":["count","min","max","countunique","avg"],"type":"date","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["date","genericdatetime","string","integer","float","boolean"]},{"aggregations":["count","min","max","countunique","avg"],"type":"datetime","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["genericdatetime","datetime","date","string","integer","float","boolean"]},{"aggregations":["count"],"type":"boolean","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","IN","NIN"],"casts":["boolean","integer","float","string"]},{"aggregations":["count"],"type":"geopoint","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","IN","NIN"],"casts":["geopoint","string"]},{"aggregations":["count"],"type":"geopolygon","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","IN","NIN"],"casts":["geopolygon","string"]},{"aggregations":["count","countunique"],"type":"uuid","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","STARTSWITH","ISTARTSWITH","ENDSWITH","IENDSWITH","CONTAINS","ICONTAINS","NOTCONTAINS","NOTICONTAINS","NOTICONTAINS","IN","NIN","BETWEEN"],"casts":["uuid","string"]},{"aggregations":["count"],"type":"markup","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL"],"casts":["markup"]},{"aggregations":["count","min","max","countunique","avg"],"type":"datetimetz","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["datetimetz","genericdatetime","date","string","integer","float","boolean"]},{"aggregations":[],"type":"unsupported","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL"],"casts":["unsupported"]},{"aggregations":["count","countunique"],"type":"array_str","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","STARTSWITH","CONTAINS","NOTCONTAINS","LENEQ","LENNE","LENGT","LENGTE","LENLT","LENLTE"],"casts":["array_str","string"]},{"aggregations":["count","countunique"],"type":"array_int","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","STARTSWITH","CONTAINS","NOTCONTAINS","LENEQ","LENNE","LENGT","LENGTE","LENLT","LENLTE"],"casts":["array_int","string"]},{"aggregations":["count","countunique"],"type":"array_float","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","STARTSWITH","CONTAINS","NOTCONTAINS","LENEQ","LENNE","LENGT","LENGTE","LENLT","LENLTE"],"casts":["array_float","string"]},{"aggregations":["count","countunique"],"type":"tree_str","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","STARTSWITH","CONTAINS","NOTCONTAINS","LENEQ","LENNE","LENGT","LENGTE","LENLT","LENLTE"],"casts":["tree_str"]},{"aggregations":["count","min","max","countunique"],"type":"genericdatetime","filter_operations":["ISNULL","ISNOTNULL","ISNOTNULL","EQ","NE","GT","GTE","LT","LTE","IN","NIN","BETWEEN"],"casts":["genericdatetime","date","string","integer","float","boolean"]}]}}"##;

fn casts_for(user_type: &str) -> Value {
    let casts: &[&str] = match user_type {
        "string" => &["string", "date", "genericdatetime", "boolean", "integer", "float", "geopoint", "geopolygon"],
        "integer" => &["integer", "float", "string", "boolean", "date", "genericdatetime"],
        "float" => &["float", "integer", "string", "boolean", "date", "genericdatetime"],
        "date" => &["date", "genericdatetime", "string", "integer", "float", "boolean"],
        "datetime" => &["genericdatetime", "datetime", "date", "string", "integer", "float", "boolean"],
        "datetimetz" => &["datetimetz", "genericdatetime", "date", "string", "integer", "float", "boolean"],
        "genericdatetime" => &["genericdatetime", "date", "string", "integer", "float", "boolean"],
        "boolean" => &["boolean", "integer", "float", "string"],
        "geopoint" => &["geopoint", "string"],
        "geopolygon" => &["geopolygon", "string"],
        "uuid" => &["uuid", "string"],
        "markup" => &["markup"],
        "array_str" => &["array_str", "string"],
        "array_int" => &["array_int", "string"],
        "array_float" => &["array_float", "string"],
        "tree_str" => &["tree_str"],
        _ => &[],
    };
    json!(casts)
}

fn aggregations_for(user_type: &str) -> Vec<&'static str> {
    match user_type {
        "string" => vec!["none", "count", "countunique"],
        "integer" | "float" => vec!["none", "count", "sum", "avg", "min", "max", "countunique"],
        "date" | "datetime" | "datetimetz" => vec!["none", "count", "min", "max", "countunique", "avg"],
        "genericdatetime" => vec!["none", "count", "min", "max", "countunique"],
        "uuid" | "array_str" | "array_int" | "array_float" | "tree_str" => {
            vec!["none", "count", "countunique"]
        }
        "unsupported" => vec![],
        _ => vec!["none", "count"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(guid: &str, data_type: &str) -> Value {
        json!({
            "cast": data_type,
            "guid": guid,
            "type": "DIMENSION",
            "title": guid,
            "valid": true,
            "hidden": false,
            "source": guid,
            "formula": "",
            "avatar_id": "avatar-1",
            "calc_mode": "direct",
            "data_type": data_type,
            "managed_by": "user",
            "aggregation": "none",
            "description": "",
            "guid_formula": "",
            "default_value": null,
            "lock_aggregation": false,
            "value_constraint": null,
            "initial_data_type": data_type,
            "has_auto_aggregation": false,
        })
    }

    /// A small direct-table dataset in the shape the US entry stores it.
    fn stored_dataset_entry() -> Value {
        let mut parameter_field = field("scale", "float");
        parameter_field["calc_mode"] = json!("parameter");
        parameter_field["default_value"] = json!({"type": "float", "value": 0.25});
        let raw_schema = vec![
            json!({"name": "amount", "type": "float", "title": "amount", "nullable": true, "description": "", "native_type": {"name": "numeric", "nullable": true, "conn_type": "postgres", "native_type_class_name": "common_native_type"}, "lock_aggregation": false, "has_auto_aggregation": false}),
            json!({"name": "category", "type": "string", "title": "category", "nullable": true, "description": "", "native_type": {"name": "text", "nullable": true, "conn_type": "postgres", "native_type_class_name": "common_native_type"}, "lock_aggregation": false, "has_auto_aggregation": false}),
        ];
        let origin = json!({
            "parameters": {"table_name": "orders", "schema_name": "public", "db_version": "13.12"},
            "raw_schema": raw_schema,
            "created_from": "PG_TABLE",
            "connection_id": "gz2tnvb9zdxm5",
        });
        let collection = json!({
            "id": "source-1",
            "type": "collection",
            "title": "orders",
            "valid": true,
            "managed_by": "user",
            "origin": origin,
            "sample": null,
            "materialization": null,
        });
        let avatar = json!({
            "id": "avatar-1",
            "title": "public.orders",
            "valid": true,
            "is_root": true,
            "source_id": "source-1",
            "managed_by": "user",
        });
        let data = json!({
            "rls": [],
            "name": "",
            "revision_id": "BEqFzq7VhoaCFRYeh",
            "component_errors": {"items": []},
            "result_schema_aux": {"inter_dependencies": {"deps": []}},
            "obligatory_filters": [],
            "avatar_relations": [],
            "result_schema": [field("amount", "float"), field("category", "string"), parameter_field],
            "source_avatars": [avatar],
            "source_collections": [collection],
        });
        let mut entry = json!({
            "entryId": "dsid",
            "scope": "dataset",
            "type": "",
            "key": "1507164767695933207/Cohorts",
            "createdAt": "2023-09-11T08:47:20.751Z",
            "updatedAt": "2023-11-24T13:39:02.195Z",
            "workbookId": "z4wtz6tg5194o",
            "collectionId": null,
            "isFavorite": false,
            "permissions": {"read": true},
            "annotation": {"description": "demo dataset"},
        });
        entry["data"] = data;
        entry
    }

    fn conn_types() -> HashMap<String, String> {
        HashMap::from([("gz2tnvb9zdxm5".to_string(), "postgres".to_string())])
    }

    #[test]
    fn fields_payload_keeps_the_six_control_api_keys() {
        let data_api = json!({"fields": [
            {"title": "COUNTS", "guid": "g1", "data_type": "integer", "cast": "integer", "hidden": false, "type": "MEASURE", "calc_mode": "formula", "ui_settings": "", "description": "", "formula": "COUNTD([X])", "aggregation": "none", "default_value": null},
            {"title": "ClientID", "guid": "g2", "data_type": "string", "cast": "string", "hidden": true, "type": "DIMENSION", "calc_mode": "direct", "ui_settings": "", "description": "", "formula": "", "aggregation": "none", "default_value": null},
        ]});
        let payload = fields_payload(&data_api);
        let fields = payload["fields"].as_array().unwrap();
        assert_eq!(fields.len(), 2);
        let keys: BTreeSet<&str> = fields[0].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            BTreeSet::from(["title", "guid", "data_type", "hidden", "type", "calc_mode"])
        );
        assert_eq!(fields[0]["title"], "COUNTS");
        assert_eq!(fields[1]["hidden"], true);
    }

    #[test]
    fn dataset_content_derives_the_computed_field_properties() {
        let content = dataset_content(&stored_dataset_entry());
        let fields = content["result_schema"].as_array().unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0]["virtual"], false);
        assert_eq!(fields[0]["ui_settings"], "");
        assert_eq!(fields[0]["autoaggregated"], false);
        assert_eq!(fields[0]["aggregation_locked"], false);
        // A parameter field keeps the scalar default and reports template_enabled.
        assert_eq!(fields[2]["default_value"], 0.25);
        assert_eq!(fields[2]["template_enabled"], false);
        let source = &content["sources"][0];
        assert_eq!(source["source_type"], "PG_TABLE");
        assert_eq!(source["connection_id"], "gz2tnvb9zdxm5");
        assert_eq!(source["raw_schema"][0]["user_type"], "float");
        assert!(source["raw_schema"][0]["native_type"].get("conn_type").is_none());
        assert_eq!(content["description"], "demo dataset");
        assert_eq!(content["rls"], json!({}));
        assert_eq!(content["source_avatars"][0]["virtual"], false);
    }

    #[test]
    fn param_hashes_match_the_python_reference() {
        // Captured from the Python control-api for the demo datasets.
        let cases: [(&str, &str, &str, &str); 2] = [
            (
                "PG_TABLE",
                "gz2tnvb9zdxm5",
                "4eb6d5f1eb0d945a",
                r#"{"table_name":"orders","db_name":null,"schema_name":"public","db_version":"PostgreSQL 13.12 on aarch64-unknown-linux-musl, compiled by gcc (Alpine 12.2.1_git20220924-r10) 12.2.1 20220924, 64-bit"}"#,
            ),
            (
                "PG_TABLE",
                "gz2tnvb9zdxm5",
                "42fa5c2274b9b1b8",
                r#"{"table_name":"managers","db_name":null,"schema_name":"public","db_version":"PostgreSQL 13.12 on aarch64-unknown-linux-musl, compiled by gcc (Alpine 12.2.1_git20220924-r10) 12.2.1 20220924, 64-bit"}"#,
            ),
        ];
        for (source_type, connection_id, expected, parameters) in cases {
            let parameters: Map<String, Value> = serde_json::from_str(parameters).unwrap();
            assert_eq!(
                parameter_hash(source_type, connection_id, &parameters),
                expected,
                "parameters: {parameters:?}"
            );
        }
    }

    #[test]
    fn xxh64_matches_the_published_vectors() {
        let cases = [
            ("", 0xef46_db37_51d8_e999u64),
            ("a", 0xd24e_c4f1_a98c_6e5b),
            ("abc", 0x44bc_2cf5_ad77_0999),
        ];
        for (input, expected) in cases {
            assert_eq!(xxh64(input.as_bytes(), 0), expected, "input: {input:?}");
        }
    }

    #[test]
    fn dataset_export_maps_connections_and_drops_volatile_keys() {
        let entry = stored_dataset_entry();
        let mut id_mapping = Map::new();
        id_mapping.insert("gz2tnvb9zdxm5".to_string(), json!("fake-conn"));
        let payload = dataset_export_payload(&entry, &id_mapping);
        let dataset = &payload["dataset"];
        assert_eq!(dataset["name"], "Cohorts");
        assert_eq!(dataset["revision_id"], Value::Null);
        assert!(dataset.get("rls").is_none());
        assert_eq!(dataset["sources"][0]["connection_id"], "fake-conn");
        assert_eq!(payload["notifications"], json!([]));
    }

    #[test]
    fn dataset_export_without_a_mapping_returns_the_critical_notification() {
        let entry = stored_dataset_entry();
        let payload = dataset_export_payload(&entry, &Map::new());
        assert_eq!(payload["notifications"][0]["code"], DS_EXPORT_CODE);
        assert_eq!(payload["notifications"][0]["level"], "critical");
        assert!(payload.get("dataset").is_none());
    }

    #[test]
    fn dataset_import_reverses_the_export_shape() {
        let entry = stored_dataset_entry();
        let mut export_mapping = Map::new();
        export_mapping.insert("gz2tnvb9zdxm5".to_string(), json!("fake-conn"));
        let export = dataset_export_payload(&entry, &export_mapping);
        let mut import_mapping = Map::new();
        import_mapping.insert("fake-conn".to_string(), json!("real-conn"));
        let body = json!({"data": {"workbook_id": "z4wtz6tg5194o", "dataset": export["dataset"]}, "id_mapping": import_mapping});
        let import = dataset_import_body(&body, &conn_types()).unwrap();
        assert_eq!(import["scope"], "dataset");
        assert_eq!(import["name"], "Cohorts");
        assert_eq!(import["workbookId"], "z4wtz6tg5194o");
        let data = &import["data"];
        assert_eq!(data["rls"], json!([]));
        assert_eq!(data["source_collections"][0]["origin"]["connection_id"], "real-conn");
        assert_eq!(data["source_collections"][0]["origin"]["created_from"], "PG_TABLE");
        assert_eq!(data["source_collections"][0]["origin"]["raw_schema"][0]["type"], "float");
        // The scalar default goes back to the typed US storage value.
        assert_eq!(data["result_schema"][2]["default_value"], json!({"type": "float", "value": 0.25}));
        assert!(data["result_schema"][0].get("ui_settings").is_none());
        assert_eq!(data["result_schema"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn dataset_import_without_a_connection_mapping_fails() {
        let entry = stored_dataset_entry();
        let export = dataset_export_payload(&entry, &Map::new());
        let body = json!({"data": {"workbook_id": "w", "dataset": export["dataset"]}, "id_mapping": {}});
        // The export failed with a notification instead of a dataset: import reports a
        // missing dataset field.
        assert!(dataset_import_body(&body, &conn_types()).is_err());
        let body = json!({"data": {"workbook_id": "w", "dataset": {
            "name": "x", "result_schema": [], "sources": [{"id": "s", "connection_id": "missing"}],
        }}, "id_mapping": {"other": "real"}});
        assert_eq!(
            dataset_import_body(&body, &conn_types()).unwrap_err(),
            IMPORT_MISSING_CONNECTION
        );
    }

    #[test]
    fn dataset_options_has_the_editor_shape() {
        let entry = stored_dataset_entry();
        let content = dataset_content(&entry);
        let options = dataset_options(&content, &conn_types());
        for key in [
            "schema_update_enabled",
            "connections",
            "source_avatars",
            "supports_offset",
            "supported_functions",
            "source_listing",
            "fields",
            "join",
            "preview",
            "sources",
            "data_types",
        ] {
            assert!(options.get(key).is_some(), "missing option {key}");
        }
        assert_eq!(options["sources"]["items"][0]["schema_update_enabled"], true);
        assert_eq!(options["connections"]["items"][0]["id"], "gz2tnvb9zdxm5");
        assert_eq!(options["connections"]["max"], 1);
        let fields = options["fields"]["items"].as_array().unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0]["casts"], json!(["float", "integer", "string", "boolean", "date", "genericdatetime"]));
        assert_eq!(options["supported_functions"].as_array().unwrap().len(), 163);
        assert_eq!(options["data_types"]["items"].as_array().unwrap().len(), 17);
        assert_eq!(options["source_listing"]["supports_source_search"], true);
    }

    #[test]
    fn dataset_response_carries_the_top_level_keys() {
        let entry = stored_dataset_entry();
        let content = dataset_content(&entry);
        let options = dataset_options(&content, &conn_types());
        let response = dataset_response(&entry, options);
        for key in [
            "options",
            "workbook_id",
            "is_favorite",
            "full_permissions",
            "dataset",
            "key",
            "permissions",
            "name",
            "id",
        ] {
            assert!(response.get(key).is_some(), "missing {key}");
        }
        assert_eq!(response["name"], "Cohorts");
        assert_eq!(response["workbook_id"], "z4wtz6tg5194o");
        assert_eq!(response["permissions"], json!({"read": true}));
        assert_eq!(response["is_favorite"], false);
    }

    #[test]
    fn field_validation_payload_matches_the_reference() {
        let ok = FormulaIssue {
            code: "FORMULA.PARSE.UNEXPECTED_EOF",
            message: "Failed to parse: unexpected end of formula".to_string(),
            token: None,
            column: Some(4),
            row: Some(0),
        };
        let token = FormulaIssue {
            code: "FORMULA.PARSE.UNEXPECTED_TOKEN",
            message: "Failed to parse formula: unexpected token".to_string(),
            token: Some("+".to_string()),
            column: Some(14),
            row: Some(0),
        };
        let cases = [
            ("SUM(", Some(&ok), true, 400, "FORMULA.PARSE.UNEXPECTED_EOF"),
            ("1+1", None, true, 200, "OK"),
            ("NOSUCHFIELD_X + 1", Some(&token), true, 400, "FORMULA.PARSE.UNEXPECTED_TOKEN"),
            ("1+1", None, false, 400, ""),
        ];
        for (formula, issue, has_sources, status, code) in cases {
            let (actual_status, payload) = field_validation_payload("X", "x", issue, has_sources);
            assert_eq!(actual_status, status, "formula: {formula:?}");
            if status == 200 {
                assert_eq!(payload["code"], code, "formula: {formula:?}");
            } else if has_sources {
                assert_eq!(payload["code"], CODE_VALIDATION_ERROR, "formula: {formula:?}");
                assert_eq!(
                    payload["field_errors"][0]["errors"][0]["code"], code,
                    "formula: {formula:?}"
                );
            } else {
                assert_eq!(payload["field_errors"][0]["errors"][0]["code"], code);
            }
            if !has_sources {
                let error = &payload["field_errors"][0]["errors"][0];
                assert_eq!(error["message"], "No data sources have been defined");
                assert_eq!(error["level"], "ERROR");
            }
            if status == 400 && has_sources {
                assert_eq!(payload["message"], "Validation finished with errors.");
                assert_eq!(payload["field_errors"][0]["guid"], "x");
            }
            if status == 200 {
                assert_eq!(payload["message"], "Validation was successful");
                assert_eq!(payload["field_errors"], json!([]));
            }
        }
    }

    #[test]
    fn formula_issue_parses_like_the_reference() {
        assert!(formula_issue("1+1", None, Some("postgres")).is_none());
        assert!(formula_issue("  ", None, Some("postgres")).is_none());
        let eof = formula_issue("SUM(", None, Some("postgres")).unwrap();
        assert_eq!(eof.code, "FORMULA.PARSE.UNEXPECTED_EOF");
        assert_eq!(eof.column, Some(4));
    }

    #[test]
    fn dataset_validation_payload_marks_ok_and_errors() {
        let content = dataset_content(&stored_dataset_entry());
        let options = dataset_options(&content, &conn_types());
        let (status, ok) = dataset_validation_payload(&content, options.clone(), &[]);
        assert_eq!(status, 200);
        assert_eq!(ok["code"], "OK");
        assert_eq!(ok["dataset_errors"], json!([]));
        let issue = json!({"guid": "g", "title": "t", "errors": []});
        let (status, bad) = dataset_validation_payload(&content, options, &[issue]);
        assert_eq!(status, 400);
        assert_eq!(bad["code"], CODE_VALIDATION_ERROR);
    }

    #[test]
    fn stored_dataset_data_keeps_real_connection_ids_without_a_mapping() {
        let entry = stored_dataset_entry();
        let content = dataset_content(&entry);
        let stored = stored_dataset_data(content.as_object().unwrap(), None, &conn_types()).unwrap();
        assert_eq!(
            stored["source_collections"][0]["origin"]["connection_id"],
            "gz2tnvb9zdxm5"
        );
        assert_eq!(stored["revision_id"], Value::Null);
        assert_eq!(stored["result_schema"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn merge_stored_dataset_keeps_storage_only_keys() {
        let existing = json!({
            "name": "",
            "created_via": "workbook_copy",
            "revision_id": "OLD",
            "result_schema": [{"guid": "old"}],
            "source_collections": [],
        });
        let incoming = json!({
            "name": "",
            "revision_id": Value::Null,
            "result_schema": [{"guid": "new"}],
            "source_collections": [],
        });
        let merged = merge_stored_dataset(&existing, incoming);
        assert_eq!(merged["created_via"], "workbook_copy");
        assert_eq!(merged["revision_id"], Value::Null);
        assert_eq!(merged["result_schema"][0]["guid"], "new");
    }

    #[test]
    fn dataset_write_response_only_has_id_for_create() {
        let entry = stored_dataset_entry();
        let stored = entry["data"].clone();
        let created = dataset_write_response(Some("newid"), &stored, "demo", &conn_types());
        assert_eq!(created["id"], "newid");
        assert_eq!(created["dataset"]["description"], "demo");
        assert!(created["options"].is_object());
        let updated = dataset_write_response(None, &stored, "demo", &conn_types());
        assert!(updated.get("id").is_none());
        assert_eq!(updated.as_object().unwrap().len(), 2);
    }

    #[test]
    fn empty_dataset_options_match_the_reference() {
        let content = json!({"sources": [], "source_avatars": [], "result_schema": [], "preview_enabled": true});
        let options = dataset_options(&content, &HashMap::new());
        assert_eq!(options["source_avatars"]["max"], 32);
        assert_eq!(options["connections"]["max"], 20);
        assert_eq!(
            options["connections"]["compatible_types"].as_array().unwrap().len(),
            11
        );
        assert_eq!(options["supported_functions"].as_array().unwrap().len(), 167);
        assert_eq!(options["data_types"]["items"].as_array().unwrap().len(), 17);
        assert_eq!(options["preview"]["enabled"], true);
        assert!(options.get("source_listing").is_none());
    }
}
