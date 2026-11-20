//! Errors raised while validating a dataset or resolving a request against it.

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CoreError {
    #[error("unknown data type: {0}")]
    UnknownDataType(String),
    #[error("unknown user data type: {0}")]
    UnknownUserDataType(String),
    #[error("dataset has no fields")]
    EmptyDataset,
    #[error("field {guid:?} has an empty guid")]
    EmptyGuid { guid: String },
    #[error("duplicate field guid: {0}")]
    DuplicateGuid(String),
    #[error("duplicate field title: {0}")]
    DuplicateTitle(String),
    #[error("field {guid:?} has no data type")]
    MissingDataType { guid: String },
    #[error("formula field {guid:?} has an empty formula")]
    MissingFormula { guid: String },
    #[error("field {guid:?} refers to unknown source {source_id:?}")]
    UnknownSource { guid: String, source_id: String },
    #[error("direct field {guid:?} cannot be rendered: {reason}")]
    UnrenderableField { guid: String, reason: String },
    /// `DataSource.raw_schema` has no such column: `DataSourceConnection` resolves a `direct`
    /// field through the schema, so a field pointing at a missing column is a broken field.
    #[error("direct field {guid:?} reads unknown column {column:?} of source {source_id:?}")]
    UnknownColumn {
        guid: String,
        column: String,
        source_id: String,
    },
}
