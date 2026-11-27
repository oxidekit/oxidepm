#![forbid(unsafe_code)]

//! The OxidePM adapter protocol.
//!
//! Adapters are separate executables that speak a versioned JSON protocol over stdin and stdout.
//! This crate is the single definition of that protocol, used by the daemon, the fixture adapter
//! and the adapter SDK: the frames, the limits, the bounded line reader, the error codes, the
//! manifest and the index with their validation, the permission comparison and the consent
//! fingerprint. It contains no process, socket or file-system code beyond reading the manifest's
//! executable header, and it has no dependency on the rest of OxidePM.

pub mod codes;
pub mod frame;
pub mod index;
pub mod limits;
pub mod manifest;
pub mod messages;
pub mod permissions;
pub mod reader;
pub mod signing;
pub mod text;

pub use frame::{from_params, parse_frame, to_params, ErrorObject, Frame, FrameError, Params};
pub use index::{host_target, Artifact, Index, IndexAdapter, Offer, Release, Revoked};
pub use limits::{
    InitializeLimits, Limits, INDEX_MAX_BYTES, MANIFEST_MAX_BYTES, SUPPORTED_PROTOCOLS,
};
pub use manifest::{
    Invalid, Kind, Manifest, Provides, Rule, ValidateCtx, Verb, VerbMode, VerbTier,
    FIXED_RESERVED_NAMES,
};
pub use permissions::{Growth, HostLevel, HostScope, Permissions};
#[cfg(feature = "tokio")]
pub use reader::AsyncFrameReader;
pub use reader::{FrameReader, LineAssembler, LineEvent, Read};
pub use signing::{
    check_trusted_comment, official_keys, trusted_comment_for, verify_index, verify_minisign,
    IndexKey, SigError, SignedBy, VerifiedIndex,
};

#[cfg(test)]
mod tests_frame;
#[cfg(test)]
mod tests_index;
#[cfg(test)]
mod tests_manifest;
#[cfg(test)]
mod tests_permissions;
#[cfg(test)]
mod tests_signing;
#[cfg(test)]
mod testutil;
