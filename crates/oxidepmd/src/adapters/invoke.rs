//! Relayed verbs: the daemon's half. A verb of an installed adapter is resolved
//! here, the directory it will work in is checked, and the call is handed to the adapter's entry,
//! which runs it and reports its output as it comes. The table of verbs the CLI reads when clap
//! does not know a command is written here too.
//!
//! The CLI never runs an adapter binary for a verb: policy, audit and the sandbox are all the
//! daemon's, so none of them can be left out by a client.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use oxidepm_adapter_proto::messages::{ActorView, InvokeParams, InvokeResult};
use oxidepm_adapter_proto::{HostLevel, Verb, VerbMode, VerbTier};
use oxidepm_core::runtime_hints::VERB_ADAPTER;
use oxidepm_core::{Error, Result};
use oxidepm_ipc::{VerbTable, VerbTableEntry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::conn::Output;
use super::entry::{AdapterEntry, InvokeCall};
use super::host::AdapterHost;
use super::install::write_private_atomic;

/// Most arguments, and most bytes of arguments, one relayed verb takes (the frame limit is 256 KiB).
const MAX_ARGS: usize = 1024;
const MAX_ARG_BYTES: usize = 128 * 1024;

/// A verb and the adapter that provides it.
#[derive(Clone)]
pub struct ResolvedVerb {
    pub adapter: String,
    pub entry: Arc<AdapterEntry>,
    pub verb: Verb,
    /// The host level the user granted the adapter.
    pub level: HostLevel,
    /// The manifest marks the adapter `ai_driven`.
    pub ai_driven: bool,
}

/// What one invocation is asked to do, after the checks.
pub struct InvokeRun {
    pub argv: Vec<String>,
    /// The checked working directory (`$CWD` of the profile).
    pub cwd: PathBuf,
    /// A rehearsal: only for a verb that declared `dry_run`.
    pub dry_run: bool,
    pub actor: ActorView,
}

/// Which of the adapters that provide a verb answers for it. `enabled` are the adapters that are
/// enabled, `disabled` the installed ones that are not. Exactly one enabled adapter may provide a
/// verb: with two the call is refused (the install rules stop this; a database edited by hand, or
/// a bug, must not make the daemon pick one).
pub fn choose(verb: &str, enabled: &[String], disabled: &[String]) -> Result<usize> {
    match enabled.len() {
        1 => Ok(0),
        0 => {
            if let Some(adapter) = disabled.first() {
                return Err(Error::AdapterUnavailable(format!(
                    "the verb '{verb}' is provided by the adapter '{adapter}', which is installed but not enabled. Enable it with `oxidepm adapter enable {adapter}`"
                )));
            }
            Err(Error::AdapterMissing(match VERB_ADAPTER.iter().find(|(v, _)| *v == verb) {
                Some((_, adapter)) => format!(
                    "'{verb}' is provided by the adapter '{adapter}', which is not installed. Install it with `oxidepm adapter import {adapter}` (see `oxidepm adapter list --available`)"
                ),
                None => format!("no installed adapter provides the verb '{verb}'. See `oxidepm adapter list`"),
            }))
        }
        _ => Err(Error::AdapterInvalid(format!(
            "the verb '{verb}' is provided by more than one enabled adapter ({}); disable all but one",
            enabled.join(", ")
        ))),
    }
}

/// Is `cwd` a directory a verb may be given as its working directory? An existing directory, not
/// `/`, not the daemon's home or anything in it, and not a directory that contains the daemon's
/// home or the user's: `$CWD` becomes a place the adapter may write, so it must not be wide enough
/// to hold the daemon's own files. Returns the directory with links resolved.
pub fn check_cwd(cwd: &str, daemon_home: &Path, user_home: Option<&Path>) -> Result<PathBuf> {
    let refuse = |why: String| {
        Error::PolicyDenied(format!(
            "the working directory {cwd:?} is not allowed for an adapter verb: {why}"
        ))
    };
    if cwd.is_empty() || cwd.contains('\0') || !Path::new(cwd).is_absolute() {
        return Err(refuse("it must be an absolute path".to_string()));
    }
    let real =
        std::fs::canonicalize(cwd).map_err(|e| refuse(format!("it cannot be used ({e})")))?;
    if !real.is_dir() {
        return Err(refuse("it is not a directory".to_string()));
    }
    if real == Path::new("/") {
        return Err(refuse("it is the root directory".to_string()));
    }
    let real_daemon =
        std::fs::canonicalize(daemon_home).unwrap_or_else(|_| daemon_home.to_path_buf());
    if real.starts_with(&real_daemon) {
        return Err(refuse("it is inside the daemon's home".to_string()));
    }
    let mut contained = vec![real_daemon];
    if let Some(home) = user_home {
        contained.push(std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()));
    }
    if contained.iter().any(|home| home.starts_with(&real)) {
        return Err(refuse(
            "it contains a home directory; run the command inside a project directory".to_string(),
        ));
    }
    Ok(real)
}

/// Are `argv` few and short enough to send to an adapter (and free of NUL)?
pub fn check_arguments(verb: &str, argv: &[String]) -> Result<()> {
    if argv.len() > MAX_ARGS
        || argv.iter().map(String::len).sum::<usize>() > MAX_ARG_BYTES
        || argv.iter().any(|a| a.contains('\0'))
    {
        return Err(Error::InvalidSpec(format!(
            "the arguments of '{verb}' are too many or too long (at most {MAX_ARGS} arguments and {MAX_ARG_BYTES} bytes, no NUL)"
        )));
    }
    Ok(())
}

fn tier_name(tier: VerbTier) -> &'static str {
    match tier {
        VerbTier::Safe => "safe",
        VerbTier::Caution => "caution",
        VerbTier::Dangerous => "dangerous",
    }
}

impl AdapterHost {
    /// The adapter that answers for `name`, and the verb as its manifest declares it, for a
    /// relayed call. A session verb is refused here: it needs a terminal.
    pub fn resolve_verb(&self, name: &str) -> Result<ResolvedVerb> {
        let resolved = self.find_verb(name)?;
        if resolved.verb.mode == VerbMode::Session {
            return Err(Error::Unsupported(format!(
                "'{name}' is an interactive verb and needs a terminal; run it as `oxidepm {name}` in a terminal or from a program that gives it one"
            )));
        }
        Ok(resolved)
    }

    /// The adapter that answers for `name` and opens it as a session: the verb must be a session
    /// verb.
    pub fn resolve_session_verb(&self, name: &str) -> Result<ResolvedVerb> {
        let resolved = self.find_verb(name)?;
        if resolved.verb.mode != VerbMode::Session {
            return Err(Error::Unsupported(format!(
                "'{name}' is not an interactive verb, so it has no session"
            )));
        }
        Ok(resolved)
    }

    fn find_verb(&self, name: &str) -> Result<ResolvedVerb> {
        let mut enabled: Vec<(String, Arc<AdapterEntry>, Verb)> = Vec::new();
        let mut disabled: Vec<String> = Vec::new();
        for (adapter, entry) in self.entries.read().iter() {
            let Some(verb) = entry.verbs().iter().find(|v| v.name == name) else {
                continue;
            };
            if entry.is_enabled() {
                enabled.push((adapter.clone(), Arc::clone(entry), verb.clone()));
            } else {
                disabled.push(adapter.clone());
            }
        }
        let names: Vec<String> = enabled.iter().map(|(a, ..)| a.clone()).collect();
        let (adapter, entry, verb) = enabled.swap_remove(choose(name, &names, &disabled)?);
        let level = entry.grant().level;
        let ai_driven = entry.ai_driven();
        Ok(ResolvedVerb {
            adapter,
            entry,
            verb,
            level,
            ai_driven,
        })
    }

    /// Check the working directory a client sent (see [`check_cwd`]).
    pub fn check_cwd(&self, cwd: &str) -> Result<PathBuf> {
        let home = self
            .ctx
            .get()
            .map(|c| c.paths.home().to_path_buf())
            .unwrap_or_default();
        let user = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        check_cwd(cwd, &home, user.as_deref())
    }

    /// How long one line of output may take to reach the client.
    pub fn relay_write_timeout(&self) -> Duration {
        self.cfg.relay_write
    }

    /// Run a resolved verb. `out` receives its output as it comes; `cancel` ends the call (the
    /// client went away or stopped reading). A rehearsal of a verb that did not declare `dry_run`
    /// is refused before anything is started.
    pub async fn invoke(
        &self,
        resolved: &ResolvedVerb,
        run: InvokeRun,
        out: mpsc::Sender<Output>,
        cancel: CancellationToken,
    ) -> Result<InvokeResult> {
        let verb = &resolved.verb;
        if run.dry_run && !verb.dry_run {
            return Err(Error::DryRunUnsupported(format!(
                "the verb '{}' of adapter '{}' has no dry run, so it was not run",
                verb.name, resolved.adapter
            )));
        }
        check_arguments(&verb.name, &run.argv)?;
        let call = InvokeCall {
            params: InvokeParams {
                verb: verb.name.clone(),
                argv: run.argv,
                cwd: run.cwd.display().to_string(),
                dry_run: run.dry_run,
                actor: run.actor,
                tty: false,
            },
            max_runtime: Duration::from_secs(u64::from(verb.max_runtime_secs)),
            restrict_writes: verb.tier == VerbTier::Safe,
            slot: self.next_slot.fetch_add(1, Ordering::SeqCst),
        };
        resolved.entry.invoke(call, out, cancel).await
    }

    /// The verbs of the enabled adapters, as the CLI reads them.
    pub fn verb_table(&self) -> VerbTable {
        let mut verbs = Vec::new();
        for (adapter, entry) in self.entries.read().iter() {
            if !entry.is_enabled() {
                continue;
            }
            for v in entry.verbs() {
                verbs.push(VerbTableEntry {
                    verb: v.name.clone(),
                    adapter: adapter.clone(),
                    summary: v.summary.clone(),
                    tier: tier_name(v.tier).to_string(),
                    mode: if v.mode == VerbMode::Session {
                        "session"
                    } else {
                        "relayed"
                    }
                    .to_string(),
                });
            }
        }
        verbs.sort_by(|a, b| (&a.verb, &a.adapter).cmp(&(&b.verb, &b.adapter)));
        VerbTable { verbs }
    }

    /// Write `adapters/verbs.json` after adapters changed. A table nobody needs (no `adapters`
    /// directory yet) is not created; a failure to write is a warning, never an error: the CLI
    /// then simply does not know the verbs and says so.
    pub(super) fn write_verb_table(&self) {
        let Some(ctx) = self.ctx.get() else { return };
        if !ctx.paths.adapters_dir().is_dir() {
            return;
        }
        let table = self.verb_table();
        let written = serde_json::to_vec_pretty(&table)
            .map_err(std::io::Error::other)
            .and_then(|bytes| write_private_atomic(&ctx.paths.adapter_verbs(), &bytes));
        if let Err(e) = written {
            warn!(
                "Could not write {}: {}",
                ctx.paths.adapter_verbs().display(),
                e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn one_enabled_provider_answers_for_the_verb() {
        assert_eq!(choose("check", &names(&["check"]), &[]).unwrap(), 0);
        // a disabled copy elsewhere does not matter
        assert_eq!(
            choose("check", &names(&["check"]), &names(&["other"])).unwrap(),
            0
        );
    }

    #[test]
    fn two_enabled_providers_are_refused_not_picked_from() {
        let err = choose("check", &names(&["one", "two"]), &[]).unwrap_err();
        assert!(matches!(err, Error::AdapterInvalid(_)), "{err}");
        assert!(
            err.to_string().contains("one") && err.to_string().contains("two"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_verb_names_the_adapter_that_provides_it() {
        let err = choose("deploy", &[], &[]).unwrap_err();
        assert!(matches!(err, Error::AdapterMissing(_)), "{err}");
        assert!(
            err.to_string().contains("oxidepm adapter import deploy"),
            "{err}"
        );
        let unknown = choose("nonesuch", &[], &[]).unwrap_err();
        assert!(
            unknown
                .to_string()
                .contains("no installed adapter provides"),
            "{unknown}"
        );
        let off = choose("nonesuch", &[], &names(&["x"])).unwrap_err();
        assert!(
            matches!(off, Error::AdapterUnavailable(_))
                && off.to_string().contains("adapter enable x"),
            "{off}"
        );
    }

    #[test]
    fn the_working_directory_is_checked() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(home.join("daemon")).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let daemon = home.join("daemon");
        let ok = check_cwd(project.to_str().unwrap(), &daemon, Some(&home)).unwrap();
        assert_eq!(ok, std::fs::canonicalize(&project).unwrap());
        for bad in [
            "/",
            "relative/dir",
            "",
            home.to_str().unwrap(),
            daemon.to_str().unwrap(),
            dir.path().to_str().unwrap(),
            "/nonexistent-dir-for-test",
        ] {
            assert!(
                matches!(
                    check_cwd(bad, &daemon, Some(&home)),
                    Err(Error::PolicyDenied(_))
                ),
                "{bad:?} must be refused"
            );
        }
        // a file is not a directory
        let file = project.join("f");
        std::fs::write(&file, "x").unwrap();
        assert!(check_cwd(file.to_str().unwrap(), &daemon, Some(&home)).is_err());
        // inside the home but outside the daemon's directory is a project like any other
        let inside = home.join("work");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(check_cwd(inside.to_str().unwrap(), &daemon, Some(&home)).is_ok());
    }

    #[test]
    fn a_link_to_the_home_is_refused_like_the_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert!(check_cwd(link.to_str().unwrap(), &home.join("d"), Some(&home)).is_err());
    }
}
