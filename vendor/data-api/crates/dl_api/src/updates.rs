//! The `updates` of a data request, ported from `dl_api_lib.dataset.validator`.
//!
//! Two rules from Python drive everything here:
//!
//! * `DatasetAction.get_actions_whitelist_for_data_api()` is `{add_field, update_field,
//!   delete_field}` — any other action is `ERR.DS_API.ACTION_NOT_ALLOWED`;
//! * `update_field` is a **partial patch**: `DatasetValidator.apply_field_action` drops every
//!   `None` key before touching the field, so a request that changes only `hidden` leaves the
//!   formula alone. That is why [`FieldPatch`] has no defaults: `None` means "not in the
//!   request", which is a different thing from "set to the default".

use dl_core::{CalcMode, Dataset, Field, FieldType, UserDataType};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;

/// `ID_LENGTH` in `dl_core.components.ids`.
pub const ID_LENGTH: usize = 36;

/// `ActionSchema` restricted to the actions `data-api` allows.
///
/// The legacy names (`add`/`update`/`delete`) are accepted too (`DatasetAction.remap_legacy`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UpdateAction {
    #[serde(alias = "add")]
    AddField {
        field: FieldPatch,
        /// Where the new field is placed in `result_schema` (default: first).
        #[serde(default)]
        order_index: Option<usize>,
    },
    #[serde(alias = "update")]
    UpdateField { field: FieldPatch },
    #[serde(alias = "delete")]
    DeleteField { field: DeletedField },
    /// `clone_field`, `add_source`, `replace_connection`, ... — refused, like Python does.
    #[serde(other)]
    Other,
}

/// `{"guid": "..."}`: `delete_field` only needs the id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedField {
    #[serde(default)]
    pub guid: String,
    /// `strict`: an error instead of a silent no-op when the field is missing.
    #[serde(default)]
    pub strict: bool,
}

/// `UpdateFieldBaseSchema`: every key is optional and `None` means "not in the request".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FieldPatch {
    #[serde(default)]
    pub guid: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub calc_mode: Option<CalcMode>,
    #[serde(default)]
    pub hidden: Option<bool>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub aggregation: Option<dl_core::Aggregation>,
    #[serde(default)]
    pub formula: Option<String>,
    #[serde(default)]
    pub guid_formula: Option<String>,
    /// `TYPE_FIELD_NAME` of `UpdateFieldBaseSchema`: the user-facing type of the field.
    #[serde(default)]
    pub cast: Option<UserDataType>,
    /// `DATA_TYPE_FIELD_NAME`. Python patches the stored field with every key the request
    /// sent (`old_field.clone(**field_data_dict)`), and `data_type` is one of them: the UI
    /// sends the recomputed type, and the legend reports it verbatim.
    #[serde(default)]
    pub data_type: Option<UserDataType>,
    /// `INITIAL_DATA_TYPE_FIELD_NAME`: the type before the cast.
    #[serde(default)]
    pub initial_data_type: Option<UserDataType>,
    #[serde(default, rename = "type")]
    pub field_type: Option<FieldType>,
    #[serde(default)]
    pub avatar_id: Option<String>,
    /// `new_id`: the field is re-created under this guid, like `rename_field_id_usages` does.
    #[serde(default)]
    pub new_id: Option<String>,
    #[serde(default, deserialize_with = "dl_core::dataset::de_typed_value")]
    pub default_value: Option<serde_json::Value>,
    #[serde(default)]
    pub ui_settings: Option<String>,
    /// `strict`: raise on a missing/duplicate id instead of returning silently.
    #[serde(default)]
    pub strict: bool,
}

impl FieldPatch {
    /// A brand new field, with the defaults `DatasetValidator` applies to `add_field`.
    fn to_field(&self, guid: &str, title: &str) -> Field {
        let has_formula = self
            .formula
            .as_deref()
            .map(|formula| !formula.trim().is_empty())
            .unwrap_or(false);
        Field {
            guid: guid.to_string(),
            title: title.to_string(),
            hidden: self.hidden.unwrap_or(false),
            initial_data_type: self.initial_data_type,
            data_type: self.data_type,
            cast: self.cast,
            field_type: self.field_type,
            aggregation: self.aggregation.unwrap_or_default(),
            calc_mode: self.calc_mode.unwrap_or(if has_formula {
                CalcMode::Formula
            } else {
                CalcMode::Direct
            }),
            source: self.source.clone().unwrap_or_default(),
            avatar_id: self.avatar_id.clone(),
            formula: self.formula.clone().unwrap_or_default(),
            guid_formula: self.guid_formula.clone().unwrap_or_default(),
            description: self.description.clone().unwrap_or_default(),
            ui_settings: self.ui_settings.clone().unwrap_or_default(),
            default_value: self.default_value.clone(),
        }
    }

    /// Apply the keys that are present to an existing field.
    fn apply_to(&self, field: &mut Field) {
        if let Some(title) = &self.title {
            field.title = title.clone();
        }
        if let Some(source) = &self.source {
            field.source = source.clone();
        }
        if let Some(calc_mode) = self.calc_mode {
            field.calc_mode = calc_mode;
        }
        if let Some(hidden) = self.hidden {
            field.hidden = hidden;
        }
        if let Some(description) = &self.description {
            field.description = description.clone();
        }
        if let Some(aggregation) = self.aggregation {
            field.aggregation = aggregation;
        }
        if let Some(cast) = self.cast {
            field.cast = Some(cast);
        }
        // `old_field.clone(**field_data_dict)`: the recomputed types the UI sends replace the
        // stored ones (`DatasetValidator` writes them back after every validating pass).
        if let Some(data_type) = self.data_type {
            field.data_type = Some(data_type);
        }
        if let Some(initial_data_type) = self.initial_data_type {
            field.initial_data_type = Some(initial_data_type);
        }
        if let Some(field_type) = self.field_type {
            field.field_type = Some(field_type);
        }
        if let Some(avatar_id) = &self.avatar_id {
            field.avatar_id = Some(avatar_id.clone());
        }
        if let Some(ui_settings) = &self.ui_settings {
            field.ui_settings = ui_settings.clone();
        }
        if let Some(default_value) = &self.default_value {
            field.default_value = Some(default_value.clone());
        }
        if let Some(formula) = &self.formula {
            field.formula = formula.clone();
        }
        if let Some(guid_formula) = &self.guid_formula {
            field.guid_formula = guid_formula.clone();
        }
        // `apply_field_action`: a formula replaces the guid formula and vice versa, so the two
        // can never disagree about what the field computes.
        if self.formula.is_some() && self.guid_formula.is_none() {
            field.guid_formula.clear();
        }
        if self.guid_formula.is_some() && self.formula.is_none() {
            field.formula.clear();
        }
    }
}

/// `BIField.rename_in_formula` (`FIELD_RE = \[([^]]*)\]`): every `[key]` the map knows is
/// replaced by its value; an unknown key is left alone, and Python only logs a warning.
fn rename_in_formula(formula: &str, key_map: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(formula.len());
    let mut rest = formula;
    while let Some(open) = rest.find('[') {
        let (before, after) = rest.split_at(open);
        out.push_str(before);
        let Some(close) = after.find(']') else {
            out.push_str(after);
            return out;
        };
        match key_map(&after[1..close]) {
            Some(value) => {
                out.push('[');
                out.push_str(&value);
                out.push(']');
            }
            None => out.push_str(&after[..=close]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// `DatasetValidator._automanage_calc_mode_attrs`, formula branch: the two spellings of a
/// formula are kept in sync, and `formula` (titles) wins over `guid_formula` (guids). The UI
/// can send a stale `guid_formula` — the demo's markup field does, its `formula` has
/// `search$45q` while its `guid_formula` has `search$46q` — and Python overwrites it from the
/// formula, so the translation must not trust the request.
///
/// Python runs this before the field is inserted into the result schema, so the map never
/// contains the field itself; `dataset` is therefore passed as it is at that moment.
fn automanage_formula(field: &mut Field, dataset: &Dataset) {
    if field.calc_mode != CalcMode::Formula {
        return;
    }
    if !field.formula.is_empty() {
        let formula = field.formula.clone();
        field.guid_formula = rename_in_formula(&formula, |key| {
            dataset.field_by_title(key).map(|field| field.guid.clone())
        });
    } else if !field.guid_formula.is_empty() {
        let guid_formula = field.guid_formula.clone();
        field.formula = rename_in_formula(&guid_formula, |key| {
            dataset.field_by_guid(key).map(|field| field.title.clone())
        });
    }
}

/// Apply the `updates` of a request to a copy of the dataset.
pub fn apply_updates(dataset: &Dataset, updates: &[UpdateAction]) -> Result<Dataset, ApiError> {
    let mut result = dataset.clone();
    // `DatasetValidator._sort_action_batch`: parameters are added first, so a formula may
    // already reference them.
    let mut ordered: Vec<&UpdateAction> = updates.iter().collect();
    ordered.sort_by_key(|update| order_index(update));
    for update in ordered {
        apply_one(&mut result, update)?;
    }
    Ok(result)
}

fn order_index(update: &UpdateAction) -> u16 {
    match update {
        UpdateAction::AddField { field, .. } if field.calc_mode == Some(CalcMode::Parameter) => 0,
        _ => 100,
    }
}

fn apply_one(dataset: &mut Dataset, update: &UpdateAction) -> Result<(), ApiError> {
    match update {
        UpdateAction::AddField { field, order_index } => add_field(dataset, field, *order_index),
        UpdateAction::UpdateField { field } => update_field(dataset, field),
        UpdateAction::DeleteField { field } => delete_field(dataset, field),
        UpdateAction::Other => Err(ApiError::bad_request(
            "ERR.DS_API.ACTION_NOT_ALLOWED",
            "only add_field, update_field and delete_field are allowed by the data-api",
        )),
    }
}

fn field_not_found(guid: &str) -> ApiError {
    ApiError::bad_request(
        "ERR.DS_API.FIELD.NOT_FOUND",
        format!("field {guid} does not exist"),
    )
}

fn field_exists(guid: &str) -> ApiError {
    ApiError::bad_request(
        "ERR.DS_API.FIELD.INVALID",
        format!("field {guid} already exists"),
    )
}

fn title_conflict(title: &str) -> ApiError {
    ApiError::bad_request(
        "ERR.DS_API.FIELD.TITLE.CONFLICT",
        format!("a field titled {title} already exists"),
    )
}

/// `apply_field_action(action=add_field)`: a new field at `order_index` (first by default).
fn add_field(
    dataset: &mut Dataset,
    patch: &FieldPatch,
    order_index: Option<usize>,
) -> Result<(), ApiError> {
    let title = patch
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .ok_or_else(|| {
            ApiError::bad_request("ERR.DS_API.FIELD.TITLE", "a new field needs a title")
        })?;
    let guid = match patch.guid.clone().filter(|guid| !guid.trim().is_empty()) {
        Some(guid) => guid,
        None => make_field_id(dataset, &title),
    };
    if dataset.field_by_guid(&guid).is_some() {
        // Without `strict` a duplicate id is a silent no-op in Python.
        return if patch.strict {
            Err(field_exists(&guid))
        } else {
            Ok(())
        };
    }
    if dataset.field_by_title(&title).is_some() {
        return Err(title_conflict(&title));
    }
    let mut field = patch.to_field(&guid, &title);
    // `_get_autoupdated_field` runs before `result_schema.add`, so the new field is not part of
    // the title map it rewrites against.
    automanage_formula(&mut field, dataset);
    let position = order_index.unwrap_or(0).min(dataset.fields.len());
    dataset.fields.insert(position, field);
    Ok(())
}

/// `apply_field_action(action=update_field)`: patch the field, honouring `new_id`.
fn update_field(dataset: &mut Dataset, patch: &FieldPatch) -> Result<(), ApiError> {
    let guid = patch
        .guid
        .clone()
        .filter(|guid| !guid.trim().is_empty())
        .ok_or_else(|| field_not_found(""))?;
    let Some(position) = dataset.fields.iter().position(|field| field.guid == guid) else {
        return if patch.strict {
            Err(field_not_found(&guid))
        } else {
            Ok(())
        };
    };
    if let Some(title) = patch.title.as_deref() {
        if let Some(existing) = dataset.field_by_title(title) {
            if existing.guid != guid {
                return Err(title_conflict(title));
            }
        }
    }
    let mut field = dataset.fields[position].clone();
    patch.apply_to(&mut field);
    // `_get_autoupdated_field` runs before `result_schema.update_field`, so the map still holds
    // the field's previous title and guid — which is exactly what a self-reference expects.
    automanage_formula(&mut field, dataset);
    if let Some(new_id) = patch.new_id.clone().filter(|id| !id.trim().is_empty()) {
        if new_id != guid {
            if dataset.field_by_guid(&new_id).is_some() {
                return Err(field_exists(&new_id));
            }
            field.guid = new_id.clone();
            rename_guid_usages(dataset, &guid, &new_id, position);
        }
    }
    dataset.fields[position] = field;
    Ok(())
}

fn delete_field(dataset: &mut Dataset, deleted: &DeletedField) -> Result<(), ApiError> {
    match dataset
        .fields
        .iter()
        .position(|field| field.guid == deleted.guid)
    {
        Some(position) => {
            dataset.fields.remove(position);
            Ok(())
        }
        None if deleted.strict => Err(field_not_found(&deleted.guid)),
        None => Ok(()),
    }
}

/// `Dataset.rename_field_id_usages`: guid formulas of the other fields follow the rename.
fn rename_guid_usages(dataset: &mut Dataset, old_id: &str, new_id: &str, self_position: usize) {
    let old = format!("[{old_id}]");
    let new = format!("[{new_id}]");
    for (position, field) in dataset.fields.iter_mut().enumerate() {
        if position == self_position {
            continue;
        }
        if field.guid_formula.contains(&old) {
            field.guid_formula = field.guid_formula.replace(&old, &new);
        }
    }
}

/// `ReadableFieldIdGenerator.make_field_id`: a slug of the title, made unique.
fn make_field_id(dataset: &Dataset, title: &str) -> String {
    let base = readable_field_id(title);
    if dataset.field_by_guid(&base).is_none() {
        return base;
    }
    let mut index = 1;
    loop {
        let suffix = format!("_{index}");
        let room = ID_LENGTH - suffix.len();
        let candidate = format!("{}{suffix}", &base[..base.len().min(room)]);
        if dataset.field_by_guid(&candidate).is_none() {
            return candidate;
        }
        index += 1;
    }
}

/// `dl_core.components.ids.make_readable_field_id`.
///
/// `anyascii()` is approximated: Latin diacritics and Cyrillic are transliterated, any other
/// non-ASCII character is dropped.
fn readable_field_id(title: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in title.chars() {
        let lowered = ch.to_lowercase().next().unwrap_or(ch);
        if lowered.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        if lowered.is_ascii() {
            if lowered.is_ascii_alphanumeric() || matches!(lowered, '_' | '-') {
                current.push(lowered);
            }
            continue;
        }
        current.push_str(ascii_of(lowered));
    }
    if !current.is_empty() {
        words.push(current);
    }
    let id: String = words.join("_").chars().take(ID_LENGTH).collect();
    // Python would hand an empty id to `perform_component_id_validation`, which rejects it;
    // the port falls back to a usable id instead of failing the whole request.
    if id.is_empty() {
        "field".to_string()
    } else {
        id
    }
}

/// The ASCII spelling of a non-ASCII character, `""` when there is none.
fn ascii_of(ch: char) -> &'static str {
    match ch {
        'а' => "a",
        'б' => "b",
        'в' => "v",
        'г' => "g",
        'д' => "d",
        'е' | 'ё' | 'э' => "e",
        'ж' => "zh",
        'з' => "z",
        'и' | 'й' => "i",
        'к' => "k",
        'л' => "l",
        'м' => "m",
        'н' => "n",
        'о' => "o",
        'п' => "p",
        'р' => "r",
        'с' => "s",
        'т' => "t",
        'у' => "u",
        'ф' => "f",
        'х' => "kh",
        'ц' => "ts",
        'ч' => "ch",
        'ш' => "sh",
        'щ' => "shch",
        'ы' => "y",
        'ю' => "iu",
        'я' => "ia",
        'ь' | 'ъ' => "",
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ą' => "a",
        'ç' | 'ć' | 'č' => "c",
        'è' | 'é' | 'ê' | 'ë' | 'ě' | 'ę' => "e",
        'ì' | 'í' | 'î' | 'ï' | 'ı' => "i",
        'ł' => "l",
        'ñ' | 'ń' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ő' => "o",
        'ř' => "r",
        'ś' | 'š' | 'ş' => "s",
        'ù' | 'ú' | 'û' | 'ü' | 'ů' | 'ű' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'ğ' => "g",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::demo_dataset;

    fn add(patch: FieldPatch) -> UpdateAction {
        UpdateAction::AddField {
            field: patch,
            order_index: None,
        }
    }

    #[test]
    fn update_field_is_a_partial_patch() {
        let dataset = demo_dataset();
        let updated = apply_updates(
            &dataset,
            &[UpdateAction::UpdateField {
                field: FieldPatch {
                    guid: Some("f_sales".to_string()),
                    hidden: Some(true),
                    ..FieldPatch::default()
                },
            }],
        )
        .unwrap();
        let sales = updated.field_by_guid("f_sales").unwrap();
        assert!(sales.hidden);
        // Everything that was not in the request is untouched.
        assert_eq!(sales.aggregation, dl_core::Aggregation::Sum);
        assert_eq!(sales.title, "Sales");
        assert_eq!(updated.fields.len(), dataset.fields.len());
    }

    #[test]
    fn a_missing_field_only_fails_in_strict_mode() {
        let lenient = apply_updates(
            &demo_dataset(),
            &[UpdateAction::UpdateField {
                field: FieldPatch {
                    guid: Some("nope".to_string()),
                    ..FieldPatch::default()
                },
            }],
        );
        assert!(lenient.is_ok());

        let error = apply_updates(
            &demo_dataset(),
            &[UpdateAction::UpdateField {
                field: FieldPatch {
                    guid: Some("nope".to_string()),
                    strict: true,
                    ..FieldPatch::default()
                },
            }],
        )
        .unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "ERR.DS_API.FIELD.NOT_FOUND");
    }

    #[test]
    fn a_formula_rewrites_its_guid_formula_and_wins_over_the_request() {
        // `DatasetValidator._automanage_calc_mode_attrs`: the `guid_formula` in the request is
        // advisory, Python derives it from `formula`. The demo's markup field sends
        // `search$46q` in the guid form while its user-visible formula says `search$45q`, and
        // the reference response contains `$45`.
        let updated = apply_updates(
            &demo_dataset(),
            &[add(FieldPatch {
                guid: Some("f_link".to_string()),
                title: Some("Link".to_string()),
                formula: Some("URL('x$45q=' + [Sales], [Sales])".to_string()),
                guid_formula: Some("URL('x$46q=' + [f_sales], [f_sales])".to_string()),
                ..FieldPatch::default()
            })],
        )
        .unwrap();
        let link = updated.field_by_guid("f_link").unwrap();
        assert_eq!(link.guid_formula, "URL('x$45q=' + [f_sales], [f_sales])");
        assert_eq!(link.formula, "URL('x$45q=' + [Sales], [Sales])");
    }

    #[test]
    fn a_guid_formula_fills_in_the_title_formula() {
        let updated = apply_updates(
            &demo_dataset(),
            &[UpdateAction::UpdateField {
                field: FieldPatch {
                    guid: Some("f_calc".to_string()),
                    formula: Some(String::new()),
                    guid_formula: Some("[f_sales] + [f_year]".to_string()),
                    calc_mode: Some(CalcMode::Formula),
                    ..FieldPatch::default()
                },
            }],
        )
        .unwrap();
        let calc = updated.field_by_guid("f_calc").unwrap();
        assert_eq!(calc.formula, "[Sales] + [Year]");
        assert_eq!(calc.guid_formula, "[f_sales] + [f_year]");
    }

    #[test]
    fn rename_in_formula_only_touches_known_keys() {
        // `FIELD_RE = \[([^]]*)\]`, and an unknown key is left as it was written.
        assert_eq!(
            rename_in_formula("[a] + [b] + [c]", |key| match key {
                "a" => Some("1".to_string()),
                "c" => Some("3".to_string()),
                _ => None,
            }),
            "[1] + [b] + [3]"
        );
        // No brackets, an unknown key and an unterminated bracket all survive unchanged.
        assert_eq!(
            rename_in_formula("1 + 2", |_| Some("x".to_string())),
            "1 + 2"
        );
        assert_eq!(
            rename_in_formula("[]", |key| (key == "a").then(|| "x".to_string())),
            "[]"
        );
        assert_eq!(rename_in_formula("[a", |_| Some("x".to_string())), "[a");
    }

    #[test]
    fn new_id_renames_the_field_and_its_guid_formula_usages() {
        let updated = apply_updates(
            &demo_dataset(),
            &[UpdateAction::UpdateField {
                field: FieldPatch {
                    guid: Some("f_sales".to_string()),
                    new_id: Some("f_revenue".to_string()),
                    ..FieldPatch::default()
                },
            }],
        )
        .unwrap();
        assert!(updated.field_by_guid("f_sales").is_none());
        assert!(updated.field_by_guid("f_revenue").is_some());
        let calc = updated.field_by_guid("f_calc").unwrap();
        assert_eq!(calc.guid_formula, "[f_revenue] * 2");
    }

    #[test]
    fn add_field_takes_a_readable_id_from_the_title() {
        let updated = apply_updates(
            &demo_dataset(),
            &[add(FieldPatch {
                title: Some("René Проверка 2".to_string()),
                cast: Some(UserDataType::String),
                calc_mode: Some(CalcMode::Direct),
                ..FieldPatch::default()
            })],
        )
        .unwrap();
        let field = updated.field_by_guid("rene_proverka_2").unwrap();
        assert_eq!(field.title, "René Проверка 2");
        // New fields are placed first, like `order_index = 0` in Python.
        assert_eq!(updated.fields[0].guid, "rene_proverka_2");
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;
    use crate::testing::demo_dataset;
    use serde_json::json;

    fn add(patch: FieldPatch) -> UpdateAction {
        UpdateAction::AddField {
            field: patch,
            order_index: None,
        }
    }

    #[test]
    fn an_id_collision_gets_a_numeric_suffix() {
        let updated = apply_updates(
            &demo_dataset(),
            &[
                add(FieldPatch {
                    title: Some("Value!".to_string()),
                    ..FieldPatch::default()
                }),
                add(FieldPatch {
                    title: Some("value".to_string()),
                    ..FieldPatch::default()
                }),
            ],
        )
        .unwrap();
        assert!(updated.field_by_guid("value").is_some());
        assert!(updated.field_by_guid("value_1").is_some());
    }

    #[test]
    fn a_title_without_letters_still_yields_a_usable_id() {
        let updated = apply_updates(
            &demo_dataset(),
            &[add(FieldPatch {
                title: Some("!!!".to_string()),
                ..FieldPatch::default()
            })],
        )
        .unwrap();
        assert!(updated.field_by_guid("field").is_some());
    }

    #[test]
    fn a_duplicate_title_is_a_conflict() {
        let error = apply_updates(
            &demo_dataset(),
            &[add(FieldPatch {
                title: Some("Sales".to_string()),
                ..FieldPatch::default()
            })],
        )
        .unwrap_err();
        assert_eq!(error.code, "ERR.DS_API.FIELD.TITLE.CONFLICT");
    }

    #[test]
    fn delete_field_removes_it() {
        let updated = apply_updates(
            &demo_dataset(),
            &[UpdateAction::DeleteField {
                field: DeletedField {
                    guid: "f_year".to_string(),
                    strict: true,
                },
            }],
        )
        .unwrap();
        assert!(updated.field_by_guid("f_year").is_none());
    }

    #[test]
    fn actions_outside_the_data_api_whitelist_are_refused() {
        for action in [
            json!({"action": "clone_field", "field": {"guid": "f_new", "from_guid": "f_sales"}}),
            json!({"action": "update_setting", "setting": {"name": "template_enabled", "value": true}}),
            json!({"action": "replace_connection", "connection": {"id": "a", "new_id": "b"}}),
        ] {
            let parsed: UpdateAction = serde_json::from_value(action).unwrap();
            assert_eq!(parsed, UpdateAction::Other);
            let error = apply_updates(&demo_dataset(), &[parsed]).unwrap_err();
            assert_eq!(error.status, 400);
            assert_eq!(error.code, "ERR.DS_API.ACTION_NOT_ALLOWED");
        }
    }

    #[test]
    fn legacy_action_names_are_accepted() {
        let parsed: UpdateAction = serde_json::from_value(json!({
            "action": "add",
            "field": {"title": "Legacy", "guid": "f_legacy", "cast": "string"},
        }))
        .unwrap();
        assert!(matches!(parsed, UpdateAction::AddField { .. }));
        let updated = apply_updates(&demo_dataset(), &[parsed]).unwrap();
        assert!(updated.field_by_guid("f_legacy").is_some());
    }

    #[test]
    fn a_typed_default_value_of_a_parameter_is_unwrapped() {
        let parsed: UpdateAction = serde_json::from_value(json!({
            "action": "add_field",
            "field": {
                "title": "Scale",
                "guid": "f_scale",
                "calc_mode": "parameter",
                "cast": "float",
                "default_value": {"type": "float", "value": 0.5},
            },
        }))
        .unwrap();
        let updated = apply_updates(&demo_dataset(), &[parsed]).unwrap();
        let field = updated.field_by_guid("f_scale").unwrap();
        assert!(field.is_parameter());
        assert_eq!(field.default_value, Some(json!(0.5)));
    }
}
