//! OxidePM Runtime - Process runners for different languages/modes

pub mod cmd;
pub mod resolve;
pub mod spawn;
pub mod step;
pub mod traits;

pub use cmd::CmdRunner;
pub use resolve::{
    build_env, build_env_from, resolve_program, script_base_env, script_env, script_env_from,
    EnvMode, APP_ENV_MODE, SCRIPT_ENV_MODE,
};
pub use spawn::{
    group_command, group_exists, kill_group, run_launcher_selftest, run_script, run_script_with,
    spawn_adapter, spawn_group, terminate_group, AdapterLaunch, ChildEnv, GroupChild,
    SandboxLaunch, ScriptOutcome, ScriptSpec, Terminated,
};
pub use traits::{BuiltinRuntimes, LaunchPlan, Origin, PrepareResult, Runner, RuntimeSource};

use oxidepm_core::Runtime;

/// Get the built-in runner for a runtime; `None` for a name no built-in runner serves.
pub fn get_runner(mode: &Runtime) -> Option<Box<dyn Runner>> {
    Some(match mode.as_str() {
        "cmd" => Box::new(CmdRunner),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::AppSpec;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// An executable that records that it ran, so a plan that executed it is caught.
    fn tripwire(path: &Path, ran: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn spec(mode: Runtime, command: &str, dir: &Path) -> AppSpec {
        AppSpec::new(
            "plan-test".to_string(),
            mode,
            command.to_string(),
            dir.to_path_buf(),
        )
        .with_args(vec!["--port".to_string(), "3000".to_string()])
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn builtin_runtimes_serve_the_built_in_runners_and_refuse_other_names() {
        for name in ["cmd"] {
            let runtime: Runtime = name.parse().unwrap();
            let runner = BuiltinRuntimes
                .runner_for(&runtime, Origin::Start)
                .await
                .unwrap();
            assert_eq!(runner.mode_name(), name);
        }
        let err = BuiltinRuntimes
            .runner_for(&"ruby".parse().unwrap(), Origin::Start)
            .await
            .err()
            .expect("no runner for ruby");
        assert!(err.to_string().contains("ruby"), "{err}");
    }

    #[test]
    fn runtimes_that_come_from_adapters_have_no_built_in_runner() {
        for name in [
            "node", "npm", "pnpm", "yarn", "python", "go", "rust", "cargo",
        ] {
            assert!(
                get_runner(&name.parse().unwrap()).is_none(),
                "{name} comes from an adapter"
            );
        }
    }

    #[test]
    fn plan_of_cmd_is_pure() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let ran = cwd.join("ran");
        let bin = cwd.join("app-bin");
        tripwire(&bin, &ran);

        let plan = get_runner(&Runtime::CMD)
            .unwrap()
            .plan(&spec(Runtime::CMD, &bin.to_string_lossy(), cwd))
            .unwrap();
        assert_eq!(plan.program, bin);
        assert_eq!(plan.args, strings(&["--port", "3000"]));
        assert_eq!(plan.cwd, cwd);
        assert!(plan.extra_env.is_empty());

        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!ran.exists(), "planning executed the program");
    }
}

#[cfg(test)]
mod step_tests;
