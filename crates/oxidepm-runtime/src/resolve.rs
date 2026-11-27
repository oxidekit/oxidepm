//! What a spec turns into at launch time: the program that will be executed and the
//! environment it gets (isolated from the daemon's unless the spec asks for it). The cmd runner's `prepare` and `plan` both go through here, so the
//! check and the launch cannot disagree.

use oxidepm_core::{AppSpec, Error, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::spawn::ChildEnv;

/// How app processes get their environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvMode {
    /// An app gets a small base set from the daemon's environment plus what its spec says; the
    /// daemon's whole environment only when the spec has `env_inherit`.
    Isolated,
    /// Every app gets the daemon's whole environment (the behaviour before isolation).
    InheritAll,
}

/// The one switch for environment isolation: `EnvMode::InheritAll` here gives every app the
/// daemon's environment again, whatever its spec says.
pub const APP_ENV_MODE: EnvMode = EnvMode::Isolated;

/// The switch for the environment of hooks and script health probes. They never get the
/// daemon's whole environment (it may hold secrets): `EnvMode::InheritAll` here gives it to
/// them again.
pub const SCRIPT_ENV_MODE: EnvMode = EnvMode::Isolated;

/// Variables of the daemon's environment an isolated app still gets. `LC_*` is any variable
/// that starts with `LC_`.
const BASE_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TZ", "TMPDIR",
];

fn in_base_env(key: &str) -> bool {
    BASE_ENV.contains(&key) || key.starts_with("LC_")
}

/// Resolve relative search directories against the canonical app directory.
pub fn absolute_search_path(path: &OsStr, app_dir: &Path) -> Result<OsString> {
    let dirs = std::env::split_paths(path).map(|dir| app_dir.join(dir));
    std::env::join_paths(dirs)
        .map_err(|e| Error::AdapterInvalid(format!("cannot resolve the runtime search path: {e}")))
}

/// Keep PATH-based lookups in adapter-launched processes independent of their working directory.
pub fn anchor_runtime_path(env: &mut ChildEnv, app_dir: &Path) -> Result<()> {
    if let Some(path) = env.get("PATH") {
        let absolute = absolute_search_path(&path, app_dir)?
            .into_string()
            .map_err(|_| Error::AdapterInvalid("the runtime search path is not UTF-8".into()))?;
        env.vars.push(("PATH".into(), absolute));
    }
    Ok(())
}

/// The environment an app process gets.
///
/// With `env_inherit` (or `APP_ENV_MODE == InheritAll`) the base is the daemon's whole
/// environment. Otherwise it starts empty and gets the base set (`PATH`, `HOME`, `USER`,
/// `LOGNAME`, `SHELL`, `LANG`, `LC_*`, `TZ`, `TMPDIR`) from the daemon. Then, in this order:
/// the app's own `env`, `OPM_APP_ID`, `OPM_APP_NAME` (and `OPM_INSTANCE_ID` for a cluster
/// instance), and the runner's `extra_env`; a later entry wins. Build commands (`prepare`) do
/// not use this: they keep the daemon's environment.
pub fn build_env(spec: &AppSpec, extra_env: &[(String, String)]) -> ChildEnv {
    build_env_from(spec, extra_env, &daemon_vars(), APP_ENV_MODE)
}

/// [`build_env`] with the daemon's environment and the mode given.
pub fn build_env_from(
    spec: &AppSpec,
    extra_env: &[(String, String)],
    daemon_env: &[(String, String)],
    mode: EnvMode,
) -> ChildEnv {
    let inherit = spec.env_inherit || mode == EnvMode::InheritAll;
    let mut vars: Vec<(String, String)> = Vec::new();
    if !inherit {
        vars.extend(daemon_env.iter().filter(|(k, _)| in_base_env(k)).cloned());
    }
    vars.extend(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    vars.push(("OPM_APP_ID".to_string(), spec.id.to_string()));
    vars.push(("OPM_APP_NAME".to_string(), spec.name.clone()));
    if let Some(instance) = spec.instance_id {
        vars.push(("OPM_INSTANCE_ID".to_string(), instance.to_string()));
    }
    vars.extend(extra_env.iter().cloned());
    ChildEnv {
        clear: !inherit,
        vars,
    }
}

/// The environment of a hook or a script health probe of `spec`: the base set from the
/// daemon (`PATH`, `HOME`, ...), then `spec.env`, `OPM_APP_ID`, `OPM_APP_NAME` and `extra`
/// (the hook's own `OPM_EVENT`, `OPM_PID`, ...); a later entry wins. Never the daemon's whole
/// environment, whatever `spec.env_inherit` says (see [`SCRIPT_ENV_MODE`]).
pub fn script_env(spec: &AppSpec, extra: &[(String, String)]) -> ChildEnv {
    script_env_from(spec, extra, &daemon_vars(), SCRIPT_ENV_MODE)
}

/// [`script_env`] with the daemon's environment and the mode given.
pub fn script_env_from(
    spec: &AppSpec,
    extra: &[(String, String)],
    daemon_env: &[(String, String)],
    mode: EnvMode,
) -> ChildEnv {
    let mut env = script_base_env_from(daemon_env, mode);
    env.vars
        .extend(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    env.vars
        .push(("OPM_APP_ID".to_string(), spec.id.to_string()));
    env.vars
        .push(("OPM_APP_NAME".to_string(), spec.name.clone()));
    env.vars.extend(extra.iter().cloned());
    env
}

/// The environment of a script that has no app to say what it needs: only the base set.
pub fn script_base_env() -> ChildEnv {
    script_base_env_from(&daemon_vars(), SCRIPT_ENV_MODE)
}

fn script_base_env_from(daemon_env: &[(String, String)], mode: EnvMode) -> ChildEnv {
    if mode == EnvMode::InheritAll {
        return ChildEnv::inherit(Vec::new());
    }
    ChildEnv {
        clear: true,
        vars: daemon_env
            .iter()
            .filter(|(k, _)| in_base_env(k))
            .cloned()
            .collect(),
    }
}

pub(crate) fn daemon_vars() -> Vec<(String, String)> {
    std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect()
}

/// The program a `cmd` app runs: `spec.command` is a program, `spec.args` its arguments; there
/// is no shell and no splitting at whitespace.
///
/// 1. A command with a `/` is a path; a relative path is relative to `spec.cwd` (not to the
///    daemon's own directory).
/// 2. Any other command is looked up in the `PATH` of `env` (the environment the process will
///    get), not in the daemon's.
/// 3. A command that does not resolve and contains whitespace gets an error that says how to
///    fix the spec.
pub fn resolve_program(spec: &AppSpec, env: &ChildEnv) -> Result<PathBuf> {
    let command = spec.command.as_str();

    if command.contains('/') {
        let path = if std::path::Path::new(command).is_absolute() {
            PathBuf::from(command)
        } else {
            spec.cwd.join(command)
        };
        if path.exists() {
            return Ok(path);
        }
        return Err(not_found(command, Some(&path)));
    }

    let search: Option<OsString> = env.get("PATH");
    if let Some(search) = search {
        if let Ok(path) = which::which_in(command, Some(search), &spec.cwd) {
            return Ok(path);
        }
    }
    Err(not_found(command, None))
}

fn not_found(command: &str, path: Option<&std::path::Path>) -> Error {
    let mut parts = command.split_whitespace();
    let program = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();
    let message = if !rest.is_empty() {
        let args: Vec<String> = rest.iter().map(|a| format!("'{a}'")).collect();
        format!(
            "command '{command}' not found; command is a program path and arguments belong \
             in args (command='{program}', args=[{}]), or use sh -c",
            args.join(", ")
        )
    } else if let Some(path) = path {
        format!("command '{command}' not found at {}", path.display())
    } else {
        format!("command '{command}' not found in PATH")
    };
    Error::ProcessStartFailed(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::Runtime;

    fn daemon_env() -> Vec<(String, String)> {
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/x"),
            ("USER", "x"),
            ("LOGNAME", "x"),
            ("SHELL", "/bin/sh"),
            ("LANG", "C"),
            ("LC_ALL", "C"),
            ("LC_TIME", "C"),
            ("TZ", "UTC"),
            ("TMPDIR", "/var/tmp"),
            ("AWS_SECRET_ACCESS_KEY", "leak"),
            ("OXIDEPM_HOME", "/h"),
            ("LCX", "not a base variable"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    fn spec() -> AppSpec {
        let mut spec = AppSpec::new(
            "web".to_string(),
            Runtime::CMD,
            "sleep".to_string(),
            std::path::PathBuf::from("/"),
        );
        spec.id = 7;
        spec
    }

    fn get<'a>(env: &'a ChildEnv, key: &str) -> Option<&'a str> {
        env.vars
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn build_env_clear_vs_inherit() {
        let daemon = daemon_env();

        let isolated = build_env_from(&spec(), &[], &daemon, EnvMode::Isolated);
        assert!(isolated.clear);
        for key in [
            "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_TIME", "TZ", "TMPDIR",
        ] {
            assert!(get(&isolated, key).is_some(), "{key} is in the base set");
        }
        for key in ["AWS_SECRET_ACCESS_KEY", "OXIDEPM_HOME", "LCX"] {
            assert!(get(&isolated, key).is_none(), "{key} is not");
        }

        let mut inheriting = spec();
        inheriting.env_inherit = true;
        let inherited = build_env_from(&inheriting, &[], &daemon, EnvMode::Isolated);
        assert!(
            !inherited.clear,
            "the daemon's whole environment is the base"
        );
        assert!(
            get(&inherited, "AWS_SECRET_ACCESS_KEY").is_none(),
            "(the base is implicit, not copied)"
        );

        // the one switch gives everything the daemon's environment
        let all = build_env_from(&spec(), &[], &daemon, EnvMode::InheritAll);
        assert!(!all.clear);
    }

    #[test]
    fn build_env_layers_win_in_order() {
        let daemon = daemon_env();
        let mut spec = spec();
        spec.instance_id = Some(2);
        spec.env
            .insert("HOME".to_string(), "/from/spec".to_string());
        spec.env
            .insert("OPM_APP_NAME".to_string(), "spoofed".to_string());
        spec.env.insert("EXTRA".to_string(), "spec".to_string());
        let extra = vec![("EXTRA".to_string(), "runner".to_string())];

        let env = build_env_from(&spec, &extra, &daemon, EnvMode::Isolated);
        assert_eq!(
            get(&env, "HOME"),
            Some("/from/spec"),
            "the spec overrides the base"
        );
        assert_eq!(get(&env, "OPM_APP_ID"), Some("7"));
        assert_eq!(
            get(&env, "OPM_APP_NAME"),
            Some("web"),
            "the daemon's identity beats the spec's"
        );
        assert_eq!(get(&env, "OPM_INSTANCE_ID"), Some("2"));
        assert_eq!(
            get(&env, "EXTRA"),
            Some("runner"),
            "the runner's variables come last"
        );

        let single = build_env_from(&self::spec(), &[], &daemon, EnvMode::Isolated);
        assert!(get(&single, "OPM_INSTANCE_ID").is_none());
    }

    #[test]
    fn app_env_mode_is_isolated_by_default() {
        assert_eq!(APP_ENV_MODE, EnvMode::Isolated);
    }

    #[test]
    fn script_env_is_base_plus_spec_env_plus_opm_vars_and_never_inherits() {
        let daemon = daemon_env();
        let mut spec = spec();
        spec.env.insert("FROM_SPEC".to_string(), "yes".to_string());
        spec.env_inherit = true; // an app may inherit; a hook or a probe may not
        let extra = vec![("OPM_EVENT".to_string(), "start".to_string())];

        let env = script_env_from(&spec, &extra, &daemon, EnvMode::Isolated);
        assert!(env.clear);
        assert_eq!(get(&env, "PATH"), Some("/usr/bin"));
        assert_eq!(get(&env, "FROM_SPEC"), Some("yes"));
        assert_eq!(get(&env, "OPM_APP_ID"), Some("7"));
        assert_eq!(get(&env, "OPM_APP_NAME"), Some("web"));
        assert_eq!(get(&env, "OPM_EVENT"), Some("start"));
        for key in ["AWS_SECRET_ACCESS_KEY", "OXIDEPM_HOME", "LCX"] {
            assert!(get(&env, key).is_none(), "{key} must not reach a hook");
        }

        let base = script_base_env_from(&daemon, EnvMode::Isolated);
        assert!(
            base.clear
                && get(&base, "HOME") == Some("/home/x")
                && get(&base, "FROM_SPEC").is_none()
        );

        // the one switch gives hooks and probes the daemon's environment again
        let all = script_env_from(&spec, &extra, &daemon, EnvMode::InheritAll);
        assert!(!all.clear);
        assert_eq!(get(&all, "FROM_SPEC"), Some("yes"));
    }

    #[test]
    fn script_env_mode_is_isolated_by_default() {
        assert_eq!(SCRIPT_ENV_MODE, EnvMode::Isolated);
    }
}
