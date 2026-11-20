//! `dl_api_lib.api_common.data_serialization.PivotDataRequestResponseSerializer`.

use super::transform::DataCellVector;
use super::{PivotError, PivotTable};
use crate::dto::{
    BlockInfo, DataExport, PivotCell, PivotCellVector, PivotData, PivotHeaderWithInfo,
    PivotResponse, PivotRole, PivotRow, PivotStructure,
};
use crate::legend::{Legend, DIMENSION_NAME_TITLE};

/// `DataExportForbiddenReason.prohibited_in_pivot`.
pub const PROHIBITED_IN_PIVOT: &str = "PROHIBITED_EXPORT_PIVOT_TABLE";

/// `enrich_resp_dict_with_data_export_info` after the pivot-specific patch.
///
/// A pivot can be exported as a file (`basic`), but never in the background: the table is
/// assembled in memory on every request, so the patch only denies `background` and appends
/// the reason to whatever the dataset already reports.
pub fn pivot_data_export(dataset_export: DataExport) -> DataExport {
    let mut background = dataset_export.background;
    background.allowed = false;
    let mut reason = background.reason.unwrap_or_default();
    reason.push(PROHIBITED_IN_PIVOT.to_string());
    background.reason = Some(reason);
    DataExport {
        background,
        basic: dataset_export.basic,
    }
}

/// `PivotDataRequestResponseSerializer.make_pivot_response`.
///
/// `blocks` is the whole query list of the request: the base block and, for a totals request,
/// one block per total (`merged_stream.meta.blocks`). The table itself is built from the
/// merged stream of all of them.
pub fn make_pivot_response(
    legend: &Legend,
    table: &PivotTable,
    blocks: &[BlockInfo],
    data_export: DataExport,
) -> Result<PivotResponse, PivotError> {
    let data_export = pivot_data_export(data_export);
    // The pivot response has no `data_export_forbidden` at all (v2 marks it `None`).
    let data_export_forbidden = None;
    let dframe = &table.dframe;

    // `columns`: the headers of the table, not its data — the data lives in `rows[].values`.
    let columns: Vec<Vec<PivotCellVector>> = dframe
        .column_keys
        .iter()
        .map(|column_key| header_cells(column_key))
        .collect();
    let columns_with_info: Vec<PivotHeaderWithInfo> = dframe
        .column_keys
        .iter()
        .map(|column_key| PivotHeaderWithInfo {
            cells: header_cells(column_key),
            header_info: dframe.info(column_key),
        })
        .collect();

    let row_dimension_headers: Vec<PivotCellVector> = row_dimension_headers(legend, table)?;

    let rows: Vec<PivotRow> = dframe
        .row_keys
        .iter()
        .map(|row_key| {
            let header: Vec<PivotCellVector> = header_cells(row_key);
            let values: Vec<PivotCellVector> = dframe
                .column_keys
                .iter()
                .map(|column_key| {
                    dframe
                        .get(row_key, column_key)
                        .map(|vector| Some(serialize_vector(vector)))
                        .unwrap_or_default()
                })
                .collect();
            PivotRow {
                header_with_info: PivotHeaderWithInfo {
                    cells: header.clone(),
                    header_info: dframe.info(row_key),
                },
                header,
                values,
            }
        })
        .collect();

    let mut response = PivotResponse {
        fields: legend.items(),
        pivot: PivotStructure {
            structure: table.pivot_legend.to_structure(),
        },
        pivot_data: PivotData {
            columns,
            columns_with_info,
            row_dimension_headers,
            rows,
        },
        blocks: blocks.to_vec(),
        notifications: Vec::new(),
        data_export,
        data_export_forbidden,
        json_len: 0,
    };
    // Same contract as `ResultResponse::json_len`: the service measures the body once so the
    // charts engine can report `sources[].size` without a second serialization.
    response.measure_json_len();
    Ok(response)
}

/// `_serialize_cell_vector`: `[value, legend_item_id, pivot_item_id]` per cell, `null` for a
/// hole in the table.
fn serialize_vector(vector: &DataCellVector) -> Vec<PivotCell> {
    vector
        .iter()
        .map(|cell| {
            (
                cell.value.to_json(),
                cell.legend_item_id,
                cell.pivot_item_id,
            )
        })
        .collect()
}

/// The cells of one row/column header: every dimension vector serialized.
fn header_cells(key: &[DataCellVector]) -> Vec<PivotCellVector> {
    key.iter()
        .map(|vector| Some(serialize_vector(vector)))
        .collect()
}

/// `TableDataFacade.iter_row_dim_headers`: one cell per `pivot_row` item, titled by it.
///
/// The cell points at the `Dimension Names` legend item and pivot item, which is why
/// `patch_pivot_legend` always adds them.
fn row_dimension_headers(
    legend: &Legend,
    table: &PivotTable,
) -> Result<Vec<PivotCellVector>, PivotError> {
    let dname_legend_item_id = *legend
        .dimension_name_ids()
        .first()
        .ok_or(PivotError::MissingPseudoDimension(DIMENSION_NAME_TITLE))?;
    let dname_pivot_item_id = *table
        .pivot_legend
        .dimension_name_piids()
        .first()
        .ok_or(PivotError::MissingPseudoDimension(DIMENSION_NAME_TITLE))?;

    Ok(table
        .pivot_legend
        .list_for_role(PivotRole::Row)
        .iter()
        .map(|item| {
            Some(vec![(
                serde_json::Value::String(item.title.clone()),
                dname_legend_item_id,
                dname_pivot_item_id,
            )])
        })
        .collect())
}
