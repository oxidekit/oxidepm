//! Runner trait and common types

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Result, Runtime};
use std::path::PathBuf;

/// Result of the prepare phase (build/validate)
#[derive(Debug)]
pub struct PrepareResult {
    pub success: bool,
    pub output: String,
    pub binary_path: Option<PathBuf>,
    /// The plan, when the runner made it while it prepared (a runtime adapter does; the
    /// built-in runners leave it to [`Runner::plan`]).
    pub plan: Option<LaunchPlan>,
    /// Things the person should know about this launch (a stored plan was used, ...).
    pub warnings: Vec<String>,
}

impl PrepareResult {
    pub fn success<S: Into<String>>(output: S) -> Self {
        Self {
            success: true,
            output: output.into(),
            binary_path: None,
            plan: None,
            warnings: Vec::new(),
        }
    }

    pub fn failure<S: Into<String>>(output: S) -> Self {
        Self {
            success: false,
            output: output.into(),
            binary_path: None,
            plan: None,
            warnings: Vec::new(),
        }
    }
}

/// What to run for an app: the result of [`Runner::plan`]. Spawning it is the job of
/// [`spawn_group`](crate::spawn_group), the one place an app process is created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// The program to execute (a name that is looked up in `PATH`, or a path).
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Variables the runner needs set for this app, applied after the app's own `env`.
    pub extra_env: Vec<(String, String)>,
}

/// Trait for process runners (Node, Cargo, etc.)
#[async_trait]
pub trait Runner: Send + Sync {
    /// Whatever must be asked of the outside before a launch slot is taken. It runs before the
    /// slot, so a slow answer holds up this launch and no other. The built-in runners have
    /// nothing to ask.
    async fn consult(&self, _spec: &AppSpec) -> Result<()> {
        Ok(())
    }

    /// Prepare the process (build for Rust, validate for Node)
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult>;

    /// What to execute for this app. Pure: it resolves the program and its arguments and
    /// spawns nothing.
    fn plan(&self, spec: &AppSpec) -> Result<LaunchPlan>;

    /// Get the command that will be executed (for display)
    fn command_string(&self, spec: &AppSpec) -> String;

    /// Get the mode name
    fn mode_name(&self) -> &'static str;
}

/// How a launch came about. A stored plan may be used only for the launches nobody asked for
/// in person (a resurrect, an automatic restart), and an adapter is told whether this is a
/// start or a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A person (or a client) started the app.
    Start,
    /// A person (or a client) restarted or reloaded it.
    Restart,
    /// The boot or an explicit resurrect.
    Resurrect,
    /// The daemon decided on this launch (a restart after a crash, the cron).
    Auto,
}

/// Where the pipeline gets the runner for a runtime. Asking can fail (no provider), and can take
/// a moment (an adapter), so it is async and fallible; the built-in runners answer at once.
#[async_trait]
pub trait RuntimeSource: Send + Sync {
    /// The runner for `runtime`, or why there is none.
    async fn runner_for(&self, runtime: &Runtime, origin: Origin) -> Result<Box<dyn Runner>>;

    /// A non-blocking look before an app is accepted for a start: is there a provider for its
    /// runtime? `E_ADAPTER_MISSING` if not.
    fn check(&self, _spec: &AppSpec) -> Result<()> {
        Ok(())
    }

    /// A non-blocking look before an app that is running is restarted: can the provider be
    /// asked right now? `E_ADAPTER_UNAVAILABLE` if not.
    fn ready(&self, _runtime: &Runtime) -> Result<()> {
        Ok(())
    }

    /// The app called `app_name` is gone: whatever the source remembers of it (a stored launch
    /// plan) goes too, so an app created later under the same name starts from nothing.
    async fn forget(&self, _app_name: &str) {}
}

/// The runners that are part of this build, served through [`RuntimeSource`].
pub struct BuiltinRuntimes;

#[async_trait]
impl RuntimeSource for BuiltinRuntimes {
    async fn runner_for(&self, runtime: &Runtime, _origin: Origin) -> Result<Box<dyn Runner>> {
        crate::get_runner(runtime).ok_or_else(|| {
            oxidepm_core::Error::InvalidMode(format!(
                "runtime '{runtime}' is not served by this daemon"
            ))
        })
    }
}
