//! OxidePM Daemon - process supervisor (library part, so tests can drive the real supervisor)

#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]

pub mod adapters;
pub mod audit;
pub mod daemon;
pub mod handlers;
pub mod lock;
pub mod policy;
pub mod supervisor;
