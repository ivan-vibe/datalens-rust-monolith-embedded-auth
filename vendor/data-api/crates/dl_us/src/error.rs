//! Errors of the `us` client, mapped onto `dl_api_lib` error codes by the server.
//!
//! The Python service answers a missing dataset with `ERR.DS_API.DB.DATASET_NOT_FOUND` and a
//! rejected token with a `401`; everything else it does not classify ends up as a `5xx`
//! `ERR.DS_API.*`. The same split is kept here so client behaviour does not change.

/// What went wrong while talking to `us`.
#[derive(Debug, thiserror::Error)]
pub enum UsError {
    #[error("dataset {entry_id} does not exist")]
    NotFound { entry_id: String },

    #[error("us rejected the request (status {status})")]
    Unauthorized { status: u16 },

    #[error("us responded with status {status}: {body}")]
    Status { status: u16, body: String },

    #[error("us is not reachable: {0}")]
    Transport(String),

    #[error("entry {entry_id} is a {scope}, not a dataset")]
    NotADataset { entry_id: String, scope: String },

    #[error("entry {entry_id} is a {scope}, not a connection")]
    NotAConnection { entry_id: String, scope: String },

    #[error("dataset entry {entry_id} has no data")]
    EmptyEntry { entry_id: String },

    #[error("dataset {entry_id} is invalid: {reason}")]
    Invalid { entry_id: String, reason: String },

    #[error("cannot decode the us response for {entry_id}: {reason}")]
    Decode { entry_id: String, reason: String },
}

impl UsError {
    /// The `data-api` error code, as `dl_api_lib` spells it.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "ERR.DS_API.DB.DATASET_NOT_FOUND",
            Self::Unauthorized { .. } => "ERR.DS_API.REQUEST.AUTH",
            Self::Status { .. } | Self::Transport(_) => "ERR.DS_API.US.REQUEST",
            Self::NotADataset { .. } => "ERR.DS_API.DB.DATASET_NOT_FOUND",
            Self::NotAConnection { .. } => "ERR.DS_API.DB.CONNECTION_NOT_FOUND",
            Self::EmptyEntry { .. } => "ERR.DS_API.DATASET.INVALID",
            Self::Invalid { .. } => "ERR.DS_API.DATASET.INVALID",
            Self::Decode { .. } => "ERR.DS_API.US.RESPONSE",
        }
    }

    /// The HTTP status `data-api` answers with.
    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound { .. } | Self::NotADataset { .. } | Self::NotAConnection { .. } => 404,
            Self::Unauthorized { status } => *status,
            Self::Status { .. } | Self::Transport(_) => 502,
            Self::EmptyEntry { .. } | Self::Invalid { .. } => 400,
            Self::Decode { .. } => 502,
        }
    }
}
