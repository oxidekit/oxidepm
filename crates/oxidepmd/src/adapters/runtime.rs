//! Runtime adapters: how the core gets the launch of an app from an adapter and still owns the
//! child.
//!
//! An adapter that provides a runtime returns data: the steps to run (`runtime.prepare`) and the
//! plan (`runtime.plan`). It does not execute anything and does not hold the child, the pipes or
//! the process group. The core checks every word of it (`oxidepm_runtime::step`), runs the steps
//! itself in their own process group, and spawns the plan through `spawn_group` like any other
//! app. The ready-wait and `runtime.prepare` happen before a launch slot is taken
//! ([`AdapterRunner::consult`]), so a hung adapter holds up the apps of its runtime and no
//! other; the circuit breaker of the entry makes every later call fail at once.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use oxidepm_adapter_proto::messages::{
    method, Phase, PlanParams, PlanResult, PrepareParams, PrepareResult as PrepareReply, SpecView,
    StepDone,
};
use oxidepm_adapter_proto::{from_params, to_params};
use oxidepm_core::{runtime_hints, AppSpec, Error, Result, Runtime};
use oxidepm_runtime::step::{run_step, validate_plan, validate_steps, BuildLog, CheckedStep};
use oxidepm_runtime::{
    build_env, get_runner, LaunchPlan, Origin, PrepareResult, Runner, RuntimeSource,
};
use parking_lot::Mutex;
use tracing::{info, warn};

use super::entry::AdapterEntry;
use super::host::AdapterHost;
use crate::audit::AdapterLine;

/// How long one `runtime.prepare` or `runtime.plan` call may take.
pub const RUNTIME_CALL: Duration = Duration::from_secs(10);

/// The call deadline; a test build can shorten it with `OXIDEPM_TEST_RUNTIME_CALL_MS`.
#[cfg(feature = "test-hooks")]
fn call_deadline() -> Duration {
    std::env::var("OXIDEPM_TEST_RUNTIME_CALL_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(RUNTIME_CALL, Duration::from_millis)
}

#[cfg(not(feature = "test-hooks"))]
fn call_deadline() -> Duration {
    RUNTIME_CALL
}

/// Who provides a runtime.
pub enum Provider {
    /// An enabled adapter lists it.
    Enabled(Arc<AdapterEntry>),
    /// An adapter that is installed but not enabled lists it.
    NotEnabled(String),
    None,
}

impl AdapterHost {
    /// The adapter that provides `runtime`: the enabled one if there is one (enabling refuses a
    /// second provider), else an installed one that is not enabled.
    pub fn runtime_provider(&self, runtime: &str) -> Provider {
        let entries = self.entries.read();
        let mut disabled = None;
        for entry in entries.values() {
            if !entry.runtimes().iter().any(|r| r == runtime) {
                continue;
            }
            if entry.is_enabled() {
                return Provider::Enabled(Arc::clone(entry));
            }
            disabled.get_or_insert_with(|| entry.name().to_string());
        }
        match disabled {
            Some(name) => Provider::NotEnabled(name),
            None => Provider::None,
        }
    }

    /// The runtimes the enabled adapters provide.
    pub fn enabled_runtimes(&self) -> Vec<String> {
        self.entries
            .read()
            .values()
            .filter(|e| e.is_enabled())
            .flat_map(|e| e.runtimes().iter().cloned())
            .collect()
    }

    fn audit(&self) -> Option<Arc<crate::audit::Audit>> {
        self.ctx.get().and_then(|c| c.audit.clone())
    }
}

/// The runtime source of the daemon: `cmd`, and the adapters the host has enabled. The supervisor is made before the host is loaded, so the host is bound later.
pub struct DaemonRuntimes {
    host: OnceLock<Arc<AdapterHost>>,
    /// `daemon.toml reuse_plan`: whether a stored plan may be used.
    reuse_plan: bool,
}

impl DaemonRuntimes {
    pub fn new(reuse_plan: bool) -> Arc<Self> {
        Arc::new(Self {
            host: OnceLock::new(),
            reuse_plan,
        })
    }

    /// From now on the adapters of `host` serve runtimes.
    pub fn bind(&self, host: Arc<AdapterHost>) {
        let _ = self.host.set(host);
    }

    fn provider(&self, runtime: &Runtime) -> Provider {
        match self.host.get() {
            Some(host) if !runtime.is_cmd() => host.runtime_provider(runtime.as_str()),
            _ => Provider::None,
        }
    }

    fn builtin_serves(runtime: &Runtime) -> bool {
        runtime.is_cmd() || get_runner(runtime).is_some()
    }

    /// Why nothing can serve `runtime`: the adapter that would, from the hints (names only), and
    /// what to run; or, for a name nobody knows, the runtimes that are available.
    fn missing(&self, runtime: &Runtime) -> Error {
        if let Provider::NotEnabled(adapter) = self.provider(runtime) {
            return Error::AdapterMissing(format!(
                "runtime '{runtime}' is provided by the adapter '{adapter}', which is installed but not enabled. Run: oxidepm adapter enable {adapter}"
            ));
        }
        if let Some(adapter) = runtime_hints::adapter_for_runtime(runtime.as_str()) {
            let installed = self.host.get().is_some_and(|h| h.entry(adapter).is_some());
            let run = if installed {
                format!("oxidepm adapter enable {adapter}")
            } else {
                format!("oxidepm adapter import {adapter} --enable")
            };
            return Error::AdapterMissing(format!(
                "runtime '{runtime}' is provided by the adapter '{adapter}', which is not enabled. Run: {run}"
            ));
        }
        let mut available: Vec<String> = vec!["cmd".to_string()];
        if let Some(host) = self.host.get() {
            available.extend(host.enabled_runtimes());
        }
        available.sort();
        available.dedup();
        Error::AdapterMissing(format!(
            "no adapter provides runtime '{runtime}'; installed runtimes: {}",
            available.join(", ")
        ))
    }

    /// Is there a way to launch `runtime`?
    fn serves(&self, runtime: &Runtime) -> bool {
        matches!(self.provider(runtime), Provider::Enabled(_)) || Self::builtin_serves(runtime)
    }
}

#[async_trait]
impl RuntimeSource for DaemonRuntimes {
    async fn runner_for(&self, runtime: &Runtime, origin: Origin) -> Result<Box<dyn Runner>> {
        if let (Provider::Enabled(entry), Some(host)) = (self.provider(runtime), self.host.get()) {
            if let Some(rules) = entry.runtime_rules(runtime.as_str()) {
                return Ok(Box::new(AdapterRunner {
                    host: Arc::clone(host),
                    entry: Arc::clone(&entry),
                    rules: rules.clone(),
                    runtime: runtime.clone(),
                    origin,
                    reuse_plan: self.reuse_plan,
                    state: Mutex::new(Consulted::default()),
                }));
            }
        }
        if Self::builtin_serves(runtime) {
            if let Some(runner) = get_runner(runtime) {
                return Ok(runner);
            }
        }
        Err(self.missing(runtime))
    }

    fn check(&self, spec: &AppSpec) -> Result<()> {
        if !self.serves(&spec.mode) {
            return Err(self.missing(&spec.mode));
        }
        for name in spec.adapters.keys() {
            let message = match self.host.get().and_then(|host| host.entry(name)) {
                Some(entry) if entry.is_enabled() => continue,
                Some(_) => format!(
                    "adapter '{name}' is installed but not enabled. Run: oxidepm adapter enable {name}"
                ),
                None => format!(
                    "adapter '{name}' is not enabled. Run: oxidepm adapter import {name} --enable"
                ),
            };
            return Err(Error::AdapterMissing(message));
        }
        Ok(())
    }

    fn ready(&self, runtime: &Runtime) -> Result<()> {
        match self.provider(runtime) {
            Provider::Enabled(entry) => entry.available(),
            _ if Self::builtin_serves(runtime) => Ok(()),
            _ => Err(self.missing(runtime)),
        }
    }

    async fn forget(&self, app_name: &str) {
        let Some(store) = self.host.get().and_then(|h| h.store.lock().clone()) else {
            return;
        };
        if let Err(e) = store.repo().delete_plan(app_name).await {
            warn!("Could not drop the stored launch plan of {app_name}: {e}");
        }
    }
}

/// The runner of an app whose runtime an adapter provides.
pub struct AdapterRunner {
    host: Arc<AdapterHost>,
    entry: Arc<AdapterEntry>,
    rules: oxidepm_runtime::step::RuntimeRules,
    runtime: Runtime,
    origin: Origin,
    reuse_plan: bool,
    state: Mutex<Consulted>,
}

/// What `consult` found out, for `prepare`.
#[derive(Default)]
struct Consulted {
    /// The steps the adapter returned, checked.
    steps: Vec<CheckedStep>,
    /// The stored plan of an earlier launch, when plan reuse is allowed for this launch.
    stored: Option<Stored>,
    /// `prepare` is to use the stored plan and ask nothing.
    use_stored: bool,
    /// What the person should be told about this launch.
    warnings: Vec<String>,
}

/// A stored plan that passed the reuse checks.
struct Stored {
    plan: oxidepm_runtime::step::CheckedPlan,
    had_steps: bool,
}

/// The errors that mean the adapter could not be asked (not that it said no, and not that it is
/// gone): the only ones a stored plan may stand in for.
fn is_outage(e: &Error) -> bool {
    matches!(
        e,
        Error::AdapterUnavailable(_)
            | Error::AdapterTimeout(_)
            | Error::AdapterCrashed(_)
            | Error::AdapterProtocol(_)
    )
}

/// A digest of what the adapter is shown of the app: a plan is the answer to this view, so it
/// is reused only for the same view.
fn spec_digest(view: &SpecView) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(view).unwrap_or_default();
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The redacted view of an app the adapter sees: the keys of `env` and not the values, and
/// that adapter's own options.
fn spec_view(spec: &AppSpec, adapter: &str) -> SpecView {
    let mut ports: Vec<u16> = spec.port.into_iter().collect();
    if let Some((lo, hi)) = spec.port_range {
        ports.extend((lo..=hi).take(256));
    }
    SpecView {
        name: spec.name.clone(),
        runtime: spec.mode.to_string(),
        command: spec.command.clone(),
        args: spec.args.clone(),
        cwd: spec.cwd.to_string_lossy().to_string(),
        tags: spec.tags.clone(),
        ports,
        instances: spec.instances,
        env: spec
            .env
            .keys()
            .map(|k| (k.clone(), "<redacted>".to_string()))
            .collect(),
        options: spec
            .adapters
            .get(adapter)
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default(),
    }
}

impl AdapterRunner {
    fn refused(&self, what: &str, e: &Error) {
        warn!("Adapter '{}': {what}: {e}", self.entry.name());
        if let Some(audit) = self.host.audit() {
            audit.adapter_line(AdapterLine {
                name: self.entry.name().to_string(),
                event: what.to_string(),
                version: Some(self.entry.version().to_string()),
                sha12: Some(self.entry.exe_sha256().chars().take(12).collect()),
            });
        }
    }

    /// The stored plan of this app, when this launch may use it and it still holds: same spec
    /// digest, same adapter and executable, and a plan that passes today's checks with a
    /// program that still exists.
    async fn load_stored(&self, spec: &AppSpec, digest: &str) -> Option<Stored> {
        if !self.reuse_plan || !matches!(self.origin, Origin::Resurrect | Origin::Auto) {
            return None;
        }
        let store = self.host.store.lock().clone()?;
        let row = store.repo().get_plan(&spec.name).await.ok().flatten()?;
        if row.spec_digest != digest
            || row.adapter != self.entry.name()
            || row.exe_sha256 != self.entry.exe_sha256()
        {
            return None;
        }
        let reply: PlanResult = serde_json::from_str(&row.plan_json).ok()?;
        let plan = validate_plan(&reply, spec, &self.rules).ok()?;
        // not redundant: `validate_plan` finds a bare program name on the PATH but takes an
        // absolute path as it is, and the file may have gone since the plan was made
        plan.launch.program.exists().then_some(Stored {
            plan,
            had_steps: row.had_steps,
        })
    }

    /// Remember the plan this launch ran on, for the next boot.
    async fn store_plan(
        &self,
        spec: &AppSpec,
        digest: String,
        reply: &PlanResult,
        had_steps: bool,
    ) {
        let Some(store) = self.host.store.lock().clone() else {
            return;
        };
        let row = oxidepm_db::PlanRow {
            app_name: spec.name.clone(),
            spec_digest: digest,
            adapter: self.entry.name().to_string(),
            exe_sha256: self.entry.exe_sha256().to_string(),
            had_steps,
            plan_json: serde_json::to_string(reply).unwrap_or_default(),
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = store.repo().put_plan(&row).await {
            warn!("Could not store the launch plan of {}: {e}", spec.name);
        }
    }

    /// The warning for a launch that went on a stored plan, also written to the audit log.
    fn stored_warning(&self, why: &Error, had_steps: bool) -> String {
        if let Some(audit) = self.host.audit() {
            audit.adapter_line(AdapterLine {
                name: self.entry.name().to_string(),
                event: "stored_plan_used".to_string(),
                version: Some(self.entry.version().to_string()),
                sha12: Some(self.entry.exe_sha256().chars().take(12).collect()),
            });
        }
        let steps = if had_steps {
            "; its build steps were skipped and the earlier build is assumed to be there"
        } else {
            ""
        };
        format!(
            "the runtime adapter '{}' could not be asked ({}); started from the launch plan it made earlier{steps}",
            self.entry.name(),
            why.code().as_str()
        )
    }

    async fn ask<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R> {
        let params = to_params(params).map_err(|e| Error::Internal(format!("{method}: {e}")))?;
        let reply = self.entry.call(method, &params, call_deadline()).await?;
        from_params::<R>(&reply).map_err(|e| {
            Error::AdapterProtocol(format!(
                "adapter '{}': the reply to {method} has the wrong shape: {e}",
                self.entry.name()
            ))
        })
    }
}

#[async_trait]
impl Runner for AdapterRunner {
    /// Wait for the adapter and ask `runtime.prepare`, before a launch slot is taken. The steps
    /// are checked here, so an adapter that asks for a program the manifest does not list fails
    /// the launch before anything runs.
    ///
    /// A resurrect or an automatic launch may stand on the plan stored by an earlier launch. It is used at once, without asking, when that plan had no build steps; or when the adapter
    /// cannot be asked. A person's start or restart never does.
    async fn consult(&self, spec: &AppSpec) -> Result<()> {
        let view = spec_view(spec, self.entry.name());
        let stored = self.load_stored(spec, &spec_digest(&view)).await;
        if let Some(stored) = stored.as_ref().filter(|s| !s.had_steps) {
            info!(
                "{} starts from its stored launch plan; the adapter is not asked",
                spec.name
            );
            let mut state = self.state.lock();
            state.use_stored = true;
            state.stored = Some(Stored {
                plan: stored.plan.clone(),
                had_steps: false,
            });
            return Ok(());
        }
        // The adapter runs in its own working directory. Resolve relative PATH entries against
        // the app's directory before passing the lookup hint.
        let path_hint = build_env(spec, &[])
            .get("PATH")
            .map(|path| {
                oxidepm_runtime::resolve::absolute_search_path(&path, &spec.cwd)
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        let params = PrepareParams {
            spec: view,
            path_hint,
            phase: if self.origin == Origin::Restart {
                Phase::Restart
            } else {
                Phase::Start
            },
        };
        let reply: PrepareReply = match self.ask(method::RUNTIME_PREPARE, &params).await {
            Ok(reply) => reply,
            Err(e) if is_outage(&e) && stored.is_some() => {
                let stored = stored.expect("checked");
                let warning = self.stored_warning(&e, stored.had_steps);
                let mut state = self.state.lock();
                state.warnings.push(warning);
                state.use_stored = true;
                state.stored = Some(stored);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match validate_steps(&reply.steps, spec, &self.rules) {
            Ok(steps) => {
                let mut state = self.state.lock();
                state.steps = steps;
                state.stored = stored;
                Ok(())
            }
            Err(e) => {
                self.refused("steps_refused", &e);
                Err(e)
            }
        }
    }

    /// Run the steps in the core, then ask `runtime.plan` and check the plan. Called while the
    /// launch slot is held.
    async fn prepare(&self, spec: &AppSpec) -> Result<PrepareResult> {
        let (steps, stored, use_stored, mut warnings) = {
            let mut state = self.state.lock();
            (
                std::mem::take(&mut state.steps),
                state.stored.take(),
                state.use_stored,
                std::mem::take(&mut state.warnings),
            )
        };
        if use_stored {
            let stored = stored.expect("a stored plan was chosen");
            let mut result = PrepareResult::success("");
            result.binary_path = stored.plan.binary_path;
            result.plan = Some(stored.plan.launch);
            result.warnings = warnings;
            return Ok(result);
        }
        let had_steps = !steps.is_empty();
        let log_path = self
            .host
            .ctx
            .get()
            .map(|c| c.paths.logs_dir().join(format!("{}-build.log", spec.name)));
        let mut log = log_path
            .and_then(|p| BuildLog::create(&p).ok())
            .unwrap_or_else(BuildLog::none);
        let mut done = Vec::with_capacity(steps.len());
        for step in &steps {
            info!("Step '{}' of {}: {}", step.id, spec.name, step.description);
            let out = run_step(step, &mut log).await?;
            if !out.success() && !step.optional {
                let why = if out.timed_out {
                    format!("was killed after {} s", step.timeout.as_secs())
                } else {
                    match out.exit_code {
                        Some(code) => format!("exited with {code}"),
                        None => "was killed by a signal".to_string(),
                    }
                };
                return Ok(PrepareResult::failure(format!(
                    "step '{}' ({}) {why}:\n{}",
                    step.id, step.description, out.tail
                )));
            }
            done.push(StepDone {
                id: out.id,
                exit_code: out.exit_code.unwrap_or(-1),
                stdout: out.stdout,
                stderr: out.stderr,
            });
        }

        let view = spec_view(spec, self.entry.name());
        let digest = spec_digest(&view);
        let params = PlanParams {
            spec: view,
            steps_done: done,
        };
        let reply: PlanResult = match self.ask(method::RUNTIME_PLAN, &params).await {
            Ok(reply) => reply,
            // the steps ran and the adapter then could not be asked: the stored plan is the same
            // program the same adapter chose earlier
            Err(e) if is_outage(&e) && stored.is_some() => {
                let stored = stored.expect("checked");
                warnings.push(self.stored_warning(&e, false));
                let mut result = PrepareResult::success("");
                result.binary_path = stored.plan.binary_path;
                result.plan = Some(stored.plan.launch);
                result.warnings = warnings;
                return Ok(result);
            }
            Err(e) => return Err(e),
        };
        let checked = match validate_plan(&reply, spec, &self.rules) {
            Ok(checked) => checked,
            Err(e) => {
                self.refused("plan_refused", &e);
                return Err(e);
            }
        };
        if let Some(audit) = self.host.audit() {
            audit.plan_line(
                None,
                &checked.launch.program.to_string_lossy(),
                &checked.launch.args,
            );
        }
        self.store_plan(spec, digest, &reply, had_steps).await;
        let mut result = PrepareResult::success("");
        result.binary_path = checked.binary_path;
        result.plan = Some(checked.launch);
        result.warnings = warnings;
        Ok(result)
    }

    /// The plan comes from `prepare` for a runtime adapter.
    fn plan(&self, _spec: &AppSpec) -> Result<LaunchPlan> {
        Err(Error::Internal(format!(
            "the plan of runtime '{}' comes from its adapter",
            self.runtime
        )))
    }

    fn command_string(&self, spec: &AppSpec) -> String {
        format!("{} {}", spec.command, spec.args.join(" "))
    }

    fn mode_name(&self) -> &'static str {
        "adapter"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_adapter_that_could_not_be_asked_lets_a_stored_plan_stand_in() {
        let s = || "x".to_string();
        for e in [
            Error::AdapterUnavailable(s()),
            Error::AdapterTimeout(s()),
            Error::AdapterCrashed(s()),
            Error::AdapterProtocol(s()),
        ] {
            assert!(is_outage(&e), "{e:?}");
        }
        // a removed adapter is a decision, a tampered one is an attack, an invalid answer is a
        // refusal, a busy one is not down
        for e in [
            Error::AdapterMissing(s()),
            Error::AdapterTampered(s()),
            Error::AdapterInvalid(s()),
            Error::AdapterBusy(s()),
            Error::BuildFailed(s()),
            Error::Internal(s()),
        ] {
            assert!(!is_outage(&e), "{e:?}");
        }
    }
}
