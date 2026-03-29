//! Command implementations

pub mod check;
pub mod cosmos_status;
pub mod delete;
pub mod describe;
pub mod flush;
pub mod kill;
pub mod logs;
pub mod notify;
pub mod ping;
pub mod restart;
pub mod resurrect;
pub mod save;
pub mod show;
pub mod start;
pub mod startup;
pub mod status;
pub mod stop;
pub mod update;

use oxidepm_core::constants;
use oxidepm_ipc::IpcClient;

/// Get the IPC client
pub fn get_client() -> IpcClient {
    IpcClient::new(constants::socket_path())
}
