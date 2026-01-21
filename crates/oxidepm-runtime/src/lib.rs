//! OxidePM Runtime - Process runners for different languages/modes

pub mod cargo;
pub mod cmd;
pub mod node;
pub mod npm;
pub mod rust;
pub mod traits;

pub use cargo::CargoRunner;
pub use cmd::CmdRunner;
pub use node::NodeRunner;
pub use npm::NpmRunner;
pub use rust::RustRunner;
pub use traits::{PrepareResult, Runner, RunningProcess};

use oxidepm_core::AppMode;

/// Get the appropriate runner for an app mode
pub fn get_runner(mode: AppMode) -> Box<dyn Runner> {
    match mode {
        AppMode::Cmd => Box::new(CmdRunner),
        AppMode::Node => Box::new(NodeRunner),
        AppMode::Npm => Box::new(NpmRunner::new("npm")),
        AppMode::Pnpm => Box::new(NpmRunner::new("pnpm")),
        AppMode::Yarn => Box::new(NpmRunner::new("yarn")),
        AppMode::Cargo => Box::new(CargoRunner),
        AppMode::Rust => Box::new(RustRunner),
    }
}
