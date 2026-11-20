//! Fixtures shared by the tests and by the `demo` mode of the server binary.

use dl_core::{Aggregation, CalcMode, Dataset, Field, FieldType, Source, UserDataType};
use dl_query::Role;

use crate::dto::{LegendItem, PivotRole, PivotRoleSpec, PivotSpec, PivotSpecItem, RoleSpec};
use crate::legend::{
    Legend, LegendEntry, DIMENSION_NAME_ID, DIMENSION_NAME_TITLE, ITEM_TYPE_DIMENSION_NAME,
    ITEM_TYPE_FIELD, ITEM_TYPE_MEASURE_NAME, MEASURE_NAME_ID, MEASURE_NAME_TITLE,
};

fn field(guid: &str, title: &str, dtype: UserDataType, aggregation: Aggregation) -> Field {
    Field {
        guid: guid.to_string(),
        title: title.to_string(),
        initial_data_type: Some(dtype),
        data_type: Some(dtype),
        aggregation,
        ..Field::default()
    }
}

/// A tiny dataset over `public.sales` with one dimension, one measure and one formula.
///
/// Columns: `Region` (string), `Sales` (integer), `Year` (integer), `Calc` (`Sales * 2`).
pub fn demo_dataset() -> Dataset {
    let mut calc = field("f_calc", "Calc", UserDataType::Float, Aggregation::None);
    calc.calc_mode = CalcMode::Formula;
    calc.formula = "[Sales] * 2".to_string();
    calc.guid_formula = "[f_sales] * 2".to_string();
    Dataset {
        data_export_forbidden: false,
        connection_id: None,
        sources: vec![Source::table("src", "public.sales")],
        fields: vec![
            field(
                "f_region",
                "Region",
                UserDataType::String,
                Aggregation::None,
            ),
            field("f_sales", "Sales", UserDataType::Integer, Aggregation::Sum),
            field("f_year", "Year", UserDataType::Integer, Aggregation::None),
            calc,
        ],
        ..Dataset::default()
    }
}

/// One legend item of [`pivot_legend`].
fn legend_item(
    legend_item_id: i64,
    id: &str,
    title: &str,
    field_type: FieldType,
    data_type: UserDataType,
    item_type: &str,
    role: Role,
) -> LegendItem {
    LegendItem {
        legend_item_id,
        id: id.to_string(),
        title: title.to_string(),
        data_type,
        field_type,
        item_type: item_type.to_string(),
        role_spec: RoleSpec::new(role),
    }
}

/// The legend a pivot test starts from: `Region` (dimension, liid 0), `Sales` (measure,
/// liid 1) and the two pseudo-dimensions (liids 2 and 3) that `PivotLegendFormalizer` adds.
///
/// The ids follow `LegendSchema`'s order for the `/pivot` query, so they line up with the
/// `legend` arrays of the rows an executor returns.
pub fn pivot_legend() -> Legend {
    Legend {
        entries: vec![
            LegendEntry {
                item: legend_item(
                    0,
                    "f_region",
                    "Region",
                    FieldType::Dimension,
                    UserDataType::String,
                    ITEM_TYPE_FIELD,
                    Role::Row,
                ),
                in_result_set: true,
                select_index: Some(0),
                block_id: None,
            },
            LegendEntry {
                item: legend_item(
                    1,
                    "f_sales",
                    "Sales",
                    FieldType::Measure,
                    UserDataType::Integer,
                    ITEM_TYPE_FIELD,
                    Role::Measure,
                ),
                in_result_set: true,
                select_index: Some(1),
                block_id: None,
            },
            LegendEntry {
                item: legend_item(
                    2,
                    MEASURE_NAME_ID,
                    MEASURE_NAME_TITLE,
                    FieldType::Dimension,
                    UserDataType::String,
                    ITEM_TYPE_MEASURE_NAME,
                    Role::Info,
                ),
                in_result_set: false,
                select_index: None,
                block_id: None,
            },
            LegendEntry {
                item: legend_item(
                    3,
                    DIMENSION_NAME_ID,
                    DIMENSION_NAME_TITLE,
                    FieldType::Dimension,
                    UserDataType::String,
                    ITEM_TYPE_DIMENSION_NAME,
                    Role::Info,
                ),
                in_result_set: false,
                select_index: None,
                block_id: None,
            },
        ],
    }
}

/// One `pivot.structure` item, referring to legend ids.
pub fn pivot_item(legend_item_ids: Vec<i64>, role: PivotRole) -> PivotSpecItem {
    PivotSpecItem {
        pivot_item_id: None,
        legend_item_ids,
        role_spec: PivotRoleSpec::new(role),
        title: None,
    }
}

/// A pivot spec with the given structure, no totals and no pagination.
pub fn pivot_spec(structure: Vec<PivotSpecItem>) -> PivotSpec {
    PivotSpec {
        structure,
        ..PivotSpec::default()
    }
}
