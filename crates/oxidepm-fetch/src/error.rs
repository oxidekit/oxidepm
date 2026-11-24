//! Errors of the fetch crate. Each variant is one thing that went wrong, so a caller (and a test)
//! can tell a hash mismatch from a size cap from a refused scheme.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("refusing {0}: only https URLs are fetched")]
    NotHttps(String),
    #[error("invalid URL {0:?}")]
    BadUrl(String),
    #[error("refusing a redirect to {0}: only https URLs are fetched")]
    RedirectNotHttps(String),
    #[error("too many redirects (at most {0} are followed)")]
    TooManyRedirects(usize),
    #[error("the server answered HTTP {0}")]
    Status(u16),
    #[error("the server announces {announced} bytes, more than the limit of {max}")]
    TooLargeAnnounced { announced: u64, max: u64 },
    #[error("the download exceeds the limit of {max} bytes and was cut off")]
    TooLarge { max: u64 },
    #[error("sha256 mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("{path}: {reason}")]
    Unsafe { path: PathBuf, reason: String },
    #[error("version {0:?} is not acceptable: {1}")]
    BadVersion(String, String),
    #[error(
        "version {candidate} is older than the installed {current}; pass --force to downgrade"
    )]
    Downgrade { candidate: String, current: String },
    #[error("unsafe archive: {0}")]
    BadArchive(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

pub type Result<T> = std::result::Result<T, FetchError>;

impl FetchError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        FetchError::Io {
            context: context.into(),
            source,
        }
    }
}
