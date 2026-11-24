//! Error types for OxidePM

use crate::error_code::ErrorCode;
use std::path::PathBuf;

/// OxidePM error type
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("App not found: {0}")]
    AppNotFound(String),

    #[error("App already exists: {0}")]
    AppAlreadyExists(String),

    #[error("Daemon not running")]
    DaemonNotRunning,

    #[error("Daemon already running")]
    DaemonAlreadyRunning,

    /// The daemon is shutting down and takes no new work.
    #[error("Daemon is shutting down")]
    ShuttingDown,

    /// A start that was still pending was cancelled by a stop or a delete.
    #[error("Cancelled: {0}")]
    Cancelled(String),

    /// An app's mailbox stayed full.
    #[error("Busy: {0}")]
    Busy(String),

    /// A bug, or a supervisor task died. Details are in the daemon log.
    #[error("Internal error: {0}")]
    Internal(String),

    /// `depends_on` names an app that is neither registered nor in the same batch.
    #[error("{0}")]
    DependencyMissing(String),

    /// `depends_on` forms a cycle; the message names it.
    #[error("{0}")]
    DependencyCycle(String),

    /// A dependency is stopped, errored or gone, so the dependent cannot start.
    #[error("{0}")]
    DependencyFailed(String),

    /// A dependency was not running within `depends_timeout_secs`.
    #[error("{0}")]
    DependencyTimeout(String),

    #[error("Build failed: {0}")]
    BuildFailed(String),

    #[error("Process failed to start: {0}")]
    ProcessStartFailed(String),

    #[error("Config error: {0}")]
    ConfigError(String),

    #[error("Config file not found: {0}")]
    ConfigNotFound(PathBuf),

    #[error("IPC error: {0}")]
    IpcError(String),

    #[error("IPC connection failed: {0}")]
    IpcConnectionFailed(String),

    #[error("Database error: {0}")]
    DbError(String),

    /// Stored state (database, saved file) was written by an incompatible
    /// OxidePM. The message names the file and the remedy.
    #[error("{0}")]
    StateIncompatible(String),

    /// `save` would replace a non-empty saved file with an empty list.
    #[error("{0}")]
    SaveWouldEmpty(String),

    /// The policy refused the request for this actor.
    #[error("{0}")]
    PolicyDenied(String),

    /// The app is protected and the actor is not a human.
    #[error("{0}")]
    Protected(String),

    /// A non-human actor asked too often.
    #[error("{0}")]
    RateLimited(String),

    /// The audit log could not record the decision, so a non-human actor's request was not run.
    #[error("{0}")]
    AuditUnavailable(String),

    /// A verb or command has no dry run.
    #[error("{0}")]
    DryRunUnsupported(String),

    /// The named adapter or runtime is not imported or not enabled; the message names the
    /// adapter that provides it and the command to import it.
    #[error("{0}")]
    AdapterMissing(String),

    /// An adapter is enabled but cannot take a call now.
    #[error("{0}")]
    AdapterUnavailable(String),

    /// A call to an adapter timed out.
    #[error("{0}")]
    AdapterTimeout(String),

    /// An adapter exited during a call.
    #[error("{0}")]
    AdapterCrashed(String),

    /// An adapter broke the protocol.
    #[error("{0}")]
    AdapterProtocol(String),

    /// An adapter's in-flight limit was reached.
    #[error("{0}")]
    AdapterBusy(String),

    /// An adapter's manifest, plan or name is invalid.
    #[error("{0}")]
    AdapterInvalid(String),

    /// An adapter's executable no longer matches the digest recorded at import.
    #[error("{0}")]
    AdapterTampered(String),

    /// A download does not match its published checksum.
    #[error("{0}")]
    ChecksumMismatch(String),

    /// A signature did not verify.
    #[error("{0}")]
    SignatureInvalid(String),

    /// The adapter index is expired or older than the last one seen.
    #[error("{0}")]
    IndexStale(String),

    #[error("Invalid selector: {0}")]
    InvalidSelector(String),

    #[error("Invalid spec: {0}")]
    InvalidSpec(String),

    /// A feature that is not available (removed, or not built yet).
    #[error("Unsupported: {0}")]
    Unsupported(String),

    #[error("Invalid mode: {0}")]
    InvalidMode(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),
}

/// Result type alias for OxidePM
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The wire code of this error (the one mapping; the daemon turns every failure into a
    /// response through it).
    pub fn code(&self) -> ErrorCode {
        match self {
            Error::AppNotFound(_) => ErrorCode::AppNotFound,
            Error::AppAlreadyExists(_) => ErrorCode::AppExists,
            Error::DependencyMissing(_) => ErrorCode::DependencyMissing,
            Error::DependencyCycle(_) => ErrorCode::DependencyCycle,
            Error::DependencyFailed(_) => ErrorCode::DependencyFailed,
            Error::DependencyTimeout(_) => ErrorCode::DependencyTimeout,
            Error::BuildFailed(_) => ErrorCode::BuildFailed,
            Error::ProcessStartFailed(_) => ErrorCode::SpawnFailed,
            Error::InvalidSelector(_) => ErrorCode::InvalidSelector,
            Error::InvalidSpec(msg)
                if ["app name ", "invalid app name ", "a cluster name "]
                    .iter()
                    .any(|prefix| msg.starts_with(prefix)) =>
            {
                ErrorCode::InvalidName
            }
            Error::InvalidSpec(_) | Error::InvalidMode(_) | Error::ConfigError(_) => {
                ErrorCode::InvalidSpec
            }
            Error::DbError(_) => ErrorCode::Db,
            Error::StateIncompatible(_) => ErrorCode::StateIncompatible,
            Error::SaveWouldEmpty(_) => ErrorCode::SaveWouldEmpty,
            Error::PolicyDenied(_) => ErrorCode::PolicyDenied,
            Error::Protected(_) => ErrorCode::Protected,
            Error::RateLimited(_) => ErrorCode::RateLimited,
            Error::AuditUnavailable(_) => ErrorCode::AuditUnavailable,
            Error::DryRunUnsupported(_) => ErrorCode::DryRunUnsupported,
            Error::AdapterMissing(_) => ErrorCode::AdapterMissing,
            Error::AdapterUnavailable(_) => ErrorCode::AdapterUnavailable,
            Error::AdapterTimeout(_) => ErrorCode::AdapterTimeout,
            Error::AdapterCrashed(_) => ErrorCode::AdapterCrashed,
            Error::AdapterProtocol(_) => ErrorCode::AdapterProtocol,
            Error::AdapterBusy(_) => ErrorCode::AdapterBusy,
            Error::AdapterInvalid(_) => ErrorCode::AdapterInvalid,
            Error::AdapterTampered(_) => ErrorCode::AdapterTampered,
            Error::ChecksumMismatch(_) => ErrorCode::ChecksumMismatch,
            Error::SignatureInvalid(_) => ErrorCode::SignatureInvalid,
            Error::IndexStale(_) => ErrorCode::IndexStale,
            Error::Unsupported(_) => ErrorCode::Unsupported,
            Error::Timeout(_) => ErrorCode::Timeout,
            Error::ShuttingDown => ErrorCode::ShuttingDown,
            Error::Cancelled(_) => ErrorCode::Cancelled,
            Error::Busy(_) => ErrorCode::Busy,
            Error::Internal(_) => ErrorCode::Internal,
            Error::IoError(_) => ErrorCode::Io,
            Error::DaemonNotRunning
            | Error::DaemonAlreadyRunning
            | Error::ConfigNotFound(_)
            | Error::IpcError(_)
            | Error::IpcConnectionFailed(_)
            | Error::JsonError(_) => ErrorCode::Internal,
        }
    }

    pub fn ipc<S: Into<String>>(msg: S) -> Self {
        Error::IpcError(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = Error::AppNotFound("myapp".to_string());
        assert_eq!(err.to_string(), "App not found: myapp");
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: Error = io_err.into();
        assert!(matches!(err, Error::IoError(_)));
    }
}
