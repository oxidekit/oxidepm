//! The adapter system's daemon side: the store, the connection to one adapter, its process and
//! its supervision.

pub mod conn;
pub mod entry;
pub mod events;
pub mod host;
pub mod hostapi;
pub mod install;
pub mod invoke;
pub mod peer;
pub mod proc;
pub mod runtime;
pub mod sandbox;
pub mod session;
pub mod store;

mod trust;
