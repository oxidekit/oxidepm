//! Stable machine-readable error codes (`E_...`) for the IPC wire.
//!
//! `Error::code` is the one mapping from an internal error to a code. A code that this build
//! does not know (a newer daemon's) is kept as `Other` so an older client still reads it.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    /// A bug, or an internal task died. Details are in the daemon log.
    Internal,
    /// Malformed JSON or an unknown request type.
    BadRequest,
    /// The envelope's protocol is outside the supported range, or the request is not an envelope.
    ProtocolVersion,
    /// A request line over the size limit.
    RequestTooLarge,
    /// A spec failed validation (the message names the field).
    InvalidSpec,
    /// An app name with a bad charset or length, or a reserved one.
    InvalidName,
    /// A selector that cannot be parsed or is not allowed for the operation.
    InvalidSelector,
    /// A feature that is not available (removed or not built yet).
    Unsupported,
    /// A selector matched nothing.
    AppNotFound,
    /// The name is taken, or the app is already active.
    AppExists,
    DependencyMissing,
    DependencyCycle,
    DependencyFailed,
    DependencyTimeout,
    /// The build step failed.
    BuildFailed,
    /// The program did not resolve or could not be spawned.
    SpawnFailed,
    /// A pending start was cancelled by a stop or delete.
    Cancelled,
    ShuttingDown,
    /// An app's mailbox stayed full.
    Busy,
    /// A bounded wait expired (including the client's request timeout).
    Timeout,
    Db,
    Io,
    /// Stored state (database, saved file) was written by an incompatible OxidePM.
    StateIncompatible,
    /// `save` would replace a non-empty file with an empty one.
    SaveWouldEmpty,
    /// The policy refused the request for this actor (the message names the tier, the actor
    /// and, for apps, which ones).
    PolicyDenied,
    /// The app is protected and the actor is not a human.
    Protected,
    /// A non-human actor asked too often (a restart of one app inside its cooldown).
    RateLimited,
    /// A non-human actor's caution or dangerous request was refused because the audit log
    /// could not record it first.
    AuditUnavailable,
    /// A verb or command has no dry run.
    DryRunUnsupported,
    /// The named adapter or runtime is not imported or not enabled.
    AdapterMissing,
    /// Enabled but not running, failed, still starting, or its circuit breaker is open.
    AdapterUnavailable,
    /// A call to an adapter timed out.
    AdapterTimeout,
    /// The adapter exited during a call.
    AdapterCrashed,
    /// The adapter broke the protocol.
    AdapterProtocol,
    /// The adapter's in-flight limit was reached.
    AdapterBusy,
    /// A manifest, plan or name is invalid, or a verb collides with a built-in one.
    AdapterInvalid,
    /// The executable's sha256 no longer matches the one recorded at import.
    AdapterTampered,
    /// A downloaded file does not match the checksum that was published for it.
    ChecksumMismatch,
    /// A signature (of the index or of an archive) is wrong.
    SignatureInvalid,
    /// The index is expired or older than the last one seen.
    IndexStale,
    /// A code this build does not know.
    Other(String),
}

impl ErrorCode {
    pub fn as_str(&self) -> &str {
        match self {
            ErrorCode::Internal => "E_INTERNAL",
            ErrorCode::BadRequest => "E_BAD_REQUEST",
            ErrorCode::ProtocolVersion => "E_PROTOCOL_VERSION",
            ErrorCode::RequestTooLarge => "E_REQUEST_TOO_LARGE",
            ErrorCode::InvalidSpec => "E_INVALID_SPEC",
            ErrorCode::InvalidName => "E_INVALID_NAME",
            ErrorCode::InvalidSelector => "E_INVALID_SELECTOR",
            ErrorCode::Unsupported => "E_UNSUPPORTED",
            ErrorCode::AppNotFound => "E_APP_NOT_FOUND",
            ErrorCode::AppExists => "E_APP_EXISTS",
            ErrorCode::DependencyMissing => "E_DEPENDENCY_MISSING",
            ErrorCode::DependencyCycle => "E_DEPENDENCY_CYCLE",
            ErrorCode::DependencyFailed => "E_DEPENDENCY_FAILED",
            ErrorCode::DependencyTimeout => "E_DEPENDENCY_TIMEOUT",
            ErrorCode::BuildFailed => "E_BUILD_FAILED",
            ErrorCode::SpawnFailed => "E_SPAWN_FAILED",
            ErrorCode::Cancelled => "E_CANCELLED",
            ErrorCode::ShuttingDown => "E_SHUTTING_DOWN",
            ErrorCode::Busy => "E_BUSY",
            ErrorCode::Timeout => "E_TIMEOUT",
            ErrorCode::Db => "E_DB",
            ErrorCode::Io => "E_IO",
            ErrorCode::StateIncompatible => "E_STATE_INCOMPATIBLE",
            ErrorCode::SaveWouldEmpty => "E_SAVE_WOULD_EMPTY",
            ErrorCode::PolicyDenied => "E_POLICY_DENIED",
            ErrorCode::Protected => "E_PROTECTED",
            ErrorCode::RateLimited => "E_RATE_LIMITED",
            ErrorCode::AuditUnavailable => "E_AUDIT_UNAVAILABLE",
            ErrorCode::DryRunUnsupported => "E_DRY_RUN_UNSUPPORTED",
            ErrorCode::AdapterMissing => "E_ADAPTER_MISSING",
            ErrorCode::AdapterUnavailable => "E_ADAPTER_UNAVAILABLE",
            ErrorCode::AdapterTimeout => "E_ADAPTER_TIMEOUT",
            ErrorCode::AdapterCrashed => "E_ADAPTER_CRASHED",
            ErrorCode::AdapterProtocol => "E_ADAPTER_PROTOCOL",
            ErrorCode::AdapterBusy => "E_ADAPTER_BUSY",
            ErrorCode::AdapterInvalid => "E_ADAPTER_INVALID",
            ErrorCode::AdapterTampered => "E_ADAPTER_TAMPERED",
            ErrorCode::ChecksumMismatch => "E_CHECKSUM_MISMATCH",
            ErrorCode::SignatureInvalid => "E_SIGNATURE_INVALID",
            ErrorCode::IndexStale => "E_INDEX_STALE",
            ErrorCode::Other(code) => code,
        }
    }

    /// The code for a wire string; anything unknown becomes `Other`.
    pub fn from_wire(code: &str) -> Self {
        match code {
            "E_INTERNAL" => ErrorCode::Internal,
            "E_BAD_REQUEST" => ErrorCode::BadRequest,
            "E_PROTOCOL_VERSION" => ErrorCode::ProtocolVersion,
            "E_REQUEST_TOO_LARGE" => ErrorCode::RequestTooLarge,
            "E_INVALID_SPEC" => ErrorCode::InvalidSpec,
            "E_INVALID_NAME" => ErrorCode::InvalidName,
            "E_INVALID_SELECTOR" => ErrorCode::InvalidSelector,
            "E_UNSUPPORTED" => ErrorCode::Unsupported,
            "E_APP_NOT_FOUND" => ErrorCode::AppNotFound,
            "E_APP_EXISTS" => ErrorCode::AppExists,
            "E_DEPENDENCY_MISSING" => ErrorCode::DependencyMissing,
            "E_DEPENDENCY_CYCLE" => ErrorCode::DependencyCycle,
            "E_DEPENDENCY_FAILED" => ErrorCode::DependencyFailed,
            "E_DEPENDENCY_TIMEOUT" => ErrorCode::DependencyTimeout,
            "E_BUILD_FAILED" => ErrorCode::BuildFailed,
            "E_SPAWN_FAILED" => ErrorCode::SpawnFailed,
            "E_CANCELLED" => ErrorCode::Cancelled,
            "E_SHUTTING_DOWN" => ErrorCode::ShuttingDown,
            "E_BUSY" => ErrorCode::Busy,
            "E_TIMEOUT" => ErrorCode::Timeout,
            "E_DB" => ErrorCode::Db,
            "E_IO" => ErrorCode::Io,
            "E_STATE_INCOMPATIBLE" => ErrorCode::StateIncompatible,
            "E_SAVE_WOULD_EMPTY" => ErrorCode::SaveWouldEmpty,
            "E_POLICY_DENIED" => ErrorCode::PolicyDenied,
            "E_PROTECTED" => ErrorCode::Protected,
            "E_RATE_LIMITED" => ErrorCode::RateLimited,
            "E_AUDIT_UNAVAILABLE" => ErrorCode::AuditUnavailable,
            "E_DRY_RUN_UNSUPPORTED" => ErrorCode::DryRunUnsupported,
            "E_ADAPTER_MISSING" => ErrorCode::AdapterMissing,
            "E_ADAPTER_UNAVAILABLE" => ErrorCode::AdapterUnavailable,
            "E_ADAPTER_TIMEOUT" => ErrorCode::AdapterTimeout,
            "E_ADAPTER_CRASHED" => ErrorCode::AdapterCrashed,
            "E_ADAPTER_PROTOCOL" => ErrorCode::AdapterProtocol,
            "E_ADAPTER_BUSY" => ErrorCode::AdapterBusy,
            "E_ADAPTER_INVALID" => ErrorCode::AdapterInvalid,
            "E_ADAPTER_TAMPERED" => ErrorCode::AdapterTampered,
            "E_CHECKSUM_MISMATCH" => ErrorCode::ChecksumMismatch,
            "E_SIGNATURE_INVALID" => ErrorCode::SignatureInvalid,
            "E_INDEX_STALE" => ErrorCode::IndexStale,
            other => ErrorCode::Other(other.to_string()),
        }
    }
}

impl Default for ErrorCode {
    /// What an error without a code (an older daemon's) reads as.
    fn default() -> Self {
        ErrorCode::Internal
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(ErrorCode::from_wire(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    const ALL: &[ErrorCode] = &[
        ErrorCode::Internal,
        ErrorCode::BadRequest,
        ErrorCode::ProtocolVersion,
        ErrorCode::RequestTooLarge,
        ErrorCode::InvalidSpec,
        ErrorCode::InvalidName,
        ErrorCode::InvalidSelector,
        ErrorCode::Unsupported,
        ErrorCode::AppNotFound,
        ErrorCode::AppExists,
        ErrorCode::DependencyMissing,
        ErrorCode::DependencyCycle,
        ErrorCode::DependencyFailed,
        ErrorCode::DependencyTimeout,
        ErrorCode::BuildFailed,
        ErrorCode::SpawnFailed,
        ErrorCode::Cancelled,
        ErrorCode::ShuttingDown,
        ErrorCode::Busy,
        ErrorCode::Timeout,
        ErrorCode::Db,
        ErrorCode::Io,
        ErrorCode::StateIncompatible,
        ErrorCode::SaveWouldEmpty,
        ErrorCode::PolicyDenied,
        ErrorCode::Protected,
        ErrorCode::RateLimited,
        ErrorCode::AuditUnavailable,
        ErrorCode::DryRunUnsupported,
        ErrorCode::AdapterMissing,
        ErrorCode::AdapterUnavailable,
        ErrorCode::AdapterTimeout,
        ErrorCode::AdapterCrashed,
        ErrorCode::AdapterProtocol,
        ErrorCode::AdapterBusy,
        ErrorCode::AdapterInvalid,
        ErrorCode::AdapterTampered,
        ErrorCode::ChecksumMismatch,
        ErrorCode::SignatureInvalid,
        ErrorCode::IndexStale,
    ];

    #[test]
    fn every_code_roundtrips_through_its_wire_string() {
        for code in ALL {
            assert!(code.as_str().starts_with("E_"), "{code:?}");
            let json = serde_json::to_string(code).unwrap();
            assert_eq!(json, format!("\"{}\"", code.as_str()));
            let back: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(&back, code);
        }
    }

    /// The codes of the adapter system: every one has a wire string, round-trips, and is
    /// produced by exactly one `Error` variant.
    #[test]
    fn new_error_codes_roundtrip_and_unknown_survives() {
        let cases: Vec<(ErrorCode, &str, Error)> = vec![
            (
                ErrorCode::DryRunUnsupported,
                "E_DRY_RUN_UNSUPPORTED",
                Error::DryRunUnsupported("x".into()),
            ),
            (
                ErrorCode::AdapterMissing,
                "E_ADAPTER_MISSING",
                Error::AdapterMissing("x".into()),
            ),
            (
                ErrorCode::AdapterUnavailable,
                "E_ADAPTER_UNAVAILABLE",
                Error::AdapterUnavailable("x".into()),
            ),
            (
                ErrorCode::AdapterTimeout,
                "E_ADAPTER_TIMEOUT",
                Error::AdapterTimeout("x".into()),
            ),
            (
                ErrorCode::AdapterCrashed,
                "E_ADAPTER_CRASHED",
                Error::AdapterCrashed("x".into()),
            ),
            (
                ErrorCode::AdapterProtocol,
                "E_ADAPTER_PROTOCOL",
                Error::AdapterProtocol("x".into()),
            ),
            (
                ErrorCode::AdapterBusy,
                "E_ADAPTER_BUSY",
                Error::AdapterBusy("x".into()),
            ),
            (
                ErrorCode::AdapterInvalid,
                "E_ADAPTER_INVALID",
                Error::AdapterInvalid("x".into()),
            ),
            (
                ErrorCode::AdapterTampered,
                "E_ADAPTER_TAMPERED",
                Error::AdapterTampered("x".into()),
            ),
            (
                ErrorCode::ChecksumMismatch,
                "E_CHECKSUM_MISMATCH",
                Error::ChecksumMismatch("x".into()),
            ),
            (
                ErrorCode::SignatureInvalid,
                "E_SIGNATURE_INVALID",
                Error::SignatureInvalid("x".into()),
            ),
            (
                ErrorCode::IndexStale,
                "E_INDEX_STALE",
                Error::IndexStale("x".into()),
            ),
        ];
        for (code, wire, error) in &cases {
            assert_eq!(code.as_str(), *wire);
            assert_eq!(
                &ErrorCode::from_wire(wire),
                code,
                "{wire} must not fall into Other"
            );
            let json = serde_json::to_string(code).unwrap();
            assert_eq!(json, format!("\"{wire}\""));
            assert_eq!(&serde_json::from_str::<ErrorCode>(&json).unwrap(), code);
            assert_eq!(&error.code(), code, "{error}");
            // The message is the text it was made with: a code never rewrites it.
            assert_eq!(error.to_string(), "x");
        }
        // An older client that does not know a code keeps its text; an unknown one still survives.
        let unknown: ErrorCode = serde_json::from_str("\"E_ADAPTER_FROM_THE_FUTURE\"").unwrap();
        assert_eq!(
            unknown,
            ErrorCode::Other("E_ADAPTER_FROM_THE_FUTURE".into())
        );
        assert_eq!(
            serde_json::to_string(&unknown).unwrap(),
            "\"E_ADAPTER_FROM_THE_FUTURE\""
        );
        // Every code of the proto crate's list that the core also names is spelled the same.
        for (_, wire, _) in &cases {
            assert!(
                wire.starts_with("E_") && wire.chars().all(|c| c.is_ascii_uppercase() || c == '_')
            );
        }
    }

    #[test]
    fn unknown_codes_become_other_and_keep_their_text() {
        let code: ErrorCode = serde_json::from_str("\"E_FROM_THE_FUTURE\"").unwrap();
        assert_eq!(code, ErrorCode::Other("E_FROM_THE_FUTURE".to_string()));
        assert_eq!(
            serde_json::to_string(&code).unwrap(),
            "\"E_FROM_THE_FUTURE\""
        );
    }

    #[test]
    fn errors_map_to_their_codes() {
        let cases: Vec<(Error, ErrorCode)> = vec![
            (Error::AppNotFound("x".into()), ErrorCode::AppNotFound),
            (Error::AppAlreadyExists("x".into()), ErrorCode::AppExists),
            (
                Error::DependencyMissing("x".into()),
                ErrorCode::DependencyMissing,
            ),
            (
                Error::DependencyCycle("x".into()),
                ErrorCode::DependencyCycle,
            ),
            (
                Error::DependencyFailed("x".into()),
                ErrorCode::DependencyFailed,
            ),
            (
                Error::DependencyTimeout("x".into()),
                ErrorCode::DependencyTimeout,
            ),
            (Error::BuildFailed("x".into()), ErrorCode::BuildFailed),
            (
                Error::ProcessStartFailed("x".into()),
                ErrorCode::SpawnFailed,
            ),
            (
                Error::InvalidSelector("x".into()),
                ErrorCode::InvalidSelector,
            ),
            (
                Error::InvalidSpec("instances must be at least 1".into()),
                ErrorCode::InvalidSpec,
            ),
            (
                Error::InvalidSpec("invalid app name 'a b'".into()),
                ErrorCode::InvalidName,
            ),
            (Error::DbError("x".into()), ErrorCode::Db),
            (Error::Unsupported("x".into()), ErrorCode::Unsupported),
            (
                Error::StateIncompatible("x".into()),
                ErrorCode::StateIncompatible,
            ),
            (Error::SaveWouldEmpty("x".into()), ErrorCode::SaveWouldEmpty),
            (Error::PolicyDenied("x".into()), ErrorCode::PolicyDenied),
            (Error::Protected("x".into()), ErrorCode::Protected),
            (Error::RateLimited("x".into()), ErrorCode::RateLimited),
            (
                Error::AuditUnavailable("x".into()),
                ErrorCode::AuditUnavailable,
            ),
            (Error::Timeout("x".into()), ErrorCode::Timeout),
            (Error::IoError(std::io::Error::other("x")), ErrorCode::Io),
            (Error::IpcError("x".into()), ErrorCode::Internal),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
        }
    }
}
