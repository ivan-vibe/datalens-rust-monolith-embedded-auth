//! `dl_core.component_errors` plus the collecting half of `DatasetValidator`.
//!
//! `Dataset::validate` answers "can this dataset be queried at all" and stops at the first
//! problem, which is what the query path needs. `/result-preflight` needs the other half:
//! *every* problem of every field, in one registry, so the UI can highlight them all at once.
//! Both are produced from the same [`dataset_issues`] walk, so they never disagree.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::dataset::{CalcMode, Dataset};
use crate::error::CoreError;

/// `dl_constants.enums.ComponentErrorLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentErrorLevel {
    #[default]
    Error,
    Warning,
}

/// `dl_constants.enums.ComponentType`: which part of the dataset an error belongs to.
///
/// The port only validates fields today, but the enum is the one `dl_api_connector`'s
/// `ComponentErrorListSchema` serialises, so every member is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentType {
    DataSource,
    SourceAvatar,
    AvatarRelation,
    #[default]
    Field,
    ObligatoryFilter,
    ResultSchema,
    ExtractFilter,
    ExtractSorting,
    ExtractProperties,
}

/// `dl_core.component_errors.ComponentError`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentError {
    #[serde(default)]
    pub level: ComponentErrorLevel,
    pub message: String,
    /// The dotted API code, e.g. `ERR.DS_API.FIELD.INVALID` (`ErrorCodeField` joins the parts).
    pub code: String,
    #[serde(default)]
    pub details: serde_json::Map<String, serde_json::Value>,
}

impl ComponentError {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            level: ComponentErrorLevel::Error,
            message: message.into(),
            code: code.into(),
            details: serde_json::Map::new(),
        }
    }
}

/// `ComponentErrorPack`: the errors of one component, keyed by its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentErrorPack {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: ComponentType,
    pub errors: Vec<ComponentError>,
}

/// `ComponentErrorRegistry`: what `/result-preflight` sends as `component_errors`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentErrorRegistry {
    #[serde(default)]
    pub items: Vec<ComponentErrorPack>,
}

impl ComponentErrorRegistry {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Add an error to the pack of `id`, creating the pack when it is the first one.
    pub fn add(&mut self, id: impl Into<String>, kind: ComponentType, error: ComponentError) {
        let id = id.into();
        if let Some(pack) = self.items.iter_mut().find(|pack| pack.id == id) {
            pack.errors.push(error);
        } else {
            self.items.push(ComponentErrorPack {
                id,
                kind,
                errors: vec![error],
            });
        }
    }

    /// Every error of every pack, in registry order.
    pub fn errors(&self) -> impl Iterator<Item = &ComponentError> {
        self.items.iter().flat_map(|pack| pack.errors.iter())
    }

    pub fn get(&self, id: &str) -> Option<&ComponentErrorPack> {
        self.items.iter().find(|pack| pack.id == id)
    }
}

/// One problem of one dataset: `field` is `None` for dataset-level errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatasetIssue {
    pub field: Option<String>,
    pub error: CoreError,
}

impl DatasetIssue {
    fn of_field(field: &crate::dataset::Field, error: CoreError) -> Self {
        Self {
            field: Some(field.guid.clone()),
            error,
        }
    }

    fn of_dataset(error: CoreError) -> Self {
        Self { field: None, error }
    }
}

/// Every problem of `dataset`, in field order, instead of only the first one.
///
/// The checks are `DatasetValidator`'s: guids and titles are unique, every field has a type,
/// a formula field has a formula, a direct field has a source to read from.
pub fn dataset_issues(dataset: &Dataset) -> Vec<DatasetIssue> {
    if dataset.fields.is_empty() {
        return vec![DatasetIssue::of_dataset(CoreError::EmptyDataset)];
    }

    let mut issues: Vec<DatasetIssue> = Vec::new();
    let mut guids: HashMap<&str, usize> = HashMap::with_capacity(dataset.fields.len());
    let mut titles: HashMap<&str, usize> = HashMap::with_capacity(dataset.fields.len());
    for field in &dataset.fields {
        if field.guid.trim().is_empty() {
            issues.push(DatasetIssue::of_field(
                field,
                CoreError::EmptyGuid {
                    guid: field.guid.clone(),
                },
            ));
        } else if guids.insert(field.guid.as_str(), 0).is_some() {
            issues.push(DatasetIssue::of_field(
                field,
                CoreError::DuplicateGuid(field.guid.clone()),
            ));
        }
        if titles.insert(field.title.as_str(), 0).is_some() {
            issues.push(DatasetIssue::of_field(
                field,
                CoreError::DuplicateTitle(field.title.clone()),
            ));
        }
        if let Err(error) = field.dtype() {
            issues.push(DatasetIssue::of_field(field, error));
        }
        match field.calc_mode {
            CalcMode::Formula => {
                if field.formula_text().trim().is_empty() {
                    issues.push(DatasetIssue::of_field(
                        field,
                        CoreError::MissingFormula {
                            guid: field.guid.clone(),
                        },
                    ));
                }
            }
            // A parameter is a literal: it has neither a column nor a source.
            CalcMode::Parameter => {}
            CalcMode::Direct => {
                if let Some(source) = &field.avatar_id {
                    if !dataset.sources.is_empty()
                        && !dataset
                            .sources
                            .iter()
                            .any(|candidate| &candidate.id == source)
                    {
                        issues.push(DatasetIssue::of_field(
                            field,
                            CoreError::UnknownSource {
                                guid: field.guid.clone(),
                                source_id: source.clone(),
                            },
                        ));
                    }
                } else if dataset.sources.is_empty() {
                    issues.push(DatasetIssue::of_field(
                        field,
                        CoreError::UnrenderableField {
                            guid: field.guid.clone(),
                            reason: "the dataset has no sources".to_string(),
                        },
                    ));
                }
            }
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::{Field, Source};
    use crate::dtype::UserDataType;

    fn dataset(fields: Vec<Field>) -> Dataset {
        Dataset {
            sources: vec![Source::table("src", "public.sales")],
            fields,
            data_export_forbidden: false,
            connection_id: None,
            ..Dataset::default()
        }
    }

    fn field(guid: &str, title: &str) -> Field {
        Field {
            guid: guid.to_string(),
            title: title.to_string(),
            data_type: Some(UserDataType::String),
            source: title.to_string(),
            ..Field::default()
        }
    }

    #[test]
    fn a_valid_dataset_has_no_issues() {
        let dataset = dataset(vec![field("f_a", "A"), field("f_b", "B")]);
        assert!(dataset_issues(&dataset).is_empty());
        assert!(dataset.validate().is_ok());
    }

    #[test]
    fn every_field_is_checked_not_only_the_first_one() {
        let mut broken = field("f_b", "B");
        broken.data_type = None;
        broken.initial_data_type = None;
        let mut no_formula = field("f_c", "C");
        no_formula.calc_mode = CalcMode::Formula;
        let dataset = dataset(vec![field("f_a", "A"), broken, no_formula]);

        let issues = dataset_issues(&dataset);
        let guids: Vec<&str> = issues
            .iter()
            .filter_map(|issue| issue.field.as_deref())
            .collect();
        assert_eq!(guids, vec!["f_b", "f_c"]);
        // `validate` keeps the fail-fast contract: the first issue is the error it returns.
        assert_eq!(dataset.validate().unwrap_err(), issues[0].error);
    }

    #[test]
    fn a_duplicate_title_is_reported_on_the_second_field() {
        let dataset = dataset(vec![field("f_a", "A"), field("f_b", "A")]);
        let issues = dataset_issues(&dataset);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].field.as_deref(), Some("f_b"));
        assert_eq!(issues[0].error, CoreError::DuplicateTitle("A".to_string()));
    }

    #[test]
    fn an_empty_dataset_is_a_dataset_level_issue() {
        let issues = dataset_issues(&dataset(Vec::new()));
        assert_eq!(issues.len(), 1);
        assert!(issues[0].field.is_none());
        assert_eq!(issues[0].error, CoreError::EmptyDataset);
    }

    #[test]
    fn errors_are_grouped_per_field() {
        let mut registry = ComponentErrorRegistry::default();
        registry.add(
            "f_a",
            ComponentType::Field,
            ComponentError::error("ERR.DS_API.FIELD.INVALID", "Field is invalid"),
        );
        registry.add(
            "f_a",
            ComponentType::Field,
            ComponentError::error("ERR.DS_API.FIELD.TITLE.CONFLICT", "conflict"),
        );
        registry.add(
            "f_b",
            ComponentType::Field,
            ComponentError::error("ERR.DS_API.FIELD.INVALID", "Field is invalid"),
        );
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.get("f_a").unwrap().errors.len(), 2);
        assert_eq!(registry.errors().count(), 3);
        // The wire shape of `ComponentErrorListSchema`.
        let json = serde_json::to_value(&registry).unwrap();
        assert_eq!(json["items"][0]["type"], "field");
        assert_eq!(json["items"][0]["errors"][0]["level"], "error");
        assert_eq!(
            json["items"][0]["errors"][0]["details"],
            serde_json::json!({})
        );
    }
}
