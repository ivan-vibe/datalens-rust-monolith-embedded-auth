//! `dl_pivot.primitives` + `dl_pivot.stream_modifiers` + `dl_pivot.native.{data_frame,
//! transformer,paginator}`.
//!
//! The stream pipeline is the same as in Python:
//!
//! ```text
//! MergedQueryDataRow -> [DataCell]            DataCellConverter
//!                    -> [(dims, value)]       MeasureDataTransposer
//!                    -> NativePivotDataFrame  NativePivotTransformer
//! ```
//!
//! `DataCellConverter` turns one row into one cell per `pivot_item_id` that references the
//! cell's `legend_item_id` (a field can be on both axes), `MeasureDataTransposer` folds the
//! measure fields into a single pseudo-dimension (`Measure Names`) and multiplies the rows by
//! the number of measures, and the data frame keys the values by the dimension values of both
//! axes.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use dl_core::FieldType;
use dl_query::Role;
use serde_json::Value;

use super::legend::PivotLegend;
use super::PivotError;
use crate::dto::{DataRow, PivotHeaderInfo, PivotHeaderRole, PivotHeaderRoleSpec, PivotRole};
use crate::legend::{Legend, MEASURE_NAME_TITLE};

/// Which axis the sorter/paginator is looking at (`dl_pivot.primitives.SortAxis`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortAxis {
    Columns,
    Rows,
}

/// `dl_pivot.primitives.DataCell.value`.
///
/// Python packs values into a hashable wrapper (`HashableValuePackerBase`); the cell value is
/// already JSON here, so the wrapper only has to make `dict`/`list` hashable — that is what
/// [`CellValue::hashable`] does (sorted keys, see `hashable_packing.py`).
#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    /// A value from the query.
    Value(Value),
    /// `MeasureNameValue`: the title of a measure plus its `pivot_item_id`.
    ///
    /// The transposer puts it into the `Measure Names` dimension, and the serializer unwraps
    /// it back to the title (`PivotTable.unpack_value`).
    MeasureName { value: String, measure_piid: i64 },
}

impl CellValue {
    /// A hashable representation: JSON objects compare by their sorted keys, exactly like
    /// `dl_pivot.hashable_packing.HashableDictWrapper` does.
    pub fn hashable(&self) -> String {
        match self {
            Self::Value(Value::String(text)) => format!("s{text}"),
            Self::Value(value) => format!("j{}", canonical_json(value)),
            Self::MeasureName {
                value,
                measure_piid,
            } => format!("m{value}/{measure_piid}"),
        }
    }

    /// The value as the response sends it (`PivotTable.unpack_value`).
    pub fn to_json(&self) -> Value {
        match self {
            Self::Value(value) => value.clone(),
            Self::MeasureName { value, .. } => Value::String(value.clone()),
        }
    }
}

/// `json.dumps(value, sort_keys=True)` for the parts where dict order matters.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let items: Vec<String> = keys
                .into_iter()
                .filter_map(|key| {
                    let value = map.get(key)?;
                    let key = serde_json::to_string(key).ok()?;
                    Some(format!("{key}:{}", canonical_json(value)))
                })
                .collect();
            format!("{{{}}}", items.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

/// `dl_pivot.primitives.DataCell`.
///
/// `Eq`/`Hash` go through [`CellValue::hashable`] so the cell can key the pivot data — which
/// is exactly why Python packs values before putting them into a `dict`.
#[derive(Debug, Clone)]
pub struct DataCell {
    pub value: CellValue,
    pub legend_item_id: i64,
    pub pivot_item_id: i64,
}

impl DataCell {
    pub fn new(value: CellValue, legend_item_id: i64, pivot_item_id: i64) -> Self {
        Self {
            value,
            legend_item_id,
            pivot_item_id,
        }
    }
}

impl PartialEq for DataCell {
    fn eq(&self, other: &Self) -> bool {
        self.legend_item_id == other.legend_item_id
            && self.pivot_item_id == other.pivot_item_id
            && self.value.hashable() == other.value.hashable()
    }
}

impl Eq for DataCell {}

impl Hash for DataCell {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hashable().hash(state);
        self.legend_item_id.hash(state);
        self.pivot_item_id.hash(state);
    }
}

/// `dl_pivot.primitives.DataCellVector`: the cells of one table cell (a measure value plus
/// its annotations; headers always hold a single cell).
pub type DataCellVector = Vec<DataCell>;

pub fn main_value(vector: &DataCellVector) -> Option<&CellValue> {
    vector.first().map(|cell| &cell.value)
}

pub fn main_legend_item_id(vector: &DataCellVector) -> Option<i64> {
    vector.first().map(|cell| cell.legend_item_id)
}

pub fn main_pivot_item_id(vector: &DataCellVector) -> Option<i64> {
    vector.first().map(|cell| cell.pivot_item_id)
}

/// `dl_pivot.native.data_frame.FlatPivotDataKey`: the dimension values of one row or column.
pub type FlatPivotDataKey = Vec<DataCellVector>;

/// `dl_pivot.stream_modifiers.TransposedDataRow`: the dimensions of one future row plus the
/// value of that cell (`None` when the pivot has no measures at all).
pub type TransposedRow = (Vec<DataCellVector>, Option<DataCellVector>);

/// `dl_pivot.native.data_frame.NativePivotDataFrame`.
///
/// Python keeps `data: dict[DoublePivotDataKey, DataCellVector]`; a nested map avoids cloning
/// both keys for every lookup and keeps the row order (`row_keys`) separate from the data.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NativePivotDataFrame {
    /// `row key -> column key -> value`, i.e. `DoublePivotDataKey` without the cloning.
    pub data: HashMap<FlatPivotDataKey, HashMap<FlatPivotDataKey, DataCellVector>>,
    /// The row/column order: a `Vec` because the sorter and the paginator reorder it (Python
    /// starts from `list(set(...))`, i.e. an arbitrary order that the sorter then fixes).
    pub row_keys: Vec<FlatPivotDataKey>,
    pub column_keys: Vec<FlatPivotDataKey>,
    /// `PivotDataFrame.headers_info`, filled by [`Self::populate_headers_info`].
    pub headers_info: HashMap<FlatPivotDataKey, PivotHeaderInfo>,
}

impl NativePivotDataFrame {
    pub fn get(
        &self,
        row_key: &FlatPivotDataKey,
        column_key: &FlatPivotDataKey,
    ) -> Option<&DataCellVector> {
        self.data.get(row_key).and_then(|row| row.get(column_key))
    }

    /// `headers_info[key]`: `defaultdict(PivotHeaderInfo)` in Python.
    pub fn info(&self, key: &FlatPivotDataKey) -> PivotHeaderInfo {
        self.headers_info.get(key).cloned().unwrap_or_default()
    }

    pub fn info_mut(&mut self, key: &FlatPivotDataKey) -> &mut PivotHeaderInfo {
        self.headers_info.entry(key.clone()).or_default()
    }

    pub fn keys(&self, axis: SortAxis) -> &Vec<FlatPivotDataKey> {
        match axis {
            SortAxis::Rows => &self.row_keys,
            SortAxis::Columns => &self.column_keys,
        }
    }

    pub fn keys_mut(&mut self, axis: SortAxis) -> &mut Vec<FlatPivotDataKey> {
        match axis {
            SortAxis::Rows => &mut self.row_keys,
            SortAxis::Columns => &mut self.column_keys,
        }
    }

    /// `PivotDataFrame.populate_headers_info`: a header that holds a `template` (`total`)
    /// value is marked as such, so the sorter can keep it at the end of its axis.
    pub fn populate_headers_info(&mut self, legend: &Legend) {
        let total_ids: HashSet<i64> = legend.total_ids().into_iter().collect();
        let keys: Vec<FlatPivotDataKey> = self
            .column_keys
            .iter()
            .chain(self.row_keys.iter())
            .cloned()
            .collect();
        for key in keys {
            let is_total = key.iter().any(|vector| {
                main_legend_item_id(vector).is_some_and(|liid| total_ids.contains(&liid))
            });
            let info = self.info_mut(&key);
            if is_total {
                info.role_spec = PivotHeaderRoleSpec {
                    role: PivotHeaderRole::Total,
                };
            }
        }
    }

    /// `NativePivotDataFrame.iter_values_along_axis`: the values of one line of the table.
    ///
    /// With `axis == Columns` the line is a *row* (fixed `dim_values`), and the other way
    /// round — that is what the measure sorting reads.
    pub fn iter_values_along_axis(
        &self,
        axis: SortAxis,
        dim_values: &FlatPivotDataKey,
    ) -> Vec<Option<&DataCellVector>> {
        match axis {
            SortAxis::Columns => self
                .column_keys
                .iter()
                .map(|column_key| self.get(dim_values, column_key))
                .collect(),
            SortAxis::Rows => self
                .row_keys
                .iter()
                .map(|row_key| self.get(row_key, dim_values))
                .collect(),
        }
    }
}

/// `NativePivotTransformer.pivot`: rows -> data frame.
///
/// The two stream modifiers are inlined, because they only exist to be chained:
/// [`convert_row`] is `DataCellConverter`, [`Transposer`] is `MeasureDataTransposer`.
pub fn pivot(
    rows: &[DataRow],
    legend: &Legend,
    pivot_legend: &PivotLegend,
) -> Result<NativePivotDataFrame, PivotError> {
    let transposer = Transposer::new(pivot_legend)?;
    let liid_to_piids = liid_to_piids_map(pivot_legend);
    let remap = legend_item_id_map(legend, pivot_legend);

    let row_piids: Vec<i64> = pivot_legend
        .list_for_role(PivotRole::Row)
        .iter()
        .map(|item| item.pivot_item_id)
        .collect();
    let column_piids: Vec<i64> = pivot_legend
        .list_for_role(PivotRole::Column)
        .iter()
        .map(|item| item.pivot_item_id)
        .collect();
    let dimension_piids: HashSet<i64> = row_piids
        .iter()
        .chain(column_piids.iter())
        .copied()
        .collect();

    let mut data: HashMap<FlatPivotDataKey, HashMap<FlatPivotDataKey, DataCellVector>> =
        HashMap::new();
    let mut row_keys: Vec<FlatPivotDataKey> = Vec::new();
    let mut column_keys: Vec<FlatPivotDataKey> = Vec::new();
    let mut row_seen: HashSet<FlatPivotDataKey> = HashSet::new();
    let mut column_seen: HashSet<FlatPivotDataKey> = HashSet::new();

    for row in rows {
        let cells = convert_row(row, &liid_to_piids, &remap);
        for (dim_vectors, value_vector) in transposer.expand_row(&cells)? {
            let dim_values = dimension_values(&dim_vectors);
            if let Some(error) = uneven_columns(&dim_values, &dimension_piids) {
                return Err(error);
            }

            let row_key: FlatPivotDataKey = row_piids
                .iter()
                .filter_map(|piid| dim_values.get(piid).map(|vector| (*vector).clone()))
                .collect();
            let column_key: FlatPivotDataKey = column_piids
                .iter()
                .filter_map(|piid| dim_values.get(piid).map(|vector| (*vector).clone()))
                .collect();

            if row_seen.insert(row_key.clone()) {
                row_keys.push(row_key.clone());
            }
            if column_seen.insert(column_key.clone()) {
                column_keys.push(column_key.clone());
            }
            if let Some(value_vector) = value_vector {
                data.entry(row_key)
                    .or_default()
                    .insert(column_key, value_vector);
            }
        }
    }

    let mut dframe = NativePivotDataFrame {
        data,
        row_keys,
        column_keys,
        headers_info: HashMap::new(),
    };
    dframe.populate_headers_info(legend);
    Ok(dframe)
}

/// `PivotLegend.leg_item_id_to_pivot_item_id_list`, built once per request.
fn liid_to_piids_map(pivot_legend: &PivotLegend) -> HashMap<i64, Vec<i64>> {
    let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
    for item in pivot_legend.items() {
        for legend_item_id in &item.legend_item_ids {
            map.entry(*legend_item_id)
                .or_default()
                .push(item.pivot_item_id);
        }
    }
    map
}

/// `DataCellConverter._legend_item_id_map`: same values of the same dimension from different
/// blocks must fall into the same pivot group.
///
/// Every totals block *clones* the dimension items it keeps (and the `template` placeholders),
/// so the same `2020` arrives twice with different ids. Python remaps a clone onto the first
/// legend item that carries the same field (or the same template) for the same pivot item, and
/// the first one is the base block's, since the blocks append their items after it.
fn legend_item_id_map(legend: &Legend, pivot_legend: &PivotLegend) -> HashMap<i64, i64> {
    let mut main_fields: HashMap<(Option<String>, i64), i64> = HashMap::new();
    let mut main_templates: HashMap<(Option<String>, i64), i64> = HashMap::new();
    let mut remap: HashMap<i64, i64> = HashMap::new();

    for entry in &legend.entries {
        let item = &entry.item;
        // `if item.field_type == FieldType.MEASURE: continue`: a measure is a value, not a
        // group, so `Sales` and `Sales (total)` stay apart.
        if item.field_type == FieldType::Measure {
            continue;
        }
        for pivot_item_id in pivot_legend.liid_to_piids(item.legend_item_id) {
            // `FieldObjSpec` vs `TemplateRoleSpec`: a placeholder is keyed by its template
            // text, everything else by the field it points at.
            let (key, main): ((Option<String>, i64), &mut HashMap<_, _>) =
                if item.role_spec.role == Role::Template {
                    (
                        (item.role_spec.template.clone(), pivot_item_id),
                        &mut main_templates,
                    )
                } else if item.is_pseudo_dimension() {
                    // `Measure Names`/`Dimension Names` have no field of their own.
                    continue;
                } else {
                    ((Some(item.id.clone()), pivot_item_id), &mut main_fields)
                };
            match main.get(&key) {
                Some(first) => {
                    remap.insert(item.legend_item_id, *first);
                }
                None => {
                    main.insert(key, item.legend_item_id);
                }
            }
        }
    }

    remap
}

/// `DataCellConverter`: one row -> one cell per `pivot_item_id` of every selected legend item.
fn convert_row(
    row: &DataRow,
    liid_to_piids: &HashMap<i64, Vec<i64>>,
    remap: &HashMap<i64, i64>,
) -> Vec<DataCell> {
    let mut cells = Vec::with_capacity(row.data.len());
    for (value, legend_item_id) in row.data.iter().zip(row.legend.iter()) {
        let Some(pivot_item_ids) = liid_to_piids.get(legend_item_id) else {
            continue;
        };
        let legend_item_id = remap
            .get(legend_item_id)
            .copied()
            .unwrap_or(*legend_item_id);
        for pivot_item_id in pivot_item_ids {
            cells.push(DataCell::new(
                CellValue::Value(value.clone()),
                legend_item_id,
                *pivot_item_id,
            ));
        }
    }
    cells
}

/// `{cell.pivot_item_id: vector}` for one transformed row.
fn dimension_values(dim_vectors: &[DataCellVector]) -> HashMap<i64, &DataCellVector> {
    let mut values = HashMap::with_capacity(dim_vectors.len());
    for vector in dim_vectors {
        if let Some(pivot_item_id) = main_pivot_item_id(vector) {
            values.insert(pivot_item_id, vector);
        }
    }
    values
}

/// `NativePivotTransformer`'s `set(dim_values) != dim_piid_set` check.
fn uneven_columns(
    dim_values: &HashMap<i64, &DataCellVector>,
    dimension_piids: &HashSet<i64>,
) -> Option<PivotError> {
    if dimension_piids.len() == dim_values.len()
        && dimension_piids
            .iter()
            .all(|piid| dim_values.contains_key(piid))
    {
        return None;
    }
    let mut expected: Vec<i64> = dimension_piids.iter().copied().collect();
    expected.sort_unstable();
    let mut actual: Vec<i64> = dim_values.keys().copied().collect();
    actual.sort_unstable();
    Some(PivotError::UnevenDataColumns { expected, actual })
}

/// `MeasureDataTransposer`: fold the measure fields into a `Measure Names` pseudo-dimension.
///
/// One row becomes one row per measure; the measure's own cell stays in the value vector,
/// followed by its annotations (which is why a value vector can hold more than one cell).
struct Transposer {
    dimension_piids: HashSet<i64>,
    /// `_measure_piid_and_name_mask_transp`: `(pivot item, title)` of every measure.
    measure_names: Vec<(i64, String)>,
    measure_name_piids: Vec<i64>,
    measure_name_legend_item_id: Option<i64>,
    /// `_anno_piid_list_by_measure_liid`: which annotations belong to which measure.
    anno_piids_by_measure_liid: HashMap<i64, Vec<i64>>,
}

impl Transposer {
    fn new(pivot_legend: &PivotLegend) -> Result<Self, PivotError> {
        let mut dimension_piids: HashSet<i64> = HashSet::new();
        for role in [PivotRole::Row, PivotRole::Column] {
            for item in pivot_legend.list_for_role(role) {
                dimension_piids.insert(item.pivot_item_id);
            }
        }

        let measure_name_piids = pivot_legend.measure_name_piids();
        let measures = pivot_legend.list_for_role(PivotRole::Measure);
        let annotations = pivot_legend.list_for_role(PivotRole::Annotation);

        let mut anno_piids_by_measure_liid: HashMap<i64, Vec<i64>> = HashMap::new();
        for measure in &measures {
            for legend_item_id in &measure.legend_item_ids {
                let entry = anno_piids_by_measure_liid
                    .entry(*legend_item_id)
                    .or_default();
                for annotation in &annotations {
                    // `target_legend_item_ids is None` means "applies to all measures".
                    let applies = annotation
                        .role_spec
                        .target_legend_item_ids
                        .as_ref()
                        .is_none_or(|targets| targets.contains(legend_item_id));
                    if applies {
                        entry.push(annotation.pivot_item_id);
                    }
                }
            }
        }

        let measure_name_legend_item_id = match measure_name_piids.first() {
            Some(pivot_item_id) => pivot_legend
                .get_item(*pivot_item_id)?
                .legend_item_ids
                .first()
                .copied(),
            None => None,
        };

        Ok(Self {
            dimension_piids,
            measure_names: measures
                .iter()
                .map(|item| (item.pivot_item_id, item.title.clone()))
                .collect(),
            measure_name_piids,
            measure_name_legend_item_id,
            anno_piids_by_measure_liid,
        })
    }

    /// `MeasureDataTransposer._expand_row`.
    fn expand_row(&self, row: &[DataCell]) -> Result<Vec<TransposedRow>, PivotError> {
        // The dimension cells keep the order of the row, exactly like in Python.
        let common: Vec<DataCellVector> = row
            .iter()
            .filter(|cell| self.dimension_piids.contains(&cell.pivot_item_id))
            .map(|cell| vec![cell.clone()])
            .collect();

        let mut values_by_piid: HashMap<i64, &DataCell> = HashMap::new();
        for cell in row {
            if !self.dimension_piids.contains(&cell.pivot_item_id) {
                values_by_piid.insert(cell.pivot_item_id, cell);
            }
        }

        let mut out: Vec<TransposedRow> = Vec::with_capacity(self.measure_names.len().max(1));
        for (measure_piid, measure_name) in &self.measure_names {
            let mut dimensions = common.clone();
            // There can be several `measure_name` items (one per measure-names pivot item).
            for mname_piid in &self.measure_name_piids {
                let measure_name_legend_item_id = self
                    .measure_name_legend_item_id
                    .ok_or(PivotError::MissingPseudoDimension(MEASURE_NAME_TITLE))?;
                dimensions.push(vec![DataCell::new(
                    CellValue::MeasureName {
                        value: measure_name.clone(),
                        measure_piid: *measure_piid,
                    },
                    measure_name_legend_item_id,
                    *mname_piid,
                )]);
            }

            let value_cell = *values_by_piid
                .get(measure_piid)
                .ok_or(PivotError::MeasureWithoutCell(*measure_piid))?;
            let mut value_vector = vec![value_cell.clone()];
            if let Some(anno_piids) = self
                .anno_piids_by_measure_liid
                .get(&value_cell.legend_item_id)
            {
                for anno_piid in anno_piids {
                    if let Some(cell) = values_by_piid.get(anno_piid) {
                        value_vector.push((*cell).clone());
                    }
                }
            }
            out.push((dimensions, Some(value_vector)));
        }

        if self.measure_names.is_empty() {
            // There are no measures: the dimensions alone still make a table row.
            out.push((common, None));
        }
        Ok(out)
    }
}

/// `NativePivotPaginator.paginate`: crop the rows of the finished table.
///
/// Columns always stay (`_remove_empty_columns` is off by default in Python too), and the
/// data of the removed rows goes with them.
pub fn paginate(
    dframe: &mut NativePivotDataFrame,
    limit_rows: Option<i64>,
    offset_rows: Option<i64>,
) {
    let start = offset_rows.unwrap_or(0).max(0) as usize;
    let end = match limit_rows {
        Some(limit) => start.saturating_add(limit.max(0) as usize),
        None => dframe.row_keys.len(),
    };
    let start = start.min(dframe.row_keys.len());
    let end = end.min(dframe.row_keys.len());
    let new_row_keys: Vec<FlatPivotDataKey> = dframe.row_keys[start..end].to_vec();

    let mut data: HashMap<FlatPivotDataKey, HashMap<FlatPivotDataKey, DataCellVector>> =
        HashMap::new();
    for row_key in &new_row_keys {
        if let Some(row) = dframe.data.get(row_key) {
            data.insert(row_key.clone(), row.clone());
        }
    }
    dframe.row_keys = new_row_keys;
    dframe.data = data;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{LegendItem, PivotRole, PivotRoleSpec, RoleSpec};
    use crate::legend::{
        LegendEntry, ITEM_TYPE_FIELD, ITEM_TYPE_PLACEHOLDER, PLACEHOLDER_ID, PLACEHOLDER_TITLE,
    };
    use crate::pivot::{PivotItemType, PivotLegendItem};
    use crate::testing::{pivot_item, pivot_legend, pivot_spec};
    use dl_core::UserDataType;

    fn cells_of(vector: &DataCellVector) -> (serde_json::Value, i64, i64) {
        let cell = vector.first().expect("a header vector always has a cell");
        (
            cell.value.to_json(),
            cell.legend_item_id,
            cell.pivot_item_id,
        )
    }

    fn rows() -> Vec<DataRow> {
        vec![
            DataRow {
                data: vec![serde_json::json!("Europe"), serde_json::json!(42)],
                legend: vec![0, 1],
            },
            DataRow {
                data: vec![serde_json::json!("Asia"), serde_json::json!(7)],
                legend: vec![0, 1],
            },
        ]
    }

    #[test]
    fn measures_are_transposed_into_measure_names() {
        let legend = pivot_legend();
        let pivot_legend = super::super::make_pivot_legend(
            &legend,
            &pivot_spec(vec![
                pivot_item(vec![0], PivotRole::Row),
                pivot_item(vec![1], PivotRole::Measure),
            ]),
        )
        .unwrap();

        let table = pivot(&rows(), &legend, &pivot_legend).unwrap();
        // One column: the patched `Measure Names` item with a single measure.
        assert_eq!(table.column_keys.len(), 1);
        assert_eq!(
            cells_of(&table.column_keys[0][0]),
            (serde_json::json!("Sales"), 2, 2)
        );
        // Rows keep the order of the data (and their own `legend_item_id`).
        assert_eq!(table.row_keys.len(), 2);
        assert_eq!(
            cells_of(&table.row_keys[0][0]),
            (serde_json::json!("Europe"), 0, 0)
        );
        assert_eq!(
            cells_of(&table.row_keys[1][0]),
            (serde_json::json!("Asia"), 0, 0)
        );

        let value = table
            .get(&table.row_keys[0], &table.column_keys[0])
            .expect("Europe has a value");
        assert_eq!(cells_of(value), (serde_json::json!(42), 1, 1));
        let value = table
            .get(&table.row_keys[1], &table.column_keys[0])
            .expect("Asia has a value");
        assert_eq!(cells_of(value), (serde_json::json!(7), 1, 1));
    }

    #[test]
    fn dimensions_only_make_a_table_without_values() {
        let legend = pivot_legend();
        let pivot_legend = super::super::make_pivot_legend(
            &legend,
            &pivot_spec(vec![pivot_item(vec![0], PivotRole::Row)]),
        )
        .unwrap();

        let table = pivot(&rows(), &legend, &pivot_legend).unwrap();
        // No column dimensions: one empty column key, and no values at all.
        assert_eq!(table.column_keys, vec![Vec::<DataCellVector>::new()]);
        assert_eq!(table.row_keys.len(), 2);
        assert!(table
            .get(&table.row_keys[0], &table.column_keys[0])
            .is_none());
    }

    fn item(
        pivot_item_id: i64,
        legend_item_ids: Vec<i64>,
        role: PivotRole,
        title: &str,
    ) -> PivotLegendItem {
        PivotLegendItem {
            pivot_item_id,
            legend_item_ids,
            role_spec: PivotRoleSpec::new(role),
            title: title.to_string(),
            item_type: PivotItemType::StreamItem,
        }
    }

    #[test]
    fn rows_without_a_dimension_cell_are_rejected() {
        // `Dimension Names` (liid 3) is a column dimension, but no row carries it.
        let legend = pivot_legend();
        let pivot_legend = PivotLegend::new(vec![
            item(0, vec![0], PivotRole::Row, "Region"),
            item(1, vec![3], PivotRole::Column, "Dimension Names"),
        ]);
        let error = pivot(&rows(), &legend, &pivot_legend).unwrap_err();
        assert_eq!(
            error,
            PivotError::UnevenDataColumns {
                expected: vec![0, 1],
                actual: vec![0],
            }
        );
    }

    #[test]
    fn a_measure_without_a_cell_is_reported() {
        // `Sales` is a pivot measure, but the rows only carry the dimension.
        let legend = pivot_legend();
        let pivot_legend = PivotLegend::new(vec![
            item(0, vec![0], PivotRole::Row, "Region"),
            item(1, vec![1], PivotRole::Measure, "Sales"),
        ]);
        let rows = vec![DataRow {
            data: vec![serde_json::json!("Europe")],
            legend: vec![0],
        }];
        let error = pivot(&rows, &legend, &pivot_legend).unwrap_err();
        assert_eq!(error, PivotError::MeasureWithoutCell(1));
    }

    #[test]
    fn dimension_clones_of_totals_blocks_collapse_onto_the_first_item() {
        // The legend of a totals request: the base `Region` (0) and `Sales` (1), then a clone
        // of `Region` (2) and two `""` templates (3 and 4) the blocks invented.
        let dimension = |legend_item_id: i64, id: &str, role_spec: RoleSpec| LegendItem {
            legend_item_id,
            id: id.to_string(),
            title: "Region".to_string(),
            data_type: UserDataType::String,
            field_type: FieldType::Dimension,
            item_type: ITEM_TYPE_FIELD.to_string(),
            role_spec,
        };
        let template = |legend_item_id: i64| LegendItem {
            legend_item_id,
            id: PLACEHOLDER_ID.to_string(),
            title: PLACEHOLDER_TITLE.to_string(),
            data_type: UserDataType::String,
            field_type: FieldType::Dimension,
            item_type: ITEM_TYPE_PLACEHOLDER.to_string(),
            role_spec: RoleSpec {
                template: Some(String::new()),
                ..RoleSpec::new(Role::Template)
            },
        };
        let legend = Legend {
            entries: vec![
                LegendEntry {
                    item: dimension(0, "f_region", RoleSpec::new(Role::Row)),
                    in_result_set: true,
                    select_index: Some(0),
                    block_id: None,
                },
                LegendEntry {
                    item: LegendItem {
                        legend_item_id: 1,
                        id: "f_sales".to_string(),
                        title: "Sales".to_string(),
                        data_type: UserDataType::Integer,
                        field_type: FieldType::Measure,
                        item_type: ITEM_TYPE_FIELD.to_string(),
                        role_spec: RoleSpec::new(Role::Measure),
                    },
                    in_result_set: true,
                    select_index: Some(1),
                    block_id: None,
                },
                LegendEntry {
                    item: dimension(2, "f_region", RoleSpec::new(Role::Row)),
                    in_result_set: true,
                    select_index: Some(2),
                    block_id: None,
                },
                LegendEntry {
                    item: template(3),
                    in_result_set: true,
                    select_index: Some(3),
                    block_id: None,
                },
                LegendEntry {
                    item: template(4),
                    in_result_set: true,
                    select_index: Some(4),
                    block_id: None,
                },
            ],
        };
        let pivot_legend = PivotLegend::new(vec![
            item(0, vec![0, 2, 3, 4], PivotRole::Row, "Region"),
            item(1, vec![1], PivotRole::Measure, "Sales"),
        ]);

        let remap = legend_item_id_map(&legend, &pivot_legend);
        // The clone of a dimension follows the base item, and the second template follows the
        // first one; the measure is a value and is never remapped.
        assert_eq!(remap.get(&2), Some(&0));
        assert_eq!(remap.get(&4), Some(&3));
        assert_eq!(remap.get(&1), None);
        assert_eq!(remap.get(&0), None);
        assert_eq!(remap.get(&3), None);
    }

    #[test]
    fn pagination_crops_rows_and_keeps_columns() {
        let legend = pivot_legend();
        let pivot_legend = super::super::make_pivot_legend(
            &legend,
            &pivot_spec(vec![
                pivot_item(vec![0], PivotRole::Row),
                pivot_item(vec![1], PivotRole::Measure),
            ]),
        )
        .unwrap();
        let mut table = pivot(&rows(), &legend, &pivot_legend).unwrap();

        paginate(&mut table, Some(1), Some(1));
        assert_eq!(table.row_keys.len(), 1);
        assert_eq!(table.column_keys.len(), 1);
        assert_eq!(
            cells_of(&table.row_keys[0][0]),
            (serde_json::json!("Asia"), 0, 0)
        );
        assert!(table
            .get(&table.row_keys[0], &table.column_keys[0])
            .is_some());
    }
}
