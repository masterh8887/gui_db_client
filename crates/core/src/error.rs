//! Error type shared by every module. Serialised over IPC as `{ code, message, detail? }`.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CoreError {
    #[error("connection failed: {0}")]
    Connect(String),
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("{msg}")]
    Query { code: Option<String>, msg: String },
    #[error("i/o error: {0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("driver '{id}' is not available: {detail}")]
    DriverMissing { id: String, detail: String },
    #[error("server memory risk: {0}")]
    OomRisk(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("ssh tunnel: {0}")]
    Tunnel(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Internal(String),
}

impl CoreError {
    /// Stable machine-readable code. The UI branches on this, never on the message.
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::Connect(_) => "CONNECT",
            CoreError::Auth(_) => "AUTH",
            CoreError::Query { .. } => "QUERY",
            CoreError::Io(_) => "IO",
            CoreError::Cancelled => "CANCELLED",
            CoreError::Timeout(_) => "TIMEOUT",
            CoreError::DriverMissing { .. } => "DRIVER_MISSING",
            CoreError::OomRisk(_) => "OOM_RISK",
            CoreError::Invalid(_) => "INVALID",
            CoreError::Tunnel(_) => "TUNNEL",
            CoreError::NotFound(_) => "NOT_FOUND",
            CoreError::Internal(_) => "INTERNAL",
        }
    }

    pub fn query(msg: impl Into<String>) -> Self {
        CoreError::Query { code: None, msg: msg.into() }
    }

    pub fn query_with_code(code: impl Into<String>, msg: impl Into<String>) -> Self {
        CoreError::Query { code: Some(code.into()), msg: msg.into() }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        CoreError::Internal(msg.into())
    }

    pub fn to_payload(&self) -> ErrorPayload {
        let detail = match self {
            CoreError::Query { code: Some(c), .. } => Some(format!("SQLSTATE {c}")),
            CoreError::DriverMissing { detail, .. } => Some(detail.clone()),
            _ => None,
        };
        ErrorPayload { code: self.code().to_string(), message: self.to_string(), detail }
    }
}

/// Wire form of an error.
#[derive(Debug, Clone, Serialize, Deserialize, TS, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
    #[ts(optional)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::Internal(format!("json: {e}"))
    }
}

impl From<toml::de::Error> for CoreError {
    fn from(e: toml::de::Error) -> Self {
        CoreError::Internal(format!("toml: {e}"))
    }
}

impl From<toml::ser::Error> for CoreError {
    fn from(e: toml::ser::Error) -> Self {
        CoreError::Internal(format!("toml: {e}"))
    }
}

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::SqliteFailure(ref f, ref msg) => {
                if f.code == rusqlite::ErrorCode::OperationInterrupted {
                    CoreError::Cancelled
                } else {
                    CoreError::Query { code: Some(format!("{:?}", f.code)), msg: msg.clone().unwrap_or_else(|| e.to_string()) }
                }
            }
            other => CoreError::query(other.to_string()),
        }
    }
}

impl From<arrow::error::ArrowError> for CoreError {
    fn from(e: arrow::error::ArrowError) -> Self {
        CoreError::Internal(format!("arrow: {e}"))
    }
}

impl From<parquet::errors::ParquetError> for CoreError {
    fn from(e: parquet::errors::ParquetError) -> Self {
        CoreError::Io(format!("parquet: {e}"))
    }
}

impl From<csv::Error> for CoreError {
    fn from(e: csv::Error) -> Self {
        CoreError::Io(format!("csv: {e}"))
    }
}

impl From<tokio::task::JoinError> for CoreError {
    fn from(e: tokio::task::JoinError) -> Self {
        if e.is_cancelled() {
            CoreError::Cancelled
        } else {
            CoreError::Internal(format!("task panicked: {e}"))
        }
    }
}
