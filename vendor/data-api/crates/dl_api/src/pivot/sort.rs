//! `dl_pivot.sort_helpers` + `dl_pivot.sort_strategy` + `dl_pivot.native.sorter`.
//!
//! Sorting happens in two passes, exactly like `NativePivotSorter.sort`:
//!
//! 1. every axis is ordered by its own dimensions, in the order the `pivot.structure` lists
//!    them, honouring each `direction`;
//! 2. then a measure with `sorting` settings reorders the *other* axis by its values in one
//!    selected row/column, which is what the "sort by this column" table header does.
//!
//! The tricky part is the comparison itself: `None`/`null` must sort as `LeastNullValue`, a
//! value that is not part of the dimension data (a totals label) must go to the end in both
//! directions, and numbers are converted by data type before they are compared.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use serde_json::Value;

use dl_core::UserDataType;
use dl_query::Direction;

use super::legend::{PivotItemType, PivotLegend};
use super::transform::{
    main_value, CellValue, DataCellVector, FlatPivotDataKey, NativePivotDataFrame, SortAxis,
};
use super::PivotError;
use crate::dto::{PivotHeaderInfo, PivotMeasureSortingSettings, PivotRole};
use crate::legend::Legend;

/// A value prepared for sorting: `dl_pivot.sort_helpers`' null wrappers plus the normalized
/// value itself.
///
/// The ordering is `TotalLeast < Least < value < TotalGreatest`, which is what the Python
/// wrappers give: `LeastNullValue(-1)` is below the normal null, and `GreatestNullValue(1)`
/// is above everything (that is how a totals label stays at the end when descending).
#[derive(Debug, Clone, PartialEq)]
pub enum SortValue {
    /// `TOTAL_LEAST_NULL_VALUE` (`LeastNullValue(-1)`).
    TotalLeast,
    /// `NORMAL_LEAST_NULL_VALUE` (`LeastNullValue(0)`): a missing value.
    Least,
    /// `TOTAL_GREATEST_NULL_VALUE` (`GreatestNullValue(1)`).
    TotalGreatest,
    /// `MeasureNameSortValueNormalizer`: the position of the measure in the pivot legend.
    MeasureIndex(usize),
    Number(f64),
    Boolean(bool),
    /// Text, already lowercased (Python uses `str.casefold()`).
    Text(String),
    /// Anything else: the canonical JSON, so the sort stays total and deterministic.
    Other(String),
}

impl SortValue {
    fn rank(&self) -> u8 {
        match self {
            Self::TotalLeast => 0,
            Self::Least => 1,
            Self::Number(_) | Self::Boolean(_) | Self::Text(_) | Self::Other(_) => 2,
            Self::MeasureIndex(_) => 2,
            Self::TotalGreatest => 3,
        }
    }
}

impl Ord for SortValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.rank(), other.rank()) {
            (left, right) if left != right => left.cmp(&right),
            // Two nulls of the same kind are equal (`OrderableNullValueBase` compares weights
            // only, and the wrappers are immutable singletons).
            (0 | 3, _) => Ordering::Equal,
            _ => match (self, other) {
                (Self::Number(left), Self::Number(right)) => {
                    left.partial_cmp(right).unwrap_or(Ordering::Equal)
                }
                (Self::Boolean(left), Self::Boolean(right)) => left.cmp(right),
                (Self::Text(left), Self::Text(right)) => left.cmp(right),
                (Self::MeasureIndex(left), Self::MeasureIndex(right)) => left.cmp(right),
                (Self::Other(left), Self::Other(right)) => left.cmp(right),
                // Mixed kinds cannot happen inside one dimension (the data type is single),
                // so the fallback only has to be stable, not meaningful.
                _ => self.rank().cmp(&other.rank()),
            },
        }
    }
}

impl PartialOrd for SortValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `NaN` never reaches a `SortValue` ([`number_value`] turns it into `Least`), so the total
/// order is well defined and `Eq` can be promised even though the enum holds an `f64`.
impl Eq for SortValue {}

/// `dl_pivot.sort_strategy.SortValueNormalizer`: one normalizer per sorted dimension.
///
/// The three variants are the three strategies: a dimension (with the totals handling), a
/// measure (the base normalizer) and `Measure Names` (the measure order).
enum ValueNormalizer {
    Dimension {
        /// `_legend_item_ids`, `template` items excluded.
        legend_item_ids: HashSet<i64>,
        converter: Option<UserDataType>,
        direction: Direction,
    },
    Measure {
        converter: Option<UserDataType>,
    },
    MeasureName {
        order: HashMap<(String, i64), usize>,
    },
}

impl ValueNormalizer {
    /// `_value_converter`: `int`/`float` for numeric dimensions, nothing for the rest.
    fn converter(&self) -> Option<UserDataType> {
        match self {
            Self::Dimension { converter, .. } | Self::Measure { converter } => *converter,
            Self::MeasureName { .. } => None,
        }
    }

    /// `normalize_vector_value`.
    fn normalize(&self, vector: Option<&DataCellVector>) -> SortValue {
        match self {
            Self::MeasureName { order } => {
                let Some(cell) = vector.and_then(|vector| vector.first()) else {
                    return SortValue::Least;
                };
                match &cell.value {
                    CellValue::MeasureName {
                        value,
                        measure_piid,
                    } => order
                        .get(&(value.clone(), *measure_piid))
                        .map_or(SortValue::Least, |index| SortValue::MeasureIndex(*index)),
                    _ => SortValue::Least,
                }
            }
            Self::Dimension {
                legend_item_ids,
                direction,
                ..
            } => {
                let Some(cell) = vector.and_then(|vector| vector.first()) else {
                    return SortValue::Least;
                };
                if !legend_item_ids.contains(&cell.legend_item_id) {
                    // A value that is not part of the main data (a template label of a
                    // total): it stays at the end in both directions, so ascending gets the
                    // greatest placeholder and descending the least one.
                    return match direction {
                        Direction::Asc => SortValue::TotalGreatest,
                        Direction::Desc => SortValue::TotalLeast,
                    };
                }
                self.normalize_value(&cell.value)
            }
            Self::Measure { .. } => match vector.and_then(|vector| vector.first()) {
                Some(cell) => self.normalize_value(&cell.value),
                None => SortValue::Least,
            },
        }
    }

    fn normalize_value(&self, value: &CellValue) -> SortValue {
        let raw = match value {
            CellValue::Value(raw) => raw,
            CellValue::MeasureName { value, .. } => return SortValue::Text(casefold(value)),
        };
        if raw.is_null() {
            return SortValue::Least;
        }
        match self.converter() {
            Some(UserDataType::Integer) => {
                as_integer(raw).map_or(SortValue::Least, |number| SortValue::Number(number as f64))
            }
            Some(UserDataType::Float) => as_float(raw).map_or(SortValue::Least, number_value),
            _ => match raw {
                Value::String(text) => SortValue::Text(casefold(text)),
                Value::Number(number) => number.as_f64().map_or(SortValue::Least, number_value),
                Value::Bool(flag) => SortValue::Boolean(*flag),
                other => SortValue::Other(other.to_string()),
            },
        }
    }
}

/// `float('nan')` sorts like a missing value (`pandas` uses NaN for empty cells).
fn number_value(number: f64) -> SortValue {
    if number.is_nan() {
        SortValue::Least
    } else {
        SortValue::Number(number)
    }
}

/// `int(value)`: an integer stays, a float is truncated, a numeric string is parsed.
///
/// Python raises for `int("1.5")`; the port truncates instead, which only changes requests
/// that mix numeric strings with an `integer` data type.
fn as_integer(raw: &Value) -> Option<i64> {
    match raw {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value.trunc() as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok().or_else(|| {
            text.trim()
                .parse::<f64>()
                .ok()
                .map(|value| value.trunc() as i64)
        }),
        Value::Bool(flag) => Some(i64::from(*flag)),
        _ => None,
    }
}

/// `float(value)`.
fn as_float(raw: &Value) -> Option<f64> {
    match raw {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        Value::Bool(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// `str.casefold()`: `std` only has `to_lowercase`, which gives the same order for the data a
/// pivot usually holds (it differs for the few characters that fold into several, e.g. `ß`).
fn casefold(text: &str) -> String {
    text.to_lowercase()
}

/// `DimensionSortValueStrategy.get_normalizer`.
fn dimension_normalizer(
    legend: &Legend,
    pivot_legend: &PivotLegend,
    pivot_item_id: i64,
) -> Result<ValueNormalizer, PivotError> {
    let item = pivot_legend.get_item(pivot_item_id)?;
    if item.item_type == PivotItemType::MeasureName {
        return Ok(ValueNormalizer::MeasureName {
            order: measure_order(pivot_legend),
        });
    }
    let (legend_item_ids, converter) =
        legend_item_ids_and_converter(legend, &item.legend_item_ids)?;
    Ok(ValueNormalizer::Dimension {
        legend_item_ids,
        converter,
        direction: item.role_spec.dimension_direction(),
    })
}

/// `MeasureSortStrategy.get_normalizer`.
fn measure_normalizer(
    legend: &Legend,
    pivot_legend: &PivotLegend,
    pivot_item_id: i64,
) -> Result<ValueNormalizer, PivotError> {
    let item = pivot_legend.get_item(pivot_item_id)?;
    let (_legend_item_ids, converter) =
        legend_item_ids_and_converter(legend, &item.legend_item_ids)?;
    Ok(ValueNormalizer::Measure { converter })
}

/// `BaseSortValueNormalizer._make_legend_item_ids` + `_make_value_converter`.
///
/// `template` items are left out: their value is the total expression, not a field value, and
/// the data type of a totals label says nothing about the dimension.
fn legend_item_ids_and_converter(
    legend: &Legend,
    legend_item_ids: &[i64],
) -> Result<(HashSet<i64>, Option<UserDataType>), PivotError> {
    let mut ids: HashSet<i64> = HashSet::new();
    let mut data_types: Vec<UserDataType> = Vec::new();
    for legend_item_id in legend_item_ids {
        let item = legend
            .get_item(*legend_item_id)
            .ok_or(PivotError::UnknownItem(*legend_item_id))?;
        if item.role_spec.role == dl_query::Role::Template {
            continue;
        }
        ids.insert(*legend_item_id);
        if !data_types.contains(&item.data_type) {
            data_types.push(item.data_type);
        }
    }
    if data_types.len() > 1 {
        // `assert len(data_types) == 1, "Only single data type is supported ..."`
        return Err(PivotError::ItemsIncompatible {
            role: "dimension",
            detail: format!("mixed data types: {data_types:?}"),
        });
    }
    let converter = match data_types.first() {
        Some(UserDataType::Integer) => Some(UserDataType::Integer),
        Some(UserDataType::Float) => Some(UserDataType::Float),
        _ => None,
    };
    Ok((ids, converter))
}

/// `_measure_name_order_values`: `(title, pivot item) -> position among the measures`.
fn measure_order(pivot_legend: &PivotLegend) -> HashMap<(String, i64), usize> {
    pivot_legend
        .list_for_role(PivotRole::Measure)
        .iter()
        .enumerate()
        .map(|(index, item)| ((item.title.clone(), item.pivot_item_id), index))
        .collect()
}

/// `NativePivotSorter.sort`.
pub fn sort(
    dframe: &mut NativePivotDataFrame,
    legend: &Legend,
    pivot_legend: &PivotLegend,
) -> Result<(), PivotError> {
    // 1. Dimensions of every axis, in the `pivot.structure` order.
    for axis in [SortAxis::Rows, SortAxis::Columns] {
        let items: Vec<_> = match axis {
            SortAxis::Rows => pivot_legend.list_for_role(PivotRole::Row),
            SortAxis::Columns => pivot_legend.list_for_role(PivotRole::Column),
        };
        if items.is_empty() {
            // Nothing to sort here.
            continue;
        }
        let directions: Vec<Direction> = items
            .iter()
            .map(|item| item.role_spec.dimension_direction())
            .collect();
        let normalizers: Vec<ValueNormalizer> = items
            .iter()
            .map(|item| dimension_normalizer(legend, pivot_legend, item.pivot_item_id))
            .collect::<Result<_, _>>()?;
        sort_keys(dframe, axis, &normalizers, &directions);
    }

    // 2. Then the measures, which may override the dimension order.
    for measure in pivot_legend.list_for_role(PivotRole::Measure) {
        let Some(sorting) = measure.role_spec.sorting.clone() else {
            continue;
        };
        for (axis, settings) in [
            (SortAxis::Columns, sorting.column),
            (SortAxis::Rows, sorting.row),
        ] {
            if let Some(settings) = settings {
                sort_by_measure(
                    dframe,
                    legend,
                    pivot_legend,
                    axis,
                    measure.pivot_item_id,
                    &settings,
                )?;
            }
        }
    }

    Ok(())
}

/// `NativePivotSorter._sort_key_list`: order the keys of one axis by their dimension values.
fn sort_keys(
    dframe: &mut NativePivotDataFrame,
    axis: SortAxis,
    normalizers: &[ValueNormalizer],
    directions: &[Direction],
) {
    let mut keys = std::mem::take(dframe.keys_mut(axis));
    keys.sort_by(|left, right| {
        for (index, normalizer) in normalizers.iter().enumerate() {
            let left_value = normalizer.normalize(left.get(index));
            let right_value = normalizer.normalize(right.get(index));
            let mut order = left_value.cmp(&right_value);
            if directions[index] == Direction::Desc {
                // `invert(value)` per dimension: the comparison is reversed, which keeps the
                // sort stable for equal values.
                order = order.reverse();
            }
            if order != Ordering::Equal {
                return order;
            }
        }
        Ordering::Equal
    });
    *dframe.keys_mut(axis) = keys;
}

/// `NativePivotSorter._sort_by_measure`: reorder the other axis by one measure.
fn sort_by_measure(
    dframe: &mut NativePivotDataFrame,
    legend: &Legend,
    pivot_legend: &PivotLegend,
    axis: SortAxis,
    sorting_piid: i64,
    settings: &PivotMeasureSortingSettings,
) -> Result<(), PivotError> {
    let other_axis = match axis {
        SortAxis::Columns => SortAxis::Rows,
        SortAxis::Rows => SortAxis::Columns,
    };

    // The settings identify one header of `axis` by its values and role.
    let mut sorting_dim_values: Option<FlatPivotDataKey> = None;
    for key in dframe.keys(axis).clone() {
        let info = dframe.info(&key);
        if compare_sorting_settings(&key, &info, settings) {
            if sorting_dim_values.is_some() {
                // `header_values + role_spec` must identify one header uniquely.
                return Err(PivotError::SortingAxisAmbiguous);
            }
            dframe.info_mut(&key).sorting_direction = Some(settings.direction);
            sorting_dim_values = Some(key);
        }
    }
    let Some(sorting_dim_values) = sorting_dim_values else {
        return Err(PivotError::SortingAxisNotFound);
    };

    let normalizer = measure_normalizer(legend, pivot_legend, sorting_piid)?;

    // First order the measure values of that line, remembering their original position.
    let mut indexed_values: Vec<(usize, SortValue)> = dframe
        .iter_values_along_axis(other_axis, &sorting_dim_values)
        .into_iter()
        .enumerate()
        .map(|(old_index, value)| (old_index, normalizer.normalize(value)))
        .collect();
    indexed_values.sort_by(|left, right| left.1.cmp(&right.1));

    // Then reorder the keys of the other axis accordingly.
    let other_keys = dframe.keys(other_axis).clone();
    let mut key_to_new_index: HashMap<FlatPivotDataKey, usize> = HashMap::new();
    for (new_index, (old_index, _)) in indexed_values.iter().enumerate() {
        if let Some(key) = other_keys.get(*old_index) {
            key_to_new_index.insert(key.clone(), new_index);
        }
    }
    let descending = settings.direction == Direction::Desc;
    let mut keys = other_keys;
    keys.sort_by(|left, right| {
        let left_index = key_to_new_index.get(left).copied().unwrap_or(usize::MAX);
        let right_index = key_to_new_index.get(right).copied().unwrap_or(usize::MAX);
        if descending {
            right_index.cmp(&left_index)
        } else {
            left_index.cmp(&right_index)
        }
    });
    *dframe.keys_mut(other_axis) = keys;

    // `_axis_has_total` hack: a total always goes last, whatever the direction was.
    if axis_has_total(dframe, other_axis)? {
        let total_key = dframe
            .keys(other_axis)
            .iter()
            .find(|key| dframe_info_is_total(dframe.headers_info.get(*key)))
            .cloned();
        if let Some(total_key) = total_key {
            let keys = dframe.keys_mut(other_axis);
            if let Some(position) = keys.iter().position(|key| *key == total_key) {
                let total_key = keys.remove(position);
                keys.push(total_key);
            }
        }
    }

    Ok(())
}

fn dframe_info_is_total(info: Option<&PivotHeaderInfo>) -> bool {
    info.is_some_and(|info| info.role_spec.role == crate::dto::PivotHeaderRole::Total)
}

/// `PivotSorter._axis_has_total`: more than one total per axis cannot be sorted around.
fn axis_has_total(dframe: &NativePivotDataFrame, axis: SortAxis) -> Result<bool, PivotError> {
    let total_count = dframe
        .keys(axis)
        .iter()
        .filter(|key| dframe_info_is_total(dframe.headers_info.get(*key)))
        .count();
    if total_count > 1 {
        return Err(PivotError::SortingWithSubtotals);
    }
    Ok(total_count == 1)
}

/// `PivotHeader.compare_sorting_settings`.
///
/// `PivotHeaderValue.value` is a string, while a header value can be any JSON scalar: Python
/// compares them with `==`, so `2023` never matches `"2023"` and the port does the same.
fn compare_sorting_settings(
    key: &FlatPivotDataKey,
    info: &PivotHeaderInfo,
    settings: &PivotMeasureSortingSettings,
) -> bool {
    if info.role_spec != settings.role_spec {
        return false;
    }

    let sorting_values: Vec<&str> = settings
        .header_values
        .iter()
        .map(|header_value| header_value.value.as_str())
        .collect();

    let mut main_values: Vec<&CellValue> =
        key.iter().filter_map(|vector| main_value(vector)).collect();
    let Some(last_value) = main_values.pop() else {
        return sorting_values.is_empty();
    };

    let mut prefix: Vec<Option<String>> = main_values
        .iter()
        .map(|value| as_header_string(value))
        .collect();

    let matches = |values: &Vec<Option<String>>| {
        values.len() == sorting_values.len()
            && values
                .iter()
                .zip(sorting_values.iter())
                .all(|(value, expected)| value.as_deref() == Some(*expected))
    };

    if matches!(last_value, CellValue::MeasureName { .. }) {
        // The last measure name may be hidden, so try to compare without it first.
        if matches(&prefix) {
            return true;
        }
        prefix.push(as_header_string(last_value));
        return matches(&prefix);
    }

    prefix.push(as_header_string(last_value));
    matches(&prefix)
}

/// What a header value looks like as a sorting-setting string.
fn as_header_string(value: &CellValue) -> Option<String> {
    match value {
        CellValue::MeasureName { value, .. } => Some(value.clone()),
        CellValue::Value(Value::String(text)) => Some(text.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dto::{
        DataRow, PivotHeaderValue, PivotMeasureSorting, PivotMeasureSortingSettings, PivotSpecItem,
    };
    use crate::pivot::{make_pivot_legend, pivot, PivotTable};
    use crate::testing::{pivot_item, pivot_legend, pivot_spec};

    fn row(region: serde_json::Value, sales: serde_json::Value) -> DataRow {
        DataRow {
            data: vec![region, sales],
            legend: vec![0, 1],
        }
    }

    fn sorted_rows(
        structure: Vec<PivotSpecItem>,
        rows: Vec<DataRow>,
    ) -> Result<Vec<serde_json::Value>, PivotError> {
        let legend = pivot_legend();
        let pivot_legend = make_pivot_legend(&legend, &pivot_spec(structure))?;
        let dframe = pivot(&rows, &legend, &pivot_legend)?;
        let mut table = PivotTable::new(dframe, pivot_legend);
        table.sort(&legend)?;
        Ok(table
            .dframe
            .row_keys
            .iter()
            .map(|key| main_value(&key[0]).expect("a header cell").to_json())
            .collect())
    }

    fn row_item(direction: Direction) -> PivotSpecItem {
        let mut item = pivot_item(vec![0], PivotRole::Row);
        item.role_spec.direction = Some(direction);
        item
    }

    fn measure_with_sorting(direction: Direction) -> PivotSpecItem {
        let mut item = pivot_item(vec![1], PivotRole::Measure);
        item.role_spec.sorting = Some(PivotMeasureSorting {
            column: Some(PivotMeasureSortingSettings {
                header_values: vec![PivotHeaderValue {
                    value: "Sales".to_string(),
                }],
                direction,
                role_spec: Default::default(),
            }),
            row: None,
        });
        item
    }

    #[test]
    fn rows_are_sorted_by_dimension_ignoring_case() {
        let rows = vec![
            row(serde_json::json!("b"), serde_json::json!(1)),
            row(serde_json::json!("A"), serde_json::json!(2)),
            row(serde_json::json!("c"), serde_json::json!(3)),
        ];
        let sorted = sorted_rows(
            vec![
                row_item(Direction::Asc),
                pivot_item(vec![1], PivotRole::Measure),
            ],
            rows,
        )
        .unwrap();
        assert_eq!(
            sorted,
            vec![
                serde_json::json!("A"),
                serde_json::json!("b"),
                serde_json::json!("c"),
            ]
        );
    }

    #[test]
    fn descending_dimensions_reverse_the_order() {
        let rows = vec![
            row(serde_json::json!("a"), serde_json::json!(1)),
            row(serde_json::json!("b"), serde_json::json!(2)),
            row(serde_json::json!("c"), serde_json::json!(3)),
        ];
        let sorted = sorted_rows(
            vec![
                row_item(Direction::Desc),
                pivot_item(vec![1], PivotRole::Measure),
            ],
            rows,
        )
        .unwrap();
        assert_eq!(
            sorted,
            vec![
                serde_json::json!("c"),
                serde_json::json!("b"),
                serde_json::json!("a"),
            ]
        );
    }

    #[test]
    fn nulls_sort_first_in_both_directions() {
        let rows = vec![
            row(serde_json::json!("a"), serde_json::json!(1)),
            row(serde_json::Value::Null, serde_json::json!(2)),
        ];
        let ascending = sorted_rows(
            vec![
                row_item(Direction::Asc),
                pivot_item(vec![1], PivotRole::Measure),
            ],
            rows.clone(),
        )
        .unwrap();
        assert_eq!(ascending[0], serde_json::Value::Null);
        assert_eq!(ascending[1], serde_json::json!("a"));

        let descending = sorted_rows(
            vec![
                row_item(Direction::Desc),
                pivot_item(vec![1], PivotRole::Measure),
            ],
            rows,
        )
        .unwrap();
        // `invert(LeastNullValue)` puts the null last when the axis is descending.
        assert_eq!(descending[0], serde_json::json!("a"));
        assert_eq!(descending[1], serde_json::Value::Null);
    }

    #[test]
    fn a_measure_sorts_the_other_axis() {
        let rows = vec![
            row(serde_json::json!("Europe"), serde_json::json!(42)),
            row(serde_json::json!("Asia"), serde_json::json!(7)),
            row(serde_json::json!("Africa"), serde_json::json!(30)),
        ];
        let ascending = sorted_rows(
            vec![
                row_item(Direction::Asc),
                measure_with_sorting(Direction::Asc),
            ],
            rows.clone(),
        )
        .unwrap();
        assert_eq!(
            ascending,
            vec![
                serde_json::json!("Asia"),
                serde_json::json!("Africa"),
                serde_json::json!("Europe"),
            ]
        );

        let descending = sorted_rows(
            vec![
                row_item(Direction::Asc),
                measure_with_sorting(Direction::Desc),
            ],
            rows,
        )
        .unwrap();
        assert_eq!(
            descending,
            vec![
                serde_json::json!("Europe"),
                serde_json::json!("Africa"),
                serde_json::json!("Asia"),
            ]
        );
    }

    #[test]
    fn a_sorting_header_that_matches_nothing_is_an_error() {
        let mut item = pivot_item(vec![1], PivotRole::Measure);
        item.role_spec.sorting = Some(PivotMeasureSorting {
            column: Some(PivotMeasureSortingSettings {
                header_values: vec![PivotHeaderValue {
                    value: "Nope".to_string(),
                }],
                direction: Direction::Asc,
                role_spec: Default::default(),
            }),
            row: None,
        });
        let rows = vec![row(serde_json::json!("Europe"), serde_json::json!(42))];
        let error = sorted_rows(vec![row_item(Direction::Asc), item], rows).unwrap_err();
        assert_eq!(error, PivotError::SortingAxisNotFound);
    }

    #[test]
    fn sort_values_order_nulls_below_numbers_and_totals_above_them() {
        assert!(SortValue::TotalLeast < SortValue::Least);
        assert!(SortValue::Least < SortValue::Number(0.0));
        assert!(SortValue::Number(0.0) < SortValue::TotalGreatest);
        assert_eq!(
            SortValue::Number(2.0).cmp(&SortValue::Number(10.0)),
            Ordering::Less
        );
        assert_eq!(
            SortValue::Text("a".to_string()).cmp(&SortValue::Text("b".to_string())),
            Ordering::Less
        );
    }

    #[test]
    fn a_missing_cell_sorts_like_a_null() {
        let normalizer = ValueNormalizer::Dimension {
            legend_item_ids: HashSet::from([0]),
            converter: None,
            direction: Direction::Asc,
        };
        assert_eq!(normalizer.normalize(None), SortValue::Least);
    }

    #[test]
    fn a_template_value_goes_to_the_end_in_both_directions() {
        // A cell whose legend item is not part of the dimension: a totals label.
        let cell = crate::pivot::DataCell::new(
            crate::pivot::CellValue::Value(serde_json::json!("Total")),
            9,
            0,
        );
        let ascending = ValueNormalizer::Dimension {
            legend_item_ids: HashSet::from([0]),
            converter: None,
            direction: Direction::Asc,
        };
        assert_eq!(
            ascending.normalize(Some(&vec![cell.clone()])),
            SortValue::TotalGreatest
        );
        let descending = ValueNormalizer::Dimension {
            legend_item_ids: HashSet::from([0]),
            converter: None,
            direction: Direction::Desc,
        };
        assert_eq!(
            descending.normalize(Some(&vec![cell])),
            SortValue::TotalLeast
        );
    }
}
