//! The legend: `dl_api_lib.query.formalization.legend_formalizer`.
//!
//! A request is not one query: it is a *legend* of items (`fields` in the response), each with
//! a role, and only some of them reach the `SELECT` list. The legend is built **before** the
//! SQL, because it decides the `legend_item_id`s the rows point at:
//!
//! 1. `LegendFormalizer.make_legend` walks the item specs in `RawQuerySpecUnion.iter_item_specs`
//!    order — `select`, `order_by`, `filters`, `parameter_values` — and takes an id for each
//!    from one `IdGenerator` (explicit `legend_item_id`s are reserved up front, duplicates are
//!    an error);
//! 2. `patch_legend` appends an `info` item for every dataset field the request did not mention
//!    when `autofill_legend` is set.
//!
//! Everything that is not `selectable` ([`Role::is_selectable`]) stays in the legend and out of
//! the rows, which is why a v2 row is a `{data, legend}` pair.

use std::collections::{HashMap, HashSet};

use dl_core::{Aggregation, Dataset, Field, FieldType, UserDataType};
use dl_query::{FilterOperation, OutputColumn, Role};

use crate::dto::{
    FieldsItem, FilterItem, LegendItem, OrderByItem, ParameterValueItem, PivotRequest, RangeType,
    ResultRequest, RoleSpec, Visibility,
};
use crate::error::ApiError;

/// `dl_api_lib.query.formalization.id_gen.IdGenerator`: `0, 1, 2, ...` minus the pinned ids.
#[derive(Debug, Clone, Default)]
pub struct IdGenerator {
    next: i64,
    used: HashSet<i64>,
}

impl IdGenerator {
    pub fn new(used: impl IntoIterator<Item = i64>) -> Self {
        Self {
            next: 0,
            used: used.into_iter().collect(),
        }
    }

    /// The next id that is not pinned by the request.
    pub fn generate_id(&mut self) -> i64 {
        loop {
            let id = self.next;
            self.next += 1;
            if self.used.insert(id) {
                return id;
            }
        }
    }
}

/// `dl_constants.internal_constants`: the pseudo-dimensions every pivot legend needs.
///
/// They are *not* dataset fields: `PivotLegendFormalizer.patch_legend` adds them to the
/// legend as `info` items, so they hold a `legend_item_id` but never a value in a row.
pub const MEASURE_NAME_ID: &str = "__@measure_name__";
pub const MEASURE_NAME_TITLE: &str = "Measure Names";
pub const DIMENSION_NAME_ID: &str = "__@dimension_name__";
pub const DIMENSION_NAME_TITLE: &str = "Dimension Names";
/// The pseudo-field a `template` item points at: its value is the template string, not a field.
pub const PLACEHOLDER_ID: &str = "__@placeholder__";
pub const PLACEHOLDER_TITLE: &str = "Placeholder";

/// `dl_constants.enums.LegendItemType`, the values of [`LegendItem::item_type`].
pub const ITEM_TYPE_FIELD: &str = "field";
pub const ITEM_TYPE_MEASURE_NAME: &str = "measure_name";
pub const ITEM_TYPE_DIMENSION_NAME: &str = "dimension_name";
pub const ITEM_TYPE_PLACEHOLDER: &str = "placeholder";

impl LegendItem {
    /// `spec_is_field_name_pseudo_dimension`: `Measure Names` and `Dimension Names` stand for
    /// no dataset field, only for the pseudo-dimensions the pivot adds to the legend.
    pub fn is_pseudo_dimension(&self) -> bool {
        self.item_type == ITEM_TYPE_MEASURE_NAME || self.item_type == ITEM_TYPE_DIMENSION_NAME
    }
}

/// One legend entry plus what the query and the row decoder need from it.
#[derive(Debug, Clone, PartialEq)]
pub struct LegendEntry {
    pub item: LegendItem,
    /// `false` for the roles that are in the legend only.
    pub in_result_set: bool,
    /// Position of the entry in the request `fields` list, for the selected items.
    pub select_index: Option<usize>,
    /// `LegendItem.block_id`: `None` means the item belongs to *every* block
    /// (`Legend.limit_to_block` keeps it), which is how the measures and the request's own
    /// filters reach the second block of a tree drill-down.
    pub block_id: Option<i64>,
}

/// The legend of one request, in `dl_api_lib` order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Legend {
    pub entries: Vec<LegendEntry>,
}

impl Legend {
    /// The response `fields`.
    pub fn items(&self) -> Vec<LegendItem> {
        self.entries.iter().map(|e| e.item.clone()).collect()
    }

    /// `legend_item_id` of every selected item, in `SELECT` order.
    ///
    /// Items that are legend-only keep their `select_index` (their position in the request
    /// `fields` list) but have no column, so they are skipped here.
    pub fn select_ids(&self) -> Vec<i64> {
        let mut selected: Vec<(usize, i64)> = self
            .entries
            .iter()
            .filter(|entry| entry.in_result_set)
            .filter_map(|entry| {
                entry
                    .select_index
                    .map(|index| (index, entry.item.legend_item_id))
            })
            .collect();
        selected.sort_by_key(|(index, _)| *index);
        selected.into_iter().map(|(_, id)| id).collect()
    }

    /// `Legend.get_item`: the item a row's `legend` id points at.
    pub fn get_item(&self, legend_item_id: i64) -> Option<&LegendItem> {
        self.entries
            .iter()
            .map(|entry| &entry.item)
            .find(|item| item.legend_item_id == legend_item_id)
    }

    /// `Legend.list_for_role`.
    pub fn list_for_role(&self, role: Role) -> impl Iterator<Item = &LegendItem> {
        self.entries
            .iter()
            .map(|entry| &entry.item)
            .filter(move |item| item.role_spec.role == role)
    }

    /// `Legend.get_measure_name_legend_item_ids`.
    pub fn measure_name_ids(&self) -> Vec<i64> {
        self.items_of_type(ITEM_TYPE_MEASURE_NAME)
    }

    /// `Legend.get_dimension_name_legend_item_ids`.
    pub fn dimension_name_ids(&self) -> Vec<i64> {
        self.items_of_type(ITEM_TYPE_DIMENSION_NAME)
    }

    fn items_of_type(&self, item_type: &str) -> Vec<i64> {
        self.entries
            .iter()
            .map(|entry| &entry.item)
            .filter(|item| item.item_type == item_type)
            .map(|item| item.legend_item_id)
            .collect()
    }

    /// `populate_headers_info`: the ids of the `template` items, i.e. of the totals labels.
    pub fn total_ids(&self) -> Vec<i64> {
        self.list_for_role(Role::Template)
            .map(|item| item.legend_item_id)
            .collect()
    }

    /// The ids already taken, i.e. the `used_ids` a patched legend continues from
    /// (`PivotTotalsNormalizerHelper.legend_item_id_gen`).
    pub fn used_ids(&self) -> HashSet<i64> {
        self.entries
            .iter()
            .map(|entry| entry.item.legend_item_id)
            .collect()
    }
}
/// `ResultLegendFormalizer.SUPPORTS_ROLES`: the roles `/result` accepts at all.
///
/// `range` and `distinct` belong to the `/values/*` endpoints, so they are rejected here the
/// way `UnsopportedRoleInLegendError` rejects them in `dl_api_lib`.
pub const RESULT_ROLES: &[Role] = &[
    Role::Row,
    Role::Measure,
    Role::Info,
    Role::OrderBy,
    Role::Filter,
    Role::Parameter,
    Role::Total,
    Role::Template,
    Role::Tree,
];

/// `PivotLegendFormalizer.SUPPORTS_ROLES`: the roles `/pivot` accepts.
///
/// Wider than [`RESULT_ROLES`]: a pivot can hold `template` items (the totals labels of the
/// extra blocks) and no `tree`/`range`/`distinct`.
pub const PIVOT_ROLES: &[Role] = &[
    Role::Row,
    Role::Measure,
    Role::Info,
    Role::OrderBy,
    Role::Filter,
    Role::Parameter,
    Role::Total,
    Role::Template,
];

/// `LegendFormalizer`: dataset + request items -> [`Legend`].
pub struct LegendFormalizer<'a> {
    dataset: &'a Dataset,
    fields: &'a [FieldsItem],
    order_by: &'a [OrderByItem],
    filters: &'a [FilterItem],
    parameter_values: &'a [ParameterValueItem],
    autofill: bool,
    ignore_nonexistent_filters: bool,
    supported: &'static [Role],
    /// `PivotLegendFormalizer.patch_legend`: append `Measure Names`/`Dimension Names`.
    pivot: bool,
    /// `FormulaCompiler.get_field_type` for the dataset (`dl_query::field_types`). Empty means
    /// "nobody computed them", and then the stored `type` decides, as before.
    field_types: HashMap<String, FieldType>,
    /// `FormulaCompiler.get_field_final_data_type` for the dataset
    /// (`dl_query::field_data_types`). Empty means "nobody computed them", and then the stored
    /// `data_type` decides, as before.
    data_types: HashMap<String, UserDataType>,
}

impl<'a> LegendFormalizer<'a> {
    /// The `/result` formalizer (`ResultLegendFormalizer`).
    pub fn for_result(dataset: &'a Dataset, request: &'a ResultRequest) -> Self {
        Self {
            dataset,
            fields: &request.fields,
            order_by: &request.order_by,
            filters: &request.filters,
            parameter_values: &request.parameter_values,
            autofill: request.autofill_legend,
            ignore_nonexistent_filters: request.ignore_nonexistent_filters,
            supported: RESULT_ROLES,
            pivot: false,
            field_types: HashMap::new(),
            data_types: HashMap::new(),
        }
    }

    /// The `/pivot` formalizer (`PivotLegendFormalizer`).
    pub fn for_pivot(dataset: &'a Dataset, request: &'a PivotRequest) -> Self {
        Self {
            dataset,
            fields: &request.fields,
            order_by: &request.order_by,
            filters: &request.filters,
            parameter_values: &request.parameter_values,
            autofill: request.autofill_legend,
            ignore_nonexistent_filters: request.ignore_nonexistent_filters,
            supported: PIVOT_ROLES,
            pivot: true,
            field_types: HashMap::new(),
            data_types: HashMap::new(),
        }
    }

    /// Fill in `FormulaCompiler.get_field_type` (`dl_query::field_types`) for this dataset.
    ///
    /// Without it a legend item reports the `type` the dataset was saved with, which is the same
    /// value once the dataset has been through the validator, but is `DIMENSION` for a field that
    /// only exists in this request's `updates`.
    pub fn with_field_types(mut self, field_types: HashMap<String, FieldType>) -> Self {
        self.field_types = field_types;
        self
    }

    /// Fill in `FormulaCompiler.get_field_final_data_type` (`dl_query::field_data_types`).
    ///
    /// Python stores the recomputed type back into the field, so the legend reports the type
    /// of the *prepared* expression: a field whose compilation failed reports the stage
    /// manager's `string` fallback, not the type the request or the dataset claims.
    pub fn with_data_types(mut self, data_types: HashMap<String, UserDataType>) -> Self {
        self.data_types = data_types;
        self
    }

    pub fn make(&self) -> Result<Legend, ApiError> {
        self.check_blocks()?;
        let mut id_gen = IdGenerator::new(self.explicit_ids()?);
        let mut entries: Vec<LegendEntry> = Vec::new();
        // `found_field_ids`: a field already in the legend is not autofilled.
        let mut found: HashSet<String> = HashSet::new();

        // `iter_item_specs`: select, then order_by, then filters, then parameter values.
        for (index, item) in self.fields.iter().enumerate() {
            // `generate_legend_items` resolves the field first and validates the role after,
            // so a request with both problems reports the missing field. A placeholder is no
            // field at all (`PlaceholderObjSpec`), so only a real reference can be missing.
            // `dl_api_lib.query.formalization.legend_formalizer`: a pseudo reference names no
            // field, so it is not resolved through the dataset; everything else is.
            let pseudo = item.field_ref.pseudo();
            let field = match pseudo {
                Some(_) => None,
                None => Some(self.field(&item.field_ref)?),
            };
            let role = item.role();
            self.check_role(role)?;
            let legend_item_id = match item.legend_item_id {
                Some(legend_item_id) => legend_item_id,
                None => id_gen.generate_id(),
            };
            let legend_item = match (field, pseudo) {
                (Some(field), _) => {
                    self.check_tree_type(role, field)?;
                    found.insert(field.guid.clone());
                    self.legend_item(field, response_role_spec(item)?, legend_item_id)?
                }
                // `Measure Names` / `Dimension Names`: `MeasureNameObjSpec` and
                // `DimensionNameObjSpec`, the pseudo-dimensions of a pivot table.
                (None, Some(dl_core::PseudoField::MeasureName)) => pseudo_dimension_item(
                    legend_item_id,
                    MEASURE_NAME_ID,
                    MEASURE_NAME_TITLE,
                    ITEM_TYPE_MEASURE_NAME,
                    response_role_spec(item)?,
                ),
                (None, Some(dl_core::PseudoField::DimensionName)) => pseudo_dimension_item(
                    legend_item_id,
                    DIMENSION_NAME_ID,
                    DIMENSION_NAME_TITLE,
                    ITEM_TYPE_DIMENSION_NAME,
                    response_role_spec(item)?,
                ),
                // The pseudo-field of a totals block: its value is the template string.
                (None, None | Some(dl_core::PseudoField::Placeholder)) => {
                    placeholder_item(legend_item_id, response_role_spec(item)?)
                }
            };
            // A pseudo-dimension is legend-only: `Legend.list_selectable_items` keeps
            // `FieldObjSpec`s, so `Measure Names` never reaches the `SELECT` list even when its
            // requested role (`row`) would be selectable.
            let in_result_set = role.is_selectable()
                && !matches!(
                    pseudo,
                    Some(dl_core::PseudoField::MeasureName | dl_core::PseudoField::DimensionName)
                );
            entries.push(LegendEntry {
                item: legend_item,
                in_result_set,
                select_index: Some(index),
                block_id: item.block_id,
            });
        }

        for item in self.order_by {
            let field = self.field(&item.field_ref)?;
            let legend_item_id = id_gen.generate_id();
            found.insert(field.guid.clone());
            let role_spec = RoleSpec {
                role: Role::OrderBy,
                direction: Some(item.direction),
                ..RoleSpec::default()
            };
            entries.push(LegendEntry {
                item: self.legend_item(field, role_spec, legend_item_id)?,
                in_result_set: Role::OrderBy.is_selectable(),
                select_index: None,
                block_id: item.block_id,
            });
        }

        for item in self.filters {
            let Some(field) = self.dataset.resolve(&item.field_ref) else {
                if self.ignore_nonexistent_filters {
                    // The id is taken before the item is resolved, so it is burnt either way.
                    id_gen.generate_id();
                    continue;
                }
                return Err(unknown(&item.field_ref));
            };
            let legend_item_id = id_gen.generate_id();
            found.insert(field.guid.clone());
            entries.push(LegendEntry {
                item: self.legend_item(field, RoleSpec::new(Role::Filter), legend_item_id)?,
                in_result_set: Role::Filter.is_selectable(),
                select_index: None,
                block_id: item.block_id,
            });
        }

        for item in self.parameter_values {
            let field = self.field(&item.field_ref)?;
            let legend_item_id = id_gen.generate_id();
            found.insert(field.guid.clone());
            entries.push(LegendEntry {
                item: self.legend_item(field, RoleSpec::new(Role::Parameter), legend_item_id)?,
                in_result_set: Role::Parameter.is_selectable(),
                select_index: None,
                block_id: item.block_id,
            });
        }

        // `patch_legend`: `_generate_tree_filter_items` runs before the autofill, so the filter
        // items of a tree sit between the request's items and the `info` fields.
        self.patch_tree_items(&mut entries, &mut id_gen);

        if self.autofill {
            self.autofill(&mut entries, &found, &mut id_gen)?;
        }

        if self.pivot {
            self.patch_pivot_items(&mut entries, &mut id_gen);
        }

        Ok(Legend { entries })
    }

    /// `_generate_tree_filter_items`: a `tree` item drags the filters of its level into the
    /// legend, all of them with the tree's own `block_id`, so the level query runs them:
    ///
    /// * `LENGTE(level)` — the level only shows items that deep in the tree (the compiler builds
    ///   this one from the item's `level`, the legend item is what the client echoes back);
    /// * `STARTSWITH(prefix)` when the level is nested inside an expanded branch;
    /// * `EQ(value)` for every dimension the level is locked by (`dimension_values`) — except
    ///   the tree itself: a level locked by its own parent is already covered by `prefix`.
    fn patch_tree_items(&self, entries: &mut Vec<LegendEntry>, id_gen: &mut IdGenerator) {
        let trees: Vec<LegendEntry> = entries
            .iter()
            .filter(|entry| entry.item.role_spec.role == Role::Tree)
            .cloned()
            .collect();
        for tree in trees {
            let mut filters: Vec<(FilterOperation, Vec<serde_json::Value>)> = Vec::new();
            if let Some(level) = tree.item.role_spec.level {
                filters.push((FilterOperation::Lengte, vec![serde_json::json!(level)]));
            }
            let prefix = tree.item.role_spec.tree_prefix();
            if !prefix.is_empty() {
                filters.push((
                    FilterOperation::Startswith,
                    vec![serde_json::Value::Array(prefix)],
                ));
            }
            for (operation, values) in filters {
                entries.push(LegendEntry {
                    item: LegendItem {
                        legend_item_id: id_gen.generate_id(),
                        role_spec: RoleSpec {
                            operation: Some(operation),
                            values: Some(values),
                            ..RoleSpec::new(Role::Filter)
                        },
                        ..tree.item.clone()
                    },
                    in_result_set: false,
                    select_index: None,
                    block_id: tree.block_id,
                });
            }
            for dimension in tree.item.role_spec.tree_dimension_values() {
                let Some(locked) = entries
                    .iter()
                    .find(|entry| entry.item.legend_item_id == dimension.legend_item_id)
                    .map(|entry| entry.item.clone())
                else {
                    // `Legend.get_item` raises `LegendItemReferenceError`; a request that sends
                    // its own `dimension_values` refers to an item of this very legend, so a
                    // missing one can only come from a legend without that item. Skip it: the
                    // level is then wider than the client asked for, not wrong.
                    continue;
                };
                if locked.id == tree.item.id {
                    continue;
                }
                entries.push(LegendEntry {
                    item: LegendItem {
                        legend_item_id: id_gen.generate_id(),
                        role_spec: RoleSpec {
                            operation: Some(FilterOperation::Eq),
                            values: Some(vec![dimension.value]),
                            ..RoleSpec::new(Role::Filter)
                        },
                        ..locked
                    },
                    in_result_set: false,
                    select_index: None,
                    block_id: tree.block_id,
                });
            }
        }
    }

    /// `PivotLegendFormalizer.patch_legend`: a pivot always needs both pseudo-dimensions.
    ///
    /// `PivotFormalizerBase.patch_pivot_legend` asserts their presence (and reads
    /// `Measure Names` even when the request never mentioned it), and
    /// `TableDataFacade.iter_row_dim_headers` takes the first `dimension_name` item
    /// unconditionally, so both are added with role `info` — not `row`, which would put them
    /// in the `SELECT` list and in the rows.
    fn patch_pivot_items(&self, entries: &mut Vec<LegendEntry>, id_gen: &mut IdGenerator) {
        let has = |entries: &Vec<LegendEntry>, item_type: &str| {
            entries
                .iter()
                .any(|entry| entry.item.item_type == item_type)
        };
        if !has(entries, ITEM_TYPE_MEASURE_NAME) {
            entries.push(LegendEntry {
                item: pseudo_item(
                    id_gen.generate_id(),
                    MEASURE_NAME_ID,
                    MEASURE_NAME_TITLE,
                    ITEM_TYPE_MEASURE_NAME,
                ),
                in_result_set: Role::Info.is_selectable(),
                select_index: None,
                block_id: None,
            });
        }
        if !has(entries, ITEM_TYPE_DIMENSION_NAME) {
            entries.push(LegendEntry {
                item: pseudo_item(
                    id_gen.generate_id(),
                    DIMENSION_NAME_ID,
                    DIMENSION_NAME_TITLE,
                    ITEM_TYPE_DIMENSION_NAME,
                ),
                in_result_set: Role::Info.is_selectable(),
                select_index: None,
                block_id: None,
            });
        }
    }

    /// `_generate_info_items`: everything the request did not mention, as `info`, in
    /// `result_schema` order.
    fn autofill(
        &self,
        entries: &mut Vec<LegendEntry>,
        found: &HashSet<String>,
        id_gen: &mut IdGenerator,
    ) -> Result<(), ApiError> {
        for field in self.dataset.fields.iter() {
            if found.contains(&field.guid) {
                continue;
            }
            let legend_item_id = id_gen.generate_id();
            entries.push(LegendEntry {
                item: self.legend_item(field, RoleSpec::new(Role::Info), legend_item_id)?,
                in_result_set: Role::Info.is_selectable(),
                select_index: None,
                block_id: None,
            });
        }
        Ok(())
    }

    /// One legend item of a field item (`FieldObjSpec` + role spec).
    fn legend_item(
        &self,
        field: &Field,
        role_spec: RoleSpec,
        legend_item_id: i64,
    ) -> Result<LegendItem, ApiError> {
        // `DatasetValidator` stores the recomputed type back into the field, so the legend
        // reports it; the declared type is only the fallback for a builder without the map.
        let data_type = match self.data_types.get(&field.guid) {
            Some(data_type) => *data_type,
            None => UserDataType::from_data_type(field.dtype()?),
        };
        Ok(LegendItem {
            legend_item_id,
            id: field.guid.clone(),
            title: field.title.clone(),
            data_type,
            field_type: self.field_type(field),
            item_type: ITEM_TYPE_FIELD.to_string(),
            role_spec,
        })
    }

    /// `FormulaCompiler.get_field_type`: the computed type, else the stored one.
    fn field_type(&self, field: &Field) -> FieldType {
        self.field_types
            .get(&field.guid)
            .copied()
            .unwrap_or_else(|| self.dataset.field_type(field))
    }

    fn field(&self, field_ref: &dl_core::FieldRef) -> Result<&Field, ApiError> {
        self.dataset
            .resolve(field_ref)
            .ok_or_else(|| unknown(field_ref))
    }

    /// `_validate_explicit_legend_item_ids`: duplicates are `NonUniqueLegendIdsError`.
    fn explicit_ids(&self) -> Result<HashSet<i64>, ApiError> {
        let ids: Vec<i64> = self
            .fields
            .iter()
            .filter_map(|item| item.legend_item_id)
            .collect();
        let unique: HashSet<i64> = ids.iter().copied().collect();
        if unique.len() != ids.len() {
            return Err(ApiError::bad_request(
                "ERR.DS_API.LEGEND.NON_UNIQUE_IDS",
                "Got non-unique legend item IDs",
            ));
        }
        Ok(unique)
    }

    /// `UnsopportedRoleInLegendError`.
    ///
    /// `total` is a normal selectable role here: `ResultLegendFormalizer.SUPPORTS_ROLES` lists
    /// it and `SELECTABLE_ROLES` keeps it, so the item is in the `SELECT` list like a measure.
    /// The role matters to the totals block, which is a second query (see `prepare_totals`).
    fn check_role(&self, role: Role) -> Result<(), ApiError> {
        if !self.supported.contains(&role) {
            return Err(ApiError::bad_request(
                "ERR.DS_API.LEGEND.UNSUPPORTED_ROLE",
                format!("Legend role {} is not supported", role.as_str()),
            ));
        }
        Ok(())
    }

    /// `validate_legend_item`: `DATA_TYPES_SUPPORTING_TREE = {tree_str}`, so a `tree` item over
    /// any other type is a `RoleDataTypeMismatchError`.
    fn check_tree_type(&self, role: Role, field: &Field) -> Result<(), ApiError> {
        if role != Role::Tree {
            return Ok(());
        }
        let data_type = UserDataType::from_data_type(field.dtype()?);
        if data_type != UserDataType::TreeStr {
            return Err(ApiError::bad_request(
                "ERR.DS_API.LEGEND.ROLE_DATA_TYPE_MISMATCH",
                "Unsupported data type for tree role",
            ));
        }
        Ok(())
    }

    /// `MultipleBlocksUnsupportedError`, narrowed to the endpoints Python rejects it on.
    ///
    /// `/result` runs every block of the request (`execute_all_queries`), so a tree drill-down —
    /// one block per level — is two queries merged into one stream. `/pivot` is the endpoint that
    /// does not: `PivotRequestModelNormalizer` raises as soon as the request names two blocks.
    fn check_blocks(&self) -> Result<(), ApiError> {
        if !self.pivot {
            return Ok(());
        }
        let mut block_ids: Vec<i64> = self
            .fields
            .iter()
            .filter_map(|item| item.block_id)
            .chain(self.order_by.iter().filter_map(|item| item.block_id))
            .chain(self.filters.iter().filter_map(|item| item.block_id))
            .chain(
                self.parameter_values
                    .iter()
                    .filter_map(|item| item.block_id),
            )
            .collect();
        block_ids.sort_unstable();
        block_ids.dedup();
        if block_ids.len() > 1 {
            return Err(ApiError::bad_request(
                "ERR.DS_API.BLOCK.MULTIPLE_UNSUPPORTED",
                "This API does not support multiple query blocks.",
            ));
        }
        Ok(())
    }
}

/// One pseudo-dimension legend item (`MeasureNameObjSpec` / `DimensionNameObjSpec`).
///
/// `LegendItem.id`/`title` are not taken from a field: `dl_query_processing.legend
/// .field_legend.LegendItem` returns the `dl_constants.internal_constants` values for these
/// item types, and the data type is always `string` on a `dimension`.
fn pseudo_item(legend_item_id: i64, id: &str, title: &str, item_type: &str) -> LegendItem {
    LegendItem {
        legend_item_id,
        id: id.to_string(),
        title: title.to_string(),
        data_type: UserDataType::String,
        field_type: FieldType::Dimension,
        item_type: item_type.to_string(),
        role_spec: RoleSpec::new(Role::Info),
    }
}

/// A pseudo-dimension the *request* named (`{"ref": {"type": "measure_name"}}`).
///
/// Unlike the patched items `patch_pivot_items` appends, this one keeps the role spec the client
/// sent (the response `fields` of Python show `{"role": "row", "visibility": "visible"}`).
fn pseudo_dimension_item(
    legend_item_id: i64,
    id: &str,
    title: &str,
    item_type: &str,
    role_spec: RoleSpec,
) -> LegendItem {
    LegendItem {
        role_spec,
        ..pseudo_item(legend_item_id, id, title, item_type)
    }
}

/// One placeholder legend item (`PlaceholderObjSpec`).
///
/// The value of the item is the `template` of its role spec — `''` for a totals block, where
/// the charts engine sends `{"role": "template", "template": ""}` — which is why the legend id
/// and not a field guid is what the row points at.
fn placeholder_item(legend_item_id: i64, role_spec: RoleSpec) -> LegendItem {
    LegendItem {
        legend_item_id,
        id: PLACEHOLDER_ID.to_string(),
        title: PLACEHOLDER_TITLE.to_string(),
        data_type: UserDataType::String,
        field_type: FieldType::Dimension,
        item_type: ITEM_TYPE_PLACEHOLDER.to_string(),
        role_spec,
    }
}

fn unknown(field_ref: &dl_core::FieldRef) -> ApiError {
    let name = match field_ref {
        dl_core::FieldRef::Id { id } => id.clone(),
        dl_core::FieldRef::Title { title } => title.clone(),
        dl_core::FieldRef::MeasureName => MEASURE_NAME_ID.to_string(),
        dl_core::FieldRef::DimensionName => DIMENSION_NAME_ID.to_string(),
        dl_core::FieldRef::Placeholder => PLACEHOLDER_ID.to_string(),
    };
    // `dl_core.exc.FieldNotFoundError`: the reference answers `ERR.DS_API.FIELD.NOT_FOUND`
    // and `Unknown field <id>` for a reference the dataset does not have.
    ApiError::bad_request(
        "ERR.DS_API.FIELD.NOT_FOUND",
        format!("Unknown field {name}"),
    )
}

/// The `role_spec` of the response: the request's, with the defaults `dl_api_lib` fills in.
fn response_role_spec(item: &FieldsItem) -> Result<RoleSpec, ApiError> {
    let role = item.role();
    let mut role_spec = item
        .role_spec
        .clone()
        .unwrap_or_else(|| RoleSpec::new(role));
    role_spec.role = role;
    match role {
        // `RawDimensionRoleSpec`: a dimension is `visible` unless the client says otherwise.
        Role::Row | Role::Template | Role::Tree => {
            if role_spec.visibility.is_none() {
                role_spec.visibility = Some(Visibility::Visible);
            }
            if role == Role::Template && role_spec.template.is_none() {
                role_spec.template = Some(String::new());
            }
            if role == Role::Tree {
                role_spec.prefix = Some(tree_prefix(role_spec.prefix)?);
                if role_spec.dimension_values.is_none() {
                    role_spec.dimension_values = Some(serde_json::Value::Array(Vec::new()));
                }
            }
        }
        _ => {}
    }
    Ok(role_spec)
}

/// `json.loads(raw_role_spec.prefix)`: a JSON *string* on the wire, a list afterwards.
///
/// Anything that does not parse into a list is `GenericInvalidRequestError` in
/// `LegendFormalizer._resolve_item_spec`.
fn tree_prefix(prefix: Option<serde_json::Value>) -> Result<serde_json::Value, ApiError> {
    let invalid = || {
        ApiError::bad_request(
            "ERR.DS_API.INVALID_REQUEST",
            "Invalid value for tree prefix",
        )
    };
    match prefix {
        None => Ok(serde_json::Value::Array(Vec::new())),
        Some(serde_json::Value::String(text)) => {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::Array(items)) => Ok(serde_json::Value::Array(items)),
                _ => Err(invalid()),
            }
        }
        Some(serde_json::Value::Array(items)) => Ok(serde_json::Value::Array(items)),
        Some(_) => Err(invalid()),
    }
}

/// The fields `preview` selects: visible, and not parameters (`PreviewLegendFormalizer`).
pub fn preview_fields(dataset: &Dataset) -> Vec<&Field> {
    dataset
        .fields
        .iter()
        .filter(|field| !field.hidden && !field.is_parameter())
        .collect()
}

/// `PreviewLegendFormalizer`: every visible field as an ordinary `row` item.
pub fn preview_legend(dataset: &Dataset) -> Legend {
    let mut id_gen = IdGenerator::new(Vec::new());
    let entries = preview_fields(dataset)
        .into_iter()
        .enumerate()
        .map(|(index, field)| LegendEntry {
            item: LegendItem {
                legend_item_id: id_gen.generate_id(),
                id: field.guid.clone(),
                title: field.title.clone(),
                data_type: field
                    .dtype()
                    .map(UserDataType::from_data_type)
                    .unwrap_or(UserDataType::String),
                field_type: dataset.field_type(field),
                item_type: "field".to_string(),
                role_spec: RoleSpec::row(),
            },
            in_result_set: true,
            select_index: Some(index),
            block_id: None,
        })
        .collect();
    Legend { entries }
}

/// The legend of a `/values/*` request: what the compiled columns say, nothing else.
/// `RangeLegendFormalizer`: both items keep the field's own id and title, and the `role_spec`
/// says which end of the range the item is.
fn column_role_spec(column: &OutputColumn) -> RoleSpec {
    let mut role_spec = RoleSpec::new(column.role);
    if column.role == Role::Range {
        role_spec.range_type = match column.aggregation {
            Aggregation::Min => Some(RangeType::Min),
            Aggregation::Max => Some(RangeType::Max),
            _ => None,
        };
    }
    role_spec
}

pub fn legend_of_columns(columns: &[OutputColumn]) -> Legend {
    let mut selected = 0;
    let entries = columns
        .iter()
        .map(|column| {
            let select_index = column.in_result_set.then(|| {
                let index = selected;
                selected += 1;
                index
            });
            LegendEntry {
                item: LegendItem {
                    legend_item_id: column.legend_item_id,
                    id: column.guid.clone(),
                    title: column.title.clone(),
                    data_type: dl_core::UserDataType::from_data_type(column.data_type),
                    field_type: match column.role {
                        Role::Measure => FieldType::Measure,
                        _ => FieldType::Dimension,
                    },
                    item_type: "field".to_string(),
                    role_spec: column_role_spec(column),
                },
                in_result_set: column.in_result_set,
                block_id: None,
                select_index,
            }
        })
        .collect();
    Legend { entries }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::demo_dataset;
    use dl_core::{CalcMode, Dataset, Field};
    use serde_json::json;

    fn parse(payload: serde_json::Value) -> ResultRequest {
        serde_json::from_value(payload).expect("the payload is a v2 request")
    }

    fn make(dataset: &Dataset, request: &ResultRequest) -> Result<Legend, ApiError> {
        LegendFormalizer::for_result(dataset, request).make()
    }

    fn dataset_with_parameter() -> Dataset {
        let mut dataset = demo_dataset();
        dataset.fields.push(Field {
            guid: "p_scale".to_string(),
            title: "Scale".to_string(),
            data_type: Some(UserDataType::Float),
            calc_mode: CalcMode::Parameter,
            default_value: Some(json!(0.5)),
            ..Field::default()
        });
        dataset
    }

    #[test]
    fn role_spec_is_where_the_api_puts_the_role() {
        let request = parse(json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}},
                {"ref": {"type": "id", "id": "f_year"}, "role": "info"},
                {"ref": {"type": "id", "id": "f_calc"}, "role": "dimension"},
            ],
        }));
        assert_eq!(request.fields[0].role(), Role::Row);
        assert_eq!(request.fields[1].role(), Role::Measure);
        // The flat `role` is this port's pre-M2b shape, still read.
        assert_eq!(request.fields[2].role(), Role::Info);
        // `dimension` is not a `FieldRole`, it is the alias of `row`.
        assert_eq!(request.fields[3].role(), Role::Row);

        let legend = make(&demo_dataset(), &request).unwrap();
        assert_eq!(legend.entries[0].item.role_spec, RoleSpec::row());
        assert_eq!(
            legend.entries[0].item.role_spec.visibility,
            Some(Visibility::Visible)
        );
        assert_eq!(
            legend.entries[1].item.role_spec,
            RoleSpec::new(Role::Measure)
        );
    }

    #[test]
    fn every_item_gets_a_legend_id_in_spec_order() {
        let request = parse(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}}],
            "order_by": [{"ref": {"type": "id", "id": "f_sales"}, "direction": "desc"}],
            "filters": [{"ref": {"type": "id", "id": "f_year"}, "operation": "gt", "values": [2000]}],
            "parameter_values": [{"ref": {"type": "id", "id": "p_scale"}, "value": 2}],
        }));
        let legend = make(&dataset_with_parameter(), &request).unwrap();
        let ids: Vec<(i64, &str, Role)> = legend
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.item.legend_item_id,
                    entry.item.id.as_str(),
                    entry.item.role_spec.role,
                )
            })
            .collect();
        assert_eq!(
            ids,
            vec![
                (0, "f_region", Role::Row),
                (1, "f_sales", Role::OrderBy),
                (2, "f_year", Role::Filter),
                (3, "p_scale", Role::Parameter),
            ]
        );
        // Only the row is in the `SELECT`; the rest are legend-only.
        assert_eq!(legend.select_ids(), vec![0]);
        assert_eq!(
            legend.entries[1].item.role_spec.direction,
            Some(dl_query::Direction::Desc)
        );
    }

    #[test]
    fn autofill_appends_the_fields_the_request_does_not_mention() {
        let mut request = parse(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}}],
        }));
        assert_eq!(make(&demo_dataset(), &request).unwrap().entries.len(), 1);

        request.autofill_legend = true;
        let legend = make(&demo_dataset(), &request).unwrap();
        let ids: Vec<i64> = legend
            .entries
            .iter()
            .map(|entry| entry.item.legend_item_id)
            .collect();
        assert_eq!(ids, vec![0, 1, 2, 3]);
        // Dataset order, and `info` for everything that is not in the request.
        let infos: Vec<&str> = legend.entries[1..]
            .iter()
            .map(|entry| entry.item.id.as_str())
            .collect();
        assert_eq!(infos, vec!["f_sales", "f_year", "f_calc"]);
        for entry in &legend.entries[1..] {
            assert_eq!(entry.item.role_spec.role, Role::Info);
            assert!(!entry.in_result_set);
            assert_eq!(entry.select_index, None);
        }
        // An info item is still typed: the client renders it in the field list.
        assert_eq!(legend.entries[1].item.field_type, FieldType::Measure);
    }

    #[test]
    fn autofill_does_not_repeat_a_field_that_is_only_filtered_on() {
        let request = parse(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}}],
            "filters": [{"ref": {"type": "id", "id": "f_year"}, "operation": "gt", "values": [2000]}],
            "autofill_legend": true,
        }));
        let legend = make(&demo_dataset(), &request).unwrap();
        let ids: Vec<&str> = legend
            .entries
            .iter()
            .map(|entry| entry.item.id.as_str())
            .collect();
        assert_eq!(ids, vec!["f_region", "f_year", "f_sales", "f_calc"]);
        assert_eq!(legend.entries[1].item.role_spec.role, Role::Filter);
    }

    #[test]
    fn explicit_legend_item_ids_are_kept_and_reserved() {
        let request = parse(json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}, "legend_item_id": 1},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}},
            ],
        }));
        let legend = make(&demo_dataset(), &request).unwrap();
        // The pinned id is kept; the generator hands out the first free id, which is 0.
        assert_eq!(legend.select_ids(), vec![1, 0]);
    }

    #[test]
    fn duplicate_legend_item_ids_are_rejected() {
        let request = parse(json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}, "legend_item_id": 7},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}, "legend_item_id": 7},
            ],
        }));
        let error = make(&demo_dataset(), &request).unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "ERR.DS_API.LEGEND.NON_UNIQUE_IDS");
    }

    #[test]
    fn a_role_the_endpoint_does_not_support_is_rejected() {
        // `range` belongs to `/values/range`, exactly as `ResultLegendFormalizer` says.
        let request = parse(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "range"}}],
        }));
        let error = make(&demo_dataset(), &request).unwrap_err();
        assert_eq!(error.code, "ERR.DS_API.LEGEND.UNSUPPORTED_ROLE");
    }

    #[test]
    fn a_totals_item_is_a_selectable_role_like_a_measure() {
        // `ResultLegendFormalizer.SUPPORTS_ROLES` and `SELECTABLE_ROLES` both contain `total`,
        // so the item is in the SELECT list; only the totals *block* treats it specially.
        let request = parse(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "total"}}],
        }));
        let legend = make(&demo_dataset(), &request).unwrap();
        assert!(legend.entries[0].in_result_set);
        assert_eq!(legend.entries[0].item.role_spec.role, Role::Total);
        assert_eq!(legend.select_ids(), vec![0]);
    }

    #[test]
    fn a_tree_item_keeps_its_level_and_gets_an_array_prefix() {
        // `DATA_TYPES_SUPPORTING_TREE = {tree_str}`; the request sends the prefix as a JSON
        // string and the response spells it as an array.
        let mut dataset = demo_dataset();
        let region = dataset
            .fields
            .iter_mut()
            .find(|field| field.guid == "f_region")
            .unwrap();
        region.initial_data_type = Some(UserDataType::TreeStr);
        region.data_type = Some(UserDataType::TreeStr);
        region.cast = Some(UserDataType::TreeStr);
        let request = parse(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"},
                        "role_spec": {"role": "tree", "level": 2, "prefix": "[\"a\"]"}}],
        }));
        let legend = make(&dataset, &request).unwrap();
        let role_spec = &legend.entries[0].item.role_spec;
        assert_eq!(role_spec.role, Role::Tree);
        assert_eq!(role_spec.level, Some(2));
        assert_eq!(role_spec.prefix, Some(json!(["a"])));
        assert_eq!(role_spec.dimension_values, Some(json!([])));
        assert_eq!(role_spec.visibility, Some(Visibility::Visible));
        assert!(legend.entries[0].in_result_set);
        // `_generate_tree_filter_items`: the level's filters are legend items of their own —
        // `LENGTE` for the level and `STARTSWITH` for the branch it is nested in.
        let length = &legend.entries[1];
        assert_eq!(length.item.role_spec.role, Role::Filter);
        assert_eq!(length.item.id, "f_region");
        assert_eq!(length.item.title, "Region");
        assert!(!length.in_result_set);
        assert_eq!(length.item.legend_item_id, 1);
        assert_eq!(
            length.item.role_spec.operation,
            Some(FilterOperation::Lengte)
        );
        assert_eq!(length.item.role_spec.values, Some(vec![json!(2)]));
        let prefix = &legend.entries[2];
        assert_eq!(prefix.item.legend_item_id, 2);
        assert_eq!(prefix.item.role_spec.role, Role::Filter);
        assert_eq!(
            prefix.item.role_spec.operation,
            Some(FilterOperation::Startswith)
        );
        assert_eq!(prefix.item.role_spec.values, Some(vec![json!(["a"])]));
    }

    #[test]
    fn a_tree_level_locked_by_another_dimension_gets_an_eq_filter() {
        // `dimension_values` of a tree item: every locked dimension the level does not inherit
        // through its own `prefix` becomes an `EQ` filter with the level's `block_id`. A level
        // locked by *itself* (the usual drill-down) adds nothing.
        let mut dataset = demo_dataset();
        for field in dataset.fields.iter_mut() {
            if field.guid == "f_region" || field.guid == "f_year" {
                field.initial_data_type = Some(UserDataType::TreeStr);
                field.data_type = Some(UserDataType::TreeStr);
                field.cast = Some(UserDataType::TreeStr);
            }
        }
        let request = parse(json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "legend_item_id": 1,
                 "role_spec": {"role": "tree", "level": 1, "prefix": "[]", "dimension_values": []},
                 "block_id": 0},
                {"ref": {"type": "id", "id": "f_year"}, "legend_item_id": 3,
                 "role_spec": {"role": "tree", "level": 1, "prefix": "[]",
                                "dimension_values": [{"legend_item_id": 1, "value": "[\"a\"]"}]},
                 "block_id": 1},
            ],
        }));
        let legend = make(&dataset, &request).unwrap();
        let ids: Vec<(i64, Option<FilterOperation>)> = legend
            .entries
            .iter()
            .map(|entry| (entry.item.legend_item_id, entry.item.role_spec.operation))
            .collect();
        // Two tree items (blocks 0 and 1), then their generated filters in legend order: the
        // `LENGTE` of each level and the `EQ` the second level is locked with.
        assert_eq!(
            ids,
            vec![
                (1, None),
                (3, None),
                (0, Some(FilterOperation::Lengte)),
                (2, Some(FilterOperation::Lengte)),
                (4, Some(FilterOperation::Eq)),
            ]
        );
        let eq = &legend.entries[4];
        assert_eq!(eq.item.id, "f_region");
        assert_eq!(eq.item.role_spec.values, Some(vec![json!("[\"a\"]")]));
        // The lock belongs to the level it locks, not to the root block.
        assert_eq!(eq.block_id, Some(1));
        assert_eq!(legend.entries[2].block_id, Some(0));
        assert_eq!(legend.entries[3].block_id, Some(1));
    }

    #[test]
    fn a_tree_item_over_a_string_field_is_a_role_data_type_mismatch() {
        let request = parse(json!({
            "fields": [{"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "tree"}}],
        }));
        let error = make(&demo_dataset(), &request).unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "ERR.DS_API.LEGEND.ROLE_DATA_TYPE_MISMATCH");
        assert_eq!(error.message, "Unsupported data type for tree role");
    }

    #[test]
    fn a_result_runs_every_block_and_a_pivot_only_one() {
        // `/result` runs every block the request names (a totals block, a tree level); a pivot is
        // one block, because `pivot.structure` addresses legend ids of a single legend.
        let fields = json!([
            {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}},
            {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}, "block_id": 1},
        ]);
        let request = parse(json!({ "fields": fields }));
        assert!(make(&demo_dataset(), &request).is_ok());

        // Two *different* block ids is what `PivotRequestModelNormalizer` refuses; a single
        // non-zero one is fine (Python counts the distinct ids, not the items).
        let pivot: PivotRequest = serde_json::from_value(json!({
            "fields": [
                {"ref": {"type": "id", "id": "f_region"}, "role_spec": {"role": "row"}, "block_id": 0},
                {"ref": {"type": "id", "id": "f_sales"}, "role_spec": {"role": "measure"}, "block_id": 1},
            ],
            "pivot": {"structure": []},
        }))
        .unwrap();
        let error = LegendFormalizer::for_pivot(&demo_dataset(), &pivot)
            .make()
            .unwrap_err();
        assert_eq!(error.code, "ERR.DS_API.BLOCK.MULTIPLE_UNSUPPORTED");
    }

    #[test]
    fn a_measure_name_ref_is_a_pseudo_legend_item_not_a_field() {
        // `{"ref": {"type": "measure_name"}}` is what the pivot table sends for `Measure Names`.
        // It used to be a 422 at deserialization (`unknown variant measure_name`).
        let request = parse(json!({
            "fields": [
                {"ref": {"type": "measure_name"}, "legend_item_id": 0},
                {"ref": {"type": "id", "id": "f_region"}, "legend_item_id": 1},
            ],
        }));
        let legend = make(&demo_dataset(), &request).unwrap();
        let item = &legend.entries[0].item;
        assert_eq!(item.id, MEASURE_NAME_ID);
        assert_eq!(item.title, MEASURE_NAME_TITLE);
        assert_eq!(item.item_type, ITEM_TYPE_MEASURE_NAME);
        assert_eq!(item.legend_item_id, 0);
        assert_eq!(item.data_type, UserDataType::String);
        assert_eq!(item.field_type, FieldType::Dimension);
        // The role arrives from the client (`row` by default), the item is never selected.
        assert_eq!(item.role_spec.role, Role::Row);
        assert!(!legend.entries[0].in_result_set);
        assert_eq!(legend.select_ids(), vec![1]);

        let dimension_name = parse(json!({
            "fields": [{"ref": {"type": "dimension_name"}, "legend_item_id": 3}],
        }));
        let legend = make(&demo_dataset(), &dimension_name).unwrap();
        assert_eq!(legend.entries[0].item.item_type, ITEM_TYPE_DIMENSION_NAME);
        assert_eq!(legend.entries[0].item.id, DIMENSION_NAME_ID);
    }

    #[test]
    fn a_placeholder_is_the_pseudo_field_of_a_totals_block() {
        let request = parse(json!({
            "fields": [{
                "ref": {"type": "placeholder"},
                "role_spec": {"role": "template", "template": ""},
                "block_id": 1,
            }],
        }));
        let legend = make(&demo_dataset(), &request).unwrap();
        let item = &legend.entries[0].item;
        assert_eq!(item.id, PLACEHOLDER_ID);
        assert_eq!(item.title, PLACEHOLDER_TITLE);
        assert_eq!(item.item_type, ITEM_TYPE_PLACEHOLDER);
        assert_eq!(item.data_type, UserDataType::String);
        assert_eq!(item.field_type, FieldType::Dimension);
        assert_eq!(item.role_spec, RoleSpec::template(""));
    }

    #[test]
    fn a_parameter_item_is_in_the_legend_but_not_in_the_select_list() {
        let request = parse(json!({
            "fields": [{"ref": {"type": "id", "id": "p_scale"}, "role_spec": {"role": "parameter"}}],
            "parameter_values": [{"ref": {"type": "id", "id": "p_scale"}, "value": 2}],
        }));
        let legend = make(&dataset_with_parameter(), &request).unwrap();
        assert_eq!(legend.select_ids(), Vec::<i64>::new());
        assert_eq!(legend.entries.len(), 2);
        assert_eq!(legend.entries[1].item.role_spec.role, Role::Parameter);
    }

    #[test]
    fn a_template_item_keeps_its_template_and_its_value() {
        let request = parse(json!({
            "fields": [{
                "ref": {"type": "id", "id": "f_region"},
                "role_spec": {"role": "template", "template": "{{f_region}}", "visibility": "visible"},
            }],
        }));
        let legend = make(&demo_dataset(), &request).unwrap();
        let entry = &legend.entries[0];
        assert!(entry.in_result_set);
        assert_eq!(entry.item.role_spec, RoleSpec::template("{{f_region}}"));
        assert_eq!(legend.select_ids(), vec![0]);
    }

    #[test]
    fn preview_legend_skips_parameters_and_hidden_fields() {
        let mut dataset = dataset_with_parameter();
        dataset.fields[2].hidden = true; // f_year
        let legend = preview_legend(&dataset);
        let ids: Vec<&str> = legend
            .entries
            .iter()
            .map(|entry| entry.item.id.as_str())
            .collect();
        assert_eq!(ids, vec!["f_region", "f_sales", "f_calc"]);
        assert_eq!(legend.select_ids(), vec![0, 1, 2]);
        assert!(legend
            .entries
            .iter()
            .all(|entry| entry.item.role_spec == RoleSpec::row()));
    }
}
