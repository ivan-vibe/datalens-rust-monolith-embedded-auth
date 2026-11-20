//! `us` dataset JSON -> `dl_core::Dataset`.
//!
//! The stored dataset keeps fields in `result_schema` and sources in two parallel lists:
//! `source_collections` (where the data is) and `source_avatars` (the ids fields point at).
//! The query translator only needs "avatar id -> table", so the two are joined here.
//!
//! Two naming details matter and are easy to get wrong:
//!
//! * a `direct` field has `source` = the **physical column** and `avatar_id` = the avatar,
//!   which is the opposite of the terse naming in `dl_core::Field`;
//! * `parameter` fields keep `calc_mode = parameter` and their `default_value`; the compiler
//!   substitutes the request `parameter_values`, else this default, as a literal.

use dl_core::{Aggregation, CalcMode, Dataset, Field, FieldType, Source, UserDataType};
use tracing::warn;

use crate::entry::{UsDatasetData, UsField, UsParameters};
use crate::error::UsError;

/// Build the dataset the query translator works with.
pub fn to_dataset(entry_id: &str, data: &UsDatasetData) -> Result<Dataset, UsError> {
    let invalid = |reason: String| UsError::Invalid {
        entry_id: entry_id.to_string(),
        reason,
    };

    let mut fields = Vec::with_capacity(data.result_schema.len());
    for raw in &data.result_schema {
        if let Some(field) = map_field(raw).map_err(invalid)? {
            fields.push(field);
        }
    }

    let mut rls = data.rls.clone();
    rls.extend_rls2(&data.rls2);
    let dataset = Dataset {
        sources: map_sources(data),
        fields,
        data_export_forbidden: data.data_export_forbidden,
        connection_id: connection_id_of(data),
        avatar_relations: map_relations(data),
        root_avatar_id: root_avatar_id(data),
        rls,
    };
    dataset
        .validate()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(dataset)
}

/// `source_collections[].origin.connection_id`: the connection the query has to be run against.
///
/// One dataset can in principle name several connections (one per collection); the port
/// executes a single query, so the first one wins — the same source the `FROM` clause reads.
pub fn connection_id_of(data: &UsDatasetData) -> Option<String> {
    data.source_collections
        .iter()
        .filter(|collection| collection.valid)
        .find_map(|collection| {
            collection
                .origin
                .as_ref()
                .and_then(|origin| origin.connection_id.clone())
                .filter(|id| !id.trim().is_empty())
        })
}

/// `avatar_relations` -> [`dl_core::AvatarRelation`]s the compiler can join on.
///
/// A relation without both ends or without a condition cannot become a `JOIN`, so it is dropped
/// here rather than failing the query: a dataset with a broken relation still answers the
/// requests that read a single avatar.
pub fn map_relations(data: &UsDatasetData) -> Vec<dl_core::AvatarRelation> {
    data.avatar_relations
        .iter()
        .filter(|relation| relation.left_avatar_id.is_some() && relation.right_avatar_id.is_some())
        .filter(|relation| !relation.conditions.is_empty())
        .map(|relation| dl_core::AvatarRelation {
            id: relation.id.clone(),
            left_avatar_id: relation.left_avatar_id.clone().unwrap_or_default(),
            right_avatar_id: relation.right_avatar_id.clone().unwrap_or_default(),
            join_type: relation
                .join_type
                .as_deref()
                .and_then(dl_core::JoinType::parse)
                .unwrap_or_default(),
            conditions: relation.conditions.clone(),
        })
        .collect()
}

/// `source_avatars[].is_root` -> the avatar the `FROM` clause starts from.
pub fn root_avatar_id(data: &UsDatasetData) -> Option<String> {
    let avatar = data
        .source_avatars
        .iter()
        .find(|avatar| avatar.is_root)
        .or_else(|| data.source_avatars.first())?;
    Some(avatar.id.clone())
}

/// Join `source_avatars` with the collection they point at.
pub fn map_sources(data: &UsDatasetData) -> Vec<Source> {
    let mut sources = Vec::with_capacity(data.source_avatars.len());
    for avatar in data.source_avatars.iter().filter(|avatar| avatar.valid) {
        let origin = avatar
            .source_id
            .as_deref()
            .and_then(|source_id| {
                data.source_collections
                    .iter()
                    .find(|collection| collection.id == source_id)
            })
            .and_then(|collection| collection.origin.as_ref());
        // `SubselectDataSourceSpec`: a "Custom SQL" source has no table at all, it carries the
        // user's SQL in `parameters.subsql` (`SubselectDataSource.get_sql_source`).
        if let Some(subsql) = origin
            .and_then(|origin| origin.parameters.as_ref())
            .and_then(|parameters| parameters.subsql.as_deref())
            .map(str::trim)
            .filter(|subsql| !subsql.is_empty())
        {
            sources.push(Source {
                id: avatar.id.clone(),
                table: String::new(),
                connection_id: connection_of(origin),
                subselect: Some(subsql.to_string()),
                columns: columns_of(origin),
            });
            continue;
        }
        let table = origin
            .and_then(|origin| origin.parameters.as_ref())
            .and_then(table_of)
            // A dataset without schema info still names its avatar after the table.
            .or_else(|| {
                let title = avatar.title.trim();
                (!title.is_empty()).then(|| title.to_string())
            });
        match table {
            Some(table) => sources.push(Source {
                id: avatar.id.clone(),
                table,
                connection_id: connection_of(origin),
                subselect: None,
                columns: columns_of(origin),
            }),
            None => warn!(
                avatar = %avatar.id,
                "source avatar has neither a table nor a title, skipping it"
            ),
        }
    }
    sources
}

/// `origin.connection_id` -> the connection the source's table lives in.
fn connection_of(origin: Option<&crate::entry::UsOrigin>) -> Option<String> {
    origin
        .and_then(|origin| origin.connection_id.clone())
        .filter(|id| !id.trim().is_empty())
}

/// `origin.raw_schema` -> [`SourceColumn`]s.
///
/// The `us` stores the user type of a column, which is the type Python gives the `direct`
/// field that reads it (`field.data_type`), so this table is what decides both "does this
/// column exist" and "what is its type". A column whose type did not parse keeps
/// `Unsupported` rather than being dropped: its existence is still known.
fn columns_of(origin: Option<&crate::entry::UsOrigin>) -> Vec<dl_core::SourceColumn> {
    origin
        .map(|origin| {
            origin
                .raw_schema
                .iter()
                .filter(|column| !column.name.trim().is_empty())
                .map(|column| dl_core::SourceColumn {
                    name: column.name.clone(),
                    data_type: column
                        .user_type
                        .as_deref()
                        .and_then(|text| UserDataType::parse(text).ok())
                        .unwrap_or(UserDataType::Unsupported),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `public.orders` for `{schema_name: public, table_name: orders}`.
fn table_of(parameters: &UsParameters) -> Option<String> {
    let table = parameters.table_name.as_deref()?.trim();
    if table.is_empty() {
        return None;
    }
    let schema = match parameters.schema_name.as_deref().map(str::trim) {
        Some(schema) if !schema.is_empty() => Some(schema),
        _ => match parameters.db_name.as_deref().map(str::trim) {
            Some(db_name) if !db_name.is_empty() => Some(db_name),
            _ => None,
        },
    };
    Some(match schema {
        Some(schema) => format!("{schema}.{table}"),
        None => table.to_string(),
    })
}

/// One stored field, or `None` when it cannot be used at all.
fn map_field(raw: &UsField) -> Result<Option<Field>, String> {
    if !raw.valid {
        warn!(guid = %raw.guid, "skipping an invalid field");
        return Ok(None);
    }

    let calc_mode_name = raw.calc_mode.as_deref().unwrap_or("direct");
    let mut field = Field {
        guid: raw.guid.clone(),
        title: if raw.title.is_empty() {
            raw.guid.clone()
        } else {
            raw.title.clone()
        },
        hidden: raw.hidden,
        initial_data_type: data_type(raw.initial_data_type.as_deref()),
        data_type: data_type(raw.data_type.as_deref()),
        cast: data_type(raw.cast.as_deref()),
        field_type: raw.field_type.as_deref().and_then(FieldType::parse),
        aggregation: raw
            .aggregation
            .as_deref()
            .and_then(Aggregation::parse)
            .unwrap_or_default(),
        calc_mode: CalcMode::Direct,
        source: raw.source.clone(),
        avatar_id: raw.avatar_id.clone(),
        formula: raw.formula.clone(),
        guid_formula: raw.guid_formula.clone(),
        description: raw.description.clone(),
        ..Field::default()
    };

    match calc_mode_name {
        "direct" => {}
        "formula" => field.calc_mode = CalcMode::Formula,
        "parameter" => {
            // Parameters stay parameters: the compiler substitutes the request value, else
            // this default, as a literal of the field's own type.
            field.calc_mode = CalcMode::Parameter;
            field.default_value = raw.default_value.as_ref().map(|value| value.value.clone());
        }
        other => {
            return Err(format!(
                "field {} has an unsupported calc_mode {other}",
                raw.guid
            ))
        }
    }

    Ok(Some(field))
}

fn data_type(name: Option<&str>) -> Option<UserDataType> {
    let name = name?.trim();
    if name.is_empty() {
        return None;
    }
    match UserDataType::parse(name) {
        Ok(data_type) => Some(data_type),
        Err(_) => {
            warn!(
                data_type = name,
                "unknown data type, falling back to unsupported"
            );
            Some(UserDataType::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::UsEntry;

    fn dataset() -> Dataset {
        let entry: UsEntry = serde_json::from_value(crate::testing::dataset_entry()).unwrap();
        to_dataset("dataset-1", &entry.dataset_data().unwrap()).unwrap()
    }

    #[test]
    fn avatars_are_joined_with_the_collection_that_holds_the_table() {
        let dataset = dataset();
        assert_eq!(dataset.sources.len(), 1);
        let source = &dataset.sources[0];
        assert_eq!(source.id, "avatar_orders");
        assert_eq!(source.table, "public.orders");
        // The collection's `origin` is also where the connection of the table lives.
        assert_eq!(source.connection_id.as_deref(), Some("conn-1"));
    }

    #[test]
    fn the_connection_id_of_the_collection_is_kept_on_the_dataset() {
        // The transport resolves it into a DSN; the query translator never reads it.
        assert_eq!(dataset().connection_id.as_deref(), Some("conn-1"));
    }

    #[test]
    fn a_direct_field_keeps_the_column_in_source_and_the_avatar_in_avatar_id() {
        let dataset = dataset();
        let region = dataset.field_by_guid("dim_region").unwrap();
        assert_eq!(region.source, "region");
        assert_eq!(region.avatar_id.as_deref(), Some("avatar_orders"));
        assert_eq!(region.physical_column(), "region");
        assert_eq!(region.dtype().unwrap().name(), "STRING");
        assert_eq!(region.field_type, Some(FieldType::Dimension));
        assert_eq!(dataset.source_for(region).unwrap().table, "public.orders");
    }

    #[test]
    fn a_measure_keeps_its_aggregation_and_a_formula_field_its_formula() {
        let dataset = dataset();
        let sales = dataset.field_by_guid("m_sales").unwrap();
        assert_eq!(sales.aggregation, Aggregation::Sum);
        assert_eq!(sales.field_type, Some(FieldType::Measure));
        let doubled = dataset.field_by_guid("f_double").unwrap();
        assert_eq!(doubled.calc_mode, CalcMode::Formula);
        assert_eq!(doubled.formula_text(), "[m_sales] * 2");
    }

    #[test]
    fn a_parameter_keeps_its_calc_mode_and_default_value() {
        let dataset = dataset();
        let scale = dataset.field_by_guid("p_scale").unwrap();
        assert_eq!(scale.calc_mode, CalcMode::Parameter);
        assert!(scale.is_parameter());
        assert_eq!(scale.default_value, Some(serde_json::json!(0.5)));
    }

    #[test]
    fn a_table_falls_back_to_the_avatar_title_and_defaults_its_schema() {
        let mut parameters = UsParameters {
            table_name: Some("orders".to_string()),
            ..UsParameters::default()
        };
        assert_eq!(table_of(&parameters).unwrap(), "orders");
        parameters.schema_name = Some("public".to_string());
        assert_eq!(table_of(&parameters).unwrap(), "public.orders");
        parameters.schema_name = Some("  ".to_string());
        parameters.db_name = Some("shop".to_string());
        assert_eq!(table_of(&parameters).unwrap(), "shop.orders");
        parameters.table_name = None;
        assert!(table_of(&parameters).is_none());
    }

    #[test]
    fn avatar_relations_become_the_joins_of_the_dataset() {
        let entry: UsEntry = serde_json::from_value(serde_json::json!({
            "entryId": "ds",
            "scope": "dataset",
            "type": "dataset",
            "key": "demo/ds",
            "data": {
                "result_schema": [
                    {"guid": "f_manager", "title": "Manager", "calc_mode": "direct",
                     "source": "manager", "avatar_id": "avatar_managers",
                     "initial_data_type": "string", "data_type": "string", "type": "DIMENSION"}
                ],
                "source_collections": [
                    {"id": "c1", "origin": {"parameters": {"schema_name": "public", "table_name": "orders"}}},
                    {"id": "c2", "origin": {"parameters": {"schema_name": "public", "table_name": "managers"}}}
                ],
                "source_avatars": [
                    {"id": "avatar_orders", "source_id": "c1", "title": "public.orders", "is_root": true},
                    {"id": "avatar_managers", "source_id": "c2", "title": "public.managers"}
                ],
                "avatar_relations": [{
                    "id": "rel-1",
                    "left_avatar_id": "avatar_orders",
                    "right_avatar_id": "avatar_managers",
                    "join_type": "inner",
                    "conditions": [{
                        "left_part": {"calc_mode": "direct", "source": "category_name_level_1"},
                        "right_part": {"calc_mode": "direct", "source": "category_name_level_1"},
                        "operator": "eq",
                        "condition_type": "binary"
                    }]
                }]
            }
        }))
        .unwrap();
        let dataset = to_dataset("ds", &entry.dataset_data().unwrap()).unwrap();
        assert_eq!(dataset.sources.len(), 2);
        assert_eq!(dataset.root_avatar_id.as_deref(), Some("avatar_orders"));
        assert_eq!(dataset.avatar_relations.len(), 1);
        let relation = &dataset.avatar_relations[0];
        assert_eq!(relation.left_avatar_id, "avatar_orders");
        assert_eq!(relation.right_avatar_id, "avatar_managers");
        assert_eq!(relation.join_type, dl_core::JoinType::Inner);
        assert_eq!(
            relation.conditions[0].left,
            dl_core::JoinConditionPart::Direct("category_name_level_1".to_string())
        );
    }

    #[test]
    fn an_unknown_data_type_becomes_unsupported_instead_of_failing_the_dataset() {
        assert_eq!(
            data_type(Some("varchar-ish")),
            Some(UserDataType::Unsupported)
        );
        assert_eq!(data_type(Some("")), None);
        assert_eq!(data_type(None), None);
    }
}
