//! OxidePM IPC - Inter-process communication via Unix sockets

pub mod adapter;
pub mod client;
pub mod protocol;
pub mod server;
pub mod session;

pub use adapter::{
    AdapterInfo, AdapterReply, AdapterRequest, EnableArgs, InstallArgs, InvokeArgs, VerbTable,
    VerbTableEntry,
};
pub use client::{CallOptions, IpcClient};
pub use protocol::{
    AdapterStatus, DryRunPlan, Envelope, ItemOutcome, ItemResult, PlannedItem, PolicyDecision,
    Request, Response, ResponseEnvelope, SandboxStatus,
};
pub use server::IpcServer;
