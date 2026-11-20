//! `/result-preflight`: the dataset, validated, without running a query.
//!
//! Python's `DatasetResultPreflightView` does not compile SQL either. It applies the request
//! `updates`, runs `DatasetValidator` over the dataset and answers with the `component_errors`
//! registry plus the field list, so the UI can highlight every broken field at once.

use dl_core::validation::dataset_issues;
use dl_core::{ComponentError, ComponentErrorRegistry, ComponentType, Dataset, UserDataType};
use dl_query::Dialect;

use crate::dto::{PreflightDataset, PreflightField, PreflightResponse};
use crate::error::ApiError;

/// `VALIDATION_OK_MESSAGE`.
pub const VALIDATION_OK_CODE: &str = "OK";
pub const VALIDATION_OK_MESSAGE: &str = "Validation was successful";
/// `exc.DLValidationError`: `ERR.DS_API.VALIDATION.ERROR`.
pub const VALIDATION_ERROR_CODE: &str = "ERR.DS_API.VALIDATION.ERROR";
pub const VALIDATION_ERROR_MESSAGE: &str = "Validation finished with errors.";
/// `exc.DLValidationFatalError`: the dataset could not even be prepared.
pub const VALIDATION_FATAL_CODE: &str = "ERR.DS_API.VALIDATION.FATAL";
pub const VALIDATION_FATAL_MESSAGE: &str = "Validation encountered a fatal error.";

/// A whole `/result-preflight` answer: the body and the status it goes out with.
#[derive(Debug, Clone, PartialEq)]
pub struct Preflight {
    pub status: u16,
    pub body: PreflightResponse,
}

impl Preflight {
    /// `_make_response` for the early return: an error code, no fields, no packs.
    pub fn fatal(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: 400,
            body: PreflightResponse {
                code: code.into(),
                message: message.into(),
                dataset: PreflightDataset::default(),
            },
        }
    }

    /// An error raised before validation could start (a refused `updates` action).
    pub fn from_error(error: &ApiError) -> Self {
        Self {
            status: error.status,
            body: PreflightResponse {
                code: error.code.clone(),
                message: error.message.clone(),
                dataset: PreflightDataset::default(),
            },
        }
    }

    /// `status=HTTPStatus.BAD_REQUEST if any_errors else HTTPStatus.OK`.
    pub fn is_ok(&self) -> bool {
        self.body.code == VALIDATION_OK_CODE
    }
}

/// `DatasetValidator` + `_prefixed_registry_copy` + `DLValidationError`.
///
/// Structure first (guids, titles, types, sources), then the formula pass, so a field that is
/// both mistyped and formula-broken reports the type problem only — it has no types to compile
/// its formula against either.
pub fn validate(dataset: &Dataset, dialect: Dialect) -> Preflight {
    let mut registry = ComponentErrorRegistry::default();
    for issue in dataset_issues(dataset) {
        let Some(guid) = issue.field else {
            // A dataset-level problem (`empty dataset`): the request cannot be served at all.
            return Preflight::fatal(VALIDATION_FATAL_CODE, VALIDATION_FATAL_MESSAGE);
        };
        registry.add(
            guid,
            ComponentType::Field,
            ComponentError::error(component_code(&issue.error), issue.error.to_string()),
        );
    }
    for (guid, error) in dl_query::check_formulas(dataset, dialect) {
        let api = ApiError::from(error);
        registry.add(
            guid,
            ComponentType::Field,
            ComponentError::error(api.code, api.message),
        );
    }

    let (code, message, status) = if registry.is_empty() {
        (VALIDATION_OK_CODE, VALIDATION_OK_MESSAGE, 200)
    } else {
        (VALIDATION_ERROR_CODE, VALIDATION_ERROR_MESSAGE, 400)
    };
    Preflight {
        status,
        body: PreflightResponse {
            code: code.to_string(),
            message: message.to_string(),
            dataset: PreflightDataset {
                component_errors: registry,
                fields: Some(fields_of(dataset)),
            },
        },
    }
}

/// `get_fields_data_raw(dataset, for_result=False)`, narrowed by the preflight schema's
/// `only=("title", "guid", "data_type", "calc_mode", "type")`.
pub fn fields_of(dataset: &Dataset) -> Vec<PreflightField> {
    dataset
        .fields
        .iter()
        .map(|field| PreflightField {
            title: field.title.clone(),
            guid: field.guid.clone(),
            data_type: field.dtype().ok().map(UserDataType::from_data_type),
            calc_mode: field.calc_mode,
            kind: dataset.field_type(field),
        })
        .collect()
}

/// The API code a dataset error carries inside a component pack.
///
/// `dl_core` names the fields with its own `DLBaseError` subclasses (`FieldTitleConflictError`
/// -> `FIELD.TITLE.CONFLICT`); this is the subset the port validates.
pub fn component_code(error: &dl_core::CoreError) -> &'static str {
    use dl_core::CoreError;

    match error {
        CoreError::DuplicateTitle(_) => "ERR.DS_API.FIELD.TITLE.CONFLICT",
        CoreError::EmptyGuid { .. }
        | CoreError::DuplicateGuid(_)
        | CoreError::MissingDataType { .. }
        | CoreError::MissingFormula { .. }
        | CoreError::UnknownSource { .. }
        | CoreError::UnknownColumn { .. }
        | CoreError::UnrenderableField { .. }
        | CoreError::UnknownDataType(_)
        | CoreError::UnknownUserDataType(_) => "ERR.DS_API.FIELD.INVALID",
        // `EmptyDataset` is answered as a fatal error before any pack is built.
        CoreError::EmptyDataset => VALIDATION_FATAL_CODE,
    }
}
