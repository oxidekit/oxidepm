//! The verified-download primitive of OxidePM.
//!
//! Everything that fetches an executable or an archive from the network and installs it goes
//! through this crate: `https` only, a size cap, a hash that is compared in constant time, a safe
//! tar extractor and an atomic install. It is linked into the CLI only; the daemon never links an
//! HTTP client, TLS, gzip or tar code.

mod error;
mod extract;
mod fetch;
mod install;
mod version;

pub use error::{FetchError, Result};
pub use extract::{extract_tar_gz, ExtractLimits, Extracted, ExtractedKind};
pub use fetch::{fetch_bytes, fetch_verified, hex, Expect, FetchOpts, Verified, MAX_REDIRECTS};
pub use install::{install_file_atomic, StagedFile};
pub use version::check_version;

#[cfg(test)]
mod tests_extract;
#[cfg(test)]
mod tests_fetch;
#[cfg(test)]
mod tests_install;
#[cfg(test)]
mod testserver;
