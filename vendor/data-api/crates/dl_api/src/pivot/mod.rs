//! Pivot tables: `dl_pivot` + `dl_api_lib/app/data_api/resources/dataset/pivot.py`.
//!
//! A `/pivot` request runs the same query a `/result` request would (one block, one merged
//! stream) and then turns the *rows* into a table:
//!
//! 1. [`make_pivot_legend`] turns the request `pivot.structure` into a [`PivotLegend`], i.e.
//!    into `pivot_item_id`s, appending the `Measure Names`/`Dimension Names` items the table
//!    needs (`PivotFormalizerBase.patch_pivot_legend`);
//! 2. [`transform::pivot`] turns every row into `DataCell`s (one per `pivot_item_id` that
//!    references the cell's `legend_item_id`), transposes the measures into a pseudo-dimension
//!    and keys the values by `(row dimension values, column dimension values)`;
//! 3. [`PivotTable::sort`] orders both axes — dimensions first, then by a measure if the
//!    request asked for it (`NativePivotSorter`);
//! 4. [`PivotTable::paginate`] crops the rows of the finished table (`NativePivotPaginator`);
//! 5. [`make_pivot_response`] renders `pivot` + `pivot_data` for the client.
//!
//! Total rows (`with_totals`/`totals`) are the multi-block path: [`totals::normalize`] expands
//! the request into the base block plus one block per total, each with its own `template`/
//! `total` legend items, and the blocks are merged into one stream of rows before the table is
//! built — exactly the way `dl_api_lib` + `dl_query_processing.merging` do it.

mod legend;
mod serialize;
mod sort;
mod totals;
mod transform;

pub use legend::{make_pivot_legend, PivotItemType, PivotLegend, PivotLegendItem};
pub use serialize::make_pivot_response;
pub use totals::{normalize as normalize_totals, TotalBlock, TotalCell, TotalsPlan};
pub use transform::{pivot, CellValue, DataCell, DataCellVector, NativePivotDataFrame};

use crate::error::ApiError;
use crate::legend::Legend;

/// What can go wrong while building a pivot table.
///
/// The variants mirror the `dl_query_processing.exc` exceptions one for one, so a client that
/// branches on `code` keeps working: Python answers all of them with `400`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PivotError {
    /// `PivotItemsIncompatibleError`.
    #[error("pivot items have incompatible types in the same {role} role: {detail}")]
    ItemsIncompatible { role: &'static str, detail: String },
    /// `PivotInvalidRoleLegendError`.
    #[error("{0}")]
    InvalidRole(String),
    /// `PivotMeasureNameRequiredError`.
    #[error("`Measure Names` must be used when there is more than one measure")]
    MeasureNameRequired,
    /// `PivotMeasureNameForbiddenError`.
    #[error("`Measure Names` cannot be used when there are no measures")]
    MeasureNameForbidden,
    /// `PivotMeasureNameDuplicateError`.
    #[error("`Measure Names` can only be used once when there are multiple measures")]
    MeasureNameDuplicate,
    /// `PivotSortingMultipleColumnsOrRowsError`.
    #[error("only one measure can sort by columns and one by rows")]
    SortingMultipleAxes,
    /// `PivotSortingAgainstMultipleMeasuresError`.
    #[error("a measure cannot sort across the `Measure Names` axis")]
    SortingAgainstMeasures,
    /// `PivotSortingRowOrColumnNotFoundError`.
    #[error("the row/column to sort by was not found")]
    SortingAxisNotFound,
    /// `PivotSortingRowOrColumnIsAmbiguousError`.
    #[error("the sorting settings match more than one row/column")]
    SortingAxisAmbiguous,
    /// `PivotSortingWithSubtotalsIsNotAllowedError`.
    #[error("sorting is not supported when a row/column has subtotals")]
    SortingWithSubtotals,
    /// A measure of the pivot legend whose cell the query did not return, i.e. a measure
    /// that is in the pivot structure but not in the legend `SELECT` list.
    #[error("measure {0} has no value in the data")]
    MeasureWithoutCell(i64),
    /// `PivotUnevenDataColumnsError`.
    #[error("expected pivot items {expected:?}; got: {actual:?}")]
    UnevenDataColumns {
        expected: Vec<i64>,
        actual: Vec<i64>,
    },
    /// `PivotLegendItemReferenceError` / `LegendItemReferenceError`.
    #[error("unknown pivot/legend item: {0}")]
    UnknownItem(i64),
    /// A legend without the pseudo-dimensions `patch_pivot_legend` guarantees.
    #[error("the legend has no {0} item")]
    MissingPseudoDimension(&'static str),
    /// A `PivotAnnotationRoleSpec` that cannot be resolved against the measures.
    #[error("annotation item {0} has no measure to attach to")]
    AnnotationWithoutMeasure(i64),
}

impl From<PivotError> for ApiError {
    /// The codes are the `dl_query_processing.exc` hierarchy under `ERR.DS_API`, e.g.
    /// `PIVOT.SORTING.ROW_OR_COLUMN_NOT_FOUND` -> `ERR.DS_API.PIVOT.SORTING.ROW_OR_COLUMN_NOT_FOUND`.
    fn from(error: PivotError) -> Self {
        let code = match &error {
            PivotError::ItemsIncompatible { .. } => "ERR.DS_API.PIVOT.LEGEND.ITEMS_INCOMPATIBLE",
            PivotError::InvalidRole(_) => "ERR.DS_API.PIVOT.LEGEND.INVALID_ROLE",
            PivotError::MeasureNameRequired => "ERR.DS_API.PIVOT.MEASURE_NAME.REQUIRED",
            PivotError::MeasureNameForbidden => "ERR.DS_API.PIVOT.MEASURE_NAME.FORBIDDEN",
            PivotError::MeasureNameDuplicate => "ERR.DS_API.PIVOT.MEASURE_NAME.DUPLICATE",
            PivotError::SortingMultipleAxes => "ERR.DS_API.PIVOT.SORTING.MULTIPLE_COLUMNS_OR_ROWS",
            PivotError::SortingAgainstMeasures => {
                "ERR.DS_API.PIVOT.SORTING.AGAINST_MULTIPLE_MEASURES"
            }
            PivotError::SortingAxisNotFound => "ERR.DS_API.PIVOT.SORTING.ROW_OR_COLUMN_NOT_FOUND",
            PivotError::SortingAxisAmbiguous => {
                "ERR.DS_API.PIVOT.SORTING.ROW_OR_COLUMN_IS_AMBIGUOUS"
            }
            PivotError::SortingWithSubtotals => {
                "ERR.DS_API.PIVOT.SORTING.SUBTOTALS_ARE_NOT_ALLOWED"
            }
            // `PivotUnevenDataColumnsError`, plus the measure cell it cannot find: in Python
            // that is a `KeyError` (a 500), which is never a useful answer to a client.
            PivotError::UnevenDataColumns { .. } | PivotError::MeasureWithoutCell(_) => {
                "ERR.DS_API.PIVOT.UNEVEN_DATA_COLUMNS"
            }
            PivotError::UnknownItem(_) => "ERR.DS_API.PIVOT.LEGEND.ITEM_REFERENCE",
            PivotError::MissingPseudoDimension(_) | PivotError::AnnotationWithoutMeasure(_) => {
                return ApiError::internal(error.to_string())
            }
        };
        ApiError::bad_request(code, error.to_string())
    }
}

/// A pivot table after [`transform::pivot`]: the data frame plus its pivot legend.
///
/// `TableDataFacade` in Python keeps the legend, the sorter and the paginator alive; here the
/// two operations are plain methods, because the sorter needs only borrows.
#[derive(Debug, Clone, PartialEq)]
pub struct PivotTable {
    pub dframe: NativePivotDataFrame,
    pub pivot_legend: PivotLegend,
}

impl PivotTable {
    pub fn new(dframe: NativePivotDataFrame, pivot_legend: PivotLegend) -> Self {
        Self {
            dframe,
            pivot_legend,
        }
    }

    /// `TableDataFacade.sort`: dimension order first, then the measure sorts.
    pub fn sort(&mut self, legend: &Legend) -> Result<(), PivotError> {
        sort::sort(&mut self.dframe, legend, &self.pivot_legend)
    }

    /// `TableDataFacade.paginate`: only rows are cropped, the columns always stay.
    pub fn paginate(&mut self, limit_rows: Option<i64>, offset_rows: Option<i64>) {
        transform::paginate(&mut self.dframe, limit_rows, offset_rows);
    }

    pub fn get_column_count(&self) -> usize {
        self.dframe.column_keys.len()
    }

    pub fn get_row_count(&self) -> usize {
        self.dframe.row_keys.len()
    }
}
