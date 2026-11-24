//! Generic command runner

use async_trait::async_trait;
use oxidepm_core::{AppSpec, Result};

use crate::resolve::{build_env, resolve_program};
use crate::traits::{LaunchPlan, PrepareResult, Runner};

/// Generic command runner - runs any command
pub struct CmdRunner;

#[async_trait]
impl Runner for CmdRunner {
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        // For generic commands, just verify the program exists: the same resolution that
        // `plan` will do, so the two cannot disagree
        if spec.command.trim().is_empty() {
            return Ok(PrepareResult::failure("Empty command"));
        }

        let env = build_env(spec, &[]);
        match resolve_program(spec, &env) {
            Ok(path) => Ok(PrepareResult::success(format!(
                "Found {} at {}",
                spec.command,
                path.display()
            ))),
            Err(e) => Ok(PrepareResult::failure(e.to_string())),
        }
    }

    fn plan(&self, spec: &AppSpec) -> Result<LaunchPlan> {
        let program = resolve_program(spec, &build_env(spec, &[]))?;
        Ok(LaunchPlan {
            program,
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            extra_env: Vec::new(),
        })
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        if spec.args.is_empty() {
            spec.command.clone()
        } else {
            format!("{} {}", spec.command, spec.args.join(" "))
        }
    }

    fn mode_name(&self) -> &'static str {
        "cmd"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::Runtime;
    use std::path::PathBuf;

    #[tokio::test]
    async fn test_prepare_valid_command() {
        let runner = CmdRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            Runtime::CMD,
            "echo".to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn test_prepare_invalid_command() {
        let runner = CmdRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            Runtime::CMD,
            "nonexistent_command_12345".to_string(),
            PathBuf::from("/tmp"),
        );

        let result = runner.prepare(&spec).await.unwrap();
        assert!(!result.success);
    }

    fn script(dir: &std::path::Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn cmd_prepare_and_plan_agree_on_relative_program() {
        let dir = tempfile::tempdir().unwrap();
        let runner = CmdRunner;
        let spec = |command: &str| {
            AppSpec::new(
                "test".to_string(),
                Runtime::CMD,
                command.to_string(),
                dir.path().to_path_buf(),
            )
        };

        // a relative path is relative to the app's cwd, which is not the test's cwd
        let run = script(dir.path(), "run.sh");
        for command in ["./run.sh", "run.sh"] {
            let spec = spec(command);
            if command == "run.sh" {
                // a bare name is a PATH lookup, not a cwd lookup: both refuse it
                assert!(!runner.prepare(&spec).await.unwrap().success);
                assert!(runner.plan(&spec).is_err());
                continue;
            }
            let prepared = runner.prepare(&spec).await.unwrap();
            assert!(prepared.success, "{}", prepared.output);
            assert_eq!(runner.plan(&spec).unwrap().program, run);
        }

        // and when the file is not there both say so
        let missing = spec("./missing.sh");
        let prepared = runner.prepare(&missing).await.unwrap();
        assert!(!prepared.success);
        assert!(
            prepared.output.contains("missing.sh"),
            "{}",
            prepared.output
        );
        assert!(runner.plan(&missing).is_err());

        // a PATH set in the app's env decides the lookup of a bare name
        let mut with_path = spec("run.sh");
        with_path
            .env
            .insert("PATH".to_string(), dir.path().to_string_lossy().to_string());
        assert!(runner.prepare(&with_path).await.unwrap().success);
        assert_eq!(runner.plan(&with_path).unwrap().program, run);
    }

    #[tokio::test]
    async fn cmd_multiword_error_mentions_args() {
        let runner = CmdRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            Runtime::CMD,
            "sleep 1".to_string(),
            PathBuf::from("/"),
        );

        let prepared = runner.prepare(&spec).await.unwrap();
        assert!(!prepared.success);
        for text in [prepared.output, runner.plan(&spec).unwrap_err().to_string()] {
            assert!(text.contains("'sleep 1' not found"), "{text}");
            assert!(text.contains("args"), "{text}");
            assert!(text.contains("command='sleep', args=['1']"), "{text}");
            assert!(text.contains("sh -c"), "{text}");
        }
    }

    #[test]
    fn test_plan_is_program_args_cwd() {
        let runner = CmdRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            Runtime::CMD,
            "sleep".to_string(),
            PathBuf::from("/tmp"),
        )
        .with_args(vec!["1".to_string()]);

        let plan = runner.plan(&spec).unwrap();
        assert_eq!(
            plan.program,
            which::which("sleep").unwrap(),
            "resolved through PATH"
        );
        assert_eq!(plan.args, vec!["1".to_string()]);
        assert_eq!(plan.cwd, PathBuf::from("/tmp"));
        assert!(plan.extra_env.is_empty());
    }

    #[tokio::test]
    async fn test_spawn_plan() {
        let runner = CmdRunner;
        let spec = AppSpec::new(
            "test".to_string(),
            Runtime::CMD,
            "sleep".to_string(),
            PathBuf::from("/tmp"),
        )
        .with_args(vec!["1".to_string()]);

        let plan = runner.plan(&spec).unwrap();
        let mut child = crate::spawn_group(&plan, &crate::ChildEnv::inherit([])).unwrap();
        assert!(child.pid() > 0);
        assert_eq!(child.pgid(), child.pid() as i32);

        // Clean up
        child.kill_and_wait().await;
    }

    #[tokio::test]
    async fn test_spawn_plan_missing_program_names_it() {
        let plan = LaunchPlan {
            program: PathBuf::from("nonexistent_command_12345"),
            args: vec![],
            cwd: PathBuf::from("/tmp"),
            extra_env: vec![],
        };
        let err = match crate::spawn_group(&plan, &crate::ChildEnv::inherit([])) {
            Ok(_) => panic!("spawned a program that does not exist"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("nonexistent_command_12345"), "{err}");
    }
}
