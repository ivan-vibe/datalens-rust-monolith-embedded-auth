//! `data-api` request/response layer, ported from `dl_api_lib.schemas.data`
//! (v2 contract) and the handlers that serve it.
//!
//! The crate is transport-free: [`Service`] takes a decoded request, applies the dataset
//! `updates`, compiles the query, runs it through an [`Executor`] and renders the response
//! body. The HTTP server in `datalens-server` is a thin axum wrapper over it, so the same
//! code can be tested without a socket — which is what the tests below do.

pub mod dto;
pub mod error;
pub mod executor;
pub mod legend;
pub mod markup;
pub mod memo;
pub mod pivot;
pub mod postprocess;
pub mod preflight;
pub mod service;
pub mod testing;
pub mod trace;
pub mod updates;

pub use dto::{
    data_row_json_len, json_len, DashSqlRequest, DataChunk, DataExport, DataRow, DistinctRequest,
    ExportPermission, FieldsItem,
    FilterItem, LegendItem, OrderByItem, ParameterValueItem, PivotData, PivotRequest,
    PivotResponse, PreflightDataset, PreflightField, PreflightResponse, PreviewRequest,
    RangeRequest, RangeType, ResultRequest, ResultResponse, ResultSpec, RoleSpec,
    TypedQueryContent, TypedQueryRawParameters, TypedQueryRawRequest, TypedQueryRequest,
    UpdateAction, Visibility,
};
pub use error::ApiError;
pub use executor::{
    ConnectionResolver, ConnectionTarget, Executor, ExecutorError, QueryMetadata, RowStream,
};
pub use legend::{Legend, LegendEntry, LegendFormalizer};
pub use pivot::{make_pivot_legend, make_pivot_response, PivotError, PivotTable};
pub use preflight::{Preflight, VALIDATION_OK_CODE, VALIDATION_OK_MESSAGE};
pub use service::{
    PreparedQuery, PreparedResult, PreparedTotals, QueryCacheSettings, QueryCacheStats, QueryCell,
    Service,
};
pub use trace::{record_detail, record_wait, record_work, with_trace, StageKind, Summary, Trace};
pub use updates::{DeletedField, FieldPatch};
