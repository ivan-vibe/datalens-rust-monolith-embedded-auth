//! Core DataLens types, mirrored from `dl_core` / `dl_constants`.
//!
//! This crate is the Rust replacement for the Python data model: it owns the dataset
//! object, its fields, the user-facing data types and the validation rules that
//! `DatasetValidator` applies before a query is built. Nothing here knows about SQL or
//! HTTP: `dl_query` and `dl_api` build on top of it.

pub mod dataset;
pub mod dtype;
pub mod error;
pub mod rls;
pub mod validation;

pub use dataset::{
    Aggregation, AvatarRelation, CalcMode, Dataset, Field, FieldRef, FieldType, JoinCondition,
    JoinConditionPart, JoinType, PseudoField, RawSqlLevel, Source, SourceColumn,
};
pub use dtype::{DataType, UserDataType};
pub use error::CoreError;
pub use rls::{Rls, RlsEntry, RlsPattern, RlsSubject, RlsSubjectType};
pub use validation::{
    ComponentError, ComponentErrorLevel, ComponentErrorPack, ComponentErrorRegistry, ComponentType,
    DatasetIssue,
};
