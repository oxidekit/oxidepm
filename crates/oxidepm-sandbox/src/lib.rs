//! The OS sandbox for adapters.
//!
//! This library holds what the daemon needs: the [`Profile`] it hands to the launcher, the
//! [`probe`] of what the kernel offers and the self-test [`SelfTestReport`]. It contains no
//! Landlock or seccomp code and no `unsafe` beyond two read-only queries. The launcher binary
//! `oxidepm-sandbox` (feature `launcher`, on by default) is the only code that restricts a
//! process, and it restricts only itself, once, before it executes the adapter.

pub mod caps;
pub mod profile;
pub mod report;

pub use caps::{probe, SandboxCaps};
pub use profile::{find_program, Profile, ProfileInput};
pub use report::{
    enforce_name, missing_enforcement, EnforcedRecord, Outcome, SelfTestReport, SelftestPlan,
};

/// The descriptor on which the launcher receives the verified executable.
pub const EXE_FD: i32 = 3;
/// The launcher exits with this status when it could not set the sandbox up.
pub const EXIT_SANDBOX_FAILED: i32 = 126;
