//! `depends_on`: the checks before a start and the wait for the dependencies.
//!
//! The wait is event driven: it subscribes to the snapshot of every dependency and looks again
//! when one of them publishes, with one deadline for the whole wait. It runs inside the
//! actor's pipeline future, so it holds no lock and a stop, a delete or a shutdown cancels it by
//! dropping the future. It fails closed: an errored or vanished dependency fails the dependent
//! at once, and so does the timeout, unless the spec asks for `depends_fail_open`.

use super::handle::HandleKind;
use super::registry::Registry;
use oxidepm_core::graph::{self, Node};
use oxidepm_core::{AppSpec, AppStatus, Error, Result};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tokio::time::Instant;
use tracing::{info, warn};

/// What the dependencies of an app say right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Every dependency is running.
    Ready,
    /// This dependency is not running yet but may get there.
    Wait(String),
    /// A dependency errored or is gone: the dependent cannot start.
    Failed(String),
}

/// Judge the states of a spec's dependencies (`None`: not in the registry). A dependency that
/// is waiting, building, starting, unhealthy, backing off or stopped is awaited (it may be
/// about to be started by the same batch); an errored or vanished one fails at once, whatever
/// the others are doing.
pub fn judge(deps: &[(String, Option<AppStatus>)]) -> Verdict {
    let mut waiting: Option<&String> = None;
    for (name, status) in deps {
        match status {
            None => return Verdict::Failed(format!("dependency '{}' no longer exists", name)),
            Some(AppStatus::Errored) => {
                return Verdict::Failed(format!("dependency '{}' errored", name))
            }
            Some(status) if status.is_ready() => {}
            Some(_) => {
                waiting.get_or_insert(name);
            }
        }
    }
    match waiting {
        Some(name) => Verdict::Wait(name.clone()),
        None => Verdict::Ready,
    }
}

/// The verdict on `spec`'s dependencies at this instant.
pub fn check_now(registry: &Registry, spec: &AppSpec) -> Verdict {
    let states: Vec<(String, Option<AppStatus>)> = spec
        .depends_on
        .iter()
        .map(|dep| {
            (
                dep.clone(),
                registry.by_name(dep).map(|h| h.snapshot().state.status),
            )
        })
        .collect();
    judge(&states)
}

/// Wait until every dependency of `spec` is running.
///
/// `Ok(true)`: they are. `Ok(false)`: the deadline passed and the spec is `depends_fail_open`
/// (a warning was logged; the caller launches anyway). Errors: `DependencyFailed` for an
/// errored or vanished dependency, `DependencyTimeout` when the deadline passes.
pub async fn wait_ready(registry: &Registry, spec: &AppSpec, deadline: Instant) -> Result<bool> {
    if spec.depends_on.is_empty() {
        return Ok(true);
    }
    info!(
        "Waiting for dependencies of {}: {:?}",
        spec.name, spec.depends_on
    );
    loop {
        // Look the handles up again on every turn: a dependency whose actor was replaced
        // (a panic) is a new handle with a new snapshot channel. `borrow_and_update` reads the
        // state and marks it seen in one step, so a publish after this read wakes the wait.
        let mut receivers = Vec::new();
        let mut states = Vec::new();
        for dep in &spec.depends_on {
            match registry.by_name(dep) {
                Some(handle) => {
                    let mut rx = handle.subscribe();
                    let status = rx.borrow_and_update().state.status;
                    states.push((dep.clone(), Some(status)));
                    receivers.push(rx);
                }
                None => states.push((dep.clone(), None)),
            }
        }
        let blocking = match judge(&states) {
            Verdict::Ready => {
                info!("Dependencies of {} are running", spec.name);
                return Ok(true);
            }
            Verdict::Failed(message) => return Err(Error::DependencyFailed(message)),
            Verdict::Wait(name) => name,
        };

        let changes =
            futures::future::select_all(receivers.iter_mut().map(|rx| Box::pin(rx.changed())));
        tokio::select! {
            (result, _, _) = changes => {
                // An error is a dropped snapshot channel (its actor ended): look again, but not
                // in a spin if the registry still holds that handle.
                if result.is_err() {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            _ = tokio::time::sleep_until(deadline) => {
                let message = format!(
                    "dependency '{}' not running after {}s",
                    blocking, spec.depends_timeout_secs
                );
                if spec.depends_fail_open {
                    warn!("{}; starting '{}' anyway (depends_fail_open)", message, spec.name);
                    return Ok(false);
                }
                return Err(Error::DependencyTimeout(message));
            }
        }
    }
}

/// Is the app registered under `name` already up, so that a `Start` of it has nothing to
/// start? A single app is up while its status is active (waiting, building, starting, running,
/// unhealthy, backoff or stopping); a cluster while every instance is.
pub fn already_up(registry: &Registry, name: &str) -> bool {
    let Some(handle) = registry.by_name(name) else {
        return false;
    };
    let snap = handle.snapshot();
    match handle.kind {
        HandleKind::Process => snap.state.status.is_active(),
        HandleKind::Group => {
            !snap.children.is_empty()
                && snap.children.iter().all(|id| {
                    registry
                        .get(*id)
                        .is_some_and(|child| child.snapshot().state.status.is_active())
                })
        }
    }
}

/// The checks of a start request, before anything exists. Every dependency not in `in_set`
/// (the other specs of the same batch, which are about to start) must be registered and not at
/// rest: a missing one is `DependencyMissing`, a stopped or errored one is `DependencyFailed`
/// at once instead of a wait that cannot end well.
///
/// An app that is already up (active, or for a cluster every instance active) is not checked: a
/// start of it changes nothing and is answered `E_APP_EXISTS` whatever its dependencies are doing.
pub fn preflight(registry: &Registry, spec: &AppSpec, in_set: &HashSet<&str>) -> Result<()> {
    if already_up(registry, &spec.name) {
        return Ok(());
    }
    for dep in &spec.depends_on {
        if in_set.contains(dep.as_str()) {
            continue;
        }
        let Some(handle) = registry.by_name(dep) else {
            return Err(Error::DependencyMissing(format!(
                "app '{}' depends on '{}', which does not exist",
                spec.name, dep
            )));
        };
        match handle.snapshot().state.status {
            AppStatus::Stopped => {
                return Err(Error::DependencyFailed(format!(
                    "dependency '{}' is stopped; start it first",
                    dep
                )))
            }
            AppStatus::Errored => {
                return Err(Error::DependencyFailed(format!(
                    "dependency '{}' is errored; fix it and start it first",
                    dep
                )))
            }
            _ => {}
        }
    }
    Ok(())
}

/// The names that sit on a dependency cycle among `nodes`, with the text that names the first
/// cycle each belongs to. Edges to names that are not nodes are ignored.
pub fn cycle_members(nodes: &[Node]) -> HashMap<String, String> {
    let mut members = HashMap::new();
    let mut rest: Vec<Node> = nodes.to_vec();
    while let Some(cycle) = graph::find_cycle(&rest) {
        let text = format!("dependency cycle: {}", cycle.join(" -> "));
        for name in &cycle {
            members.insert(name.clone(), text.clone());
        }
        rest.retain(|(name, _)| !cycle.contains(name));
    }
    members
}

/// The apps of `specs` that sit on a dependency cycle once the specs are put into the
/// registry's graph (the registered apps' declared dependencies, with `specs` replacing the
/// entries of the same names). Only what the specs can reach counts. The values are the text
/// naming the cycle.
pub fn cycles_through_registry(registry: &Registry, specs: &[&AppSpec]) -> HashMap<String, String> {
    let mut nodes: HashMap<String, Vec<String>> = registry
        .list()
        .iter()
        .map(|h| {
            let snap = h.snapshot();
            (snap.spec.name.clone(), snap.spec.depends_on.clone())
        })
        .collect();
    for spec in specs {
        nodes.insert(spec.name.clone(), spec.depends_on.clone());
    }
    // what the specs can reach
    let mut reach: Vec<Node> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut todo: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
    while let Some(name) = todo.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(deps) = nodes.get(&name) {
            todo.extend(deps.iter().cloned());
            reach.push((name, deps.clone()));
        }
    }
    let mut members = cycle_members(&reach);
    let wanted: HashSet<&str> = specs.iter().map(|s| s.name.as_str()).collect();
    members.retain(|name, _| wanted.contains(name.as_str()));
    members
}

/// The apps of a set that cannot start, and why: an unknown, stopped or
/// errored dependency outside the set, a cycle, and every app that depends on one of those
/// (it would only wait for something that is never started). `failed` holds the apps that
/// already failed (admission), so their dependents fail too.
pub fn reject_set(
    registry: &Registry,
    specs: &[&AppSpec],
    in_set: &HashSet<&str>,
    mut failed: HashMap<String, Error>,
) -> HashMap<String, Error> {
    // a cycle first: it is the better explanation than "stopped" for an app on one
    for (name, text) in cycles_through_registry(registry, specs) {
        failed.entry(name).or_insert(Error::DependencyCycle(text));
    }
    for spec in specs {
        if failed.contains_key(&spec.name) {
            continue;
        }
        if let Err(e) = preflight(registry, spec, in_set) {
            failed.insert(spec.name.clone(), e);
        }
    }
    // dependents of the failed, until nothing changes
    loop {
        let mut grew = false;
        for spec in specs {
            if failed.contains_key(&spec.name) || already_up(registry, &spec.name) {
                continue;
            }
            if let Some(dep) = spec
                .depends_on
                .iter()
                .find(|d| failed.contains_key(d.as_str()))
            {
                let why = failed[dep.as_str()].to_string();
                failed.insert(
                    spec.name.clone(),
                    Error::DependencyFailed(format!("dependency '{}' cannot start: {}", dep, why)),
                );
                grew = true;
            }
        }
        if !grew {
            return failed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::handle::{HandleKind, HandleParts, ProcessHandle, Snapshot};
    use super::*;
    use oxidepm_core::{Desired, Runtime};
    use std::sync::Arc;

    fn spec(name: &str, deps: &[&str]) -> AppSpec {
        let mut spec = AppSpec::new(
            name.to_string(),
            Runtime::CMD,
            "true".to_string(),
            "/".into(),
        );
        spec.depends_on = deps.iter().map(|d| d.to_string()).collect();
        spec
    }

    /// A handle with no actor behind it, and the sender that publishes its snapshots.
    fn fake(id: u32, spec: &AppSpec, status: AppStatus) -> (ProcessHandle, HandleParts) {
        let mut spec = spec.clone();
        spec.id = id;
        let mut snap = Snapshot::new(Arc::new(spec), Desired::Running, None);
        snap.state.status = status;
        ProcessHandle::channel(id, HandleKind::Process, snap)
    }

    fn publish(parts: &HandleParts, status: AppStatus) {
        let mut snap = (**parts.snap_tx.borrow()).clone();
        snap.state.status = status;
        parts.snap_tx.send_replace(Arc::new(snap));
    }

    fn deps(list: &[(&str, Option<AppStatus>)]) -> Vec<(String, Option<AppStatus>)> {
        list.iter().map(|(n, s)| (n.to_string(), *s)).collect()
    }

    #[test]
    fn judge_awaits_the_states_that_can_still_become_running() {
        use AppStatus::*;
        assert_eq!(judge(&[]), Verdict::Ready);
        assert_eq!(judge(&deps(&[("db", Some(Running))])), Verdict::Ready);
        for status in [
            Waiting, Building, Starting, Unhealthy, Backoff, Stopped, Stopping,
        ] {
            assert_eq!(
                judge(&deps(&[("db", Some(Running)), ("cache", Some(status))])),
                Verdict::Wait("cache".into()),
                "{status:?}"
            );
        }
    }

    #[test]
    fn judge_fails_at_once_for_errored_or_gone_even_when_another_is_waiting() {
        use AppStatus::*;
        assert_eq!(
            judge(&deps(&[("a", Some(Starting)), ("b", Some(Errored))])),
            Verdict::Failed("dependency 'b' errored".into())
        );
        assert_eq!(
            judge(&deps(&[("a", Some(Starting)), ("b", None)])),
            Verdict::Failed("dependency 'b' no longer exists".into())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deps_wait_is_event_driven_not_polling() {
        let registry = Registry::new();
        let db = spec("db", &[]);
        let (handle, parts) = fake(1, &db, AppStatus::Starting);
        registry.insert("db", handle, None).unwrap();
        let web = spec("web", &["db"]);

        let started = Instant::now();
        let deadline = started + Duration::from_secs(60);
        let reg = Arc::clone(&registry);
        let waiter = tokio::spawn(async move { wait_ready(&reg, &web, deadline).await });
        // the wait is parked; with the clock paused nothing moves it on
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());

        publish(&parts, AppStatus::Running);
        assert!(waiter.await.unwrap().unwrap());
        // a poll loop would have slept first; an event wakes the wait with no time passing
        assert!(
            started.elapsed() < Duration::from_millis(10),
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deps_wait_times_out_closed_and_open() {
        let registry = Registry::new();
        let (handle, _parts) = fake(1, &spec("db", &[]), AppStatus::Starting);
        registry.insert("db", handle, None).unwrap();

        let mut web = spec("web", &["db"]);
        web.depends_timeout_secs = 5;
        let started = Instant::now();
        let err = wait_ready(&registry, &web, started + Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::DependencyTimeout(m) if m == "dependency 'db' not running after 5s"),
            "{err}"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(5));

        web.depends_fail_open = true;
        let open = wait_ready(&registry, &web, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            !open,
            "fail-open reports that the dependencies are not running"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deps_wait_fails_when_a_dependency_errors_or_disappears() {
        let registry = Registry::new();
        let (handle, parts) = fake(1, &spec("db", &[]), AppStatus::Starting);
        registry.insert("db", handle, None).unwrap();
        let web = spec("web", &["db"]);
        let deadline = Instant::now() + Duration::from_secs(60);

        let reg = Arc::clone(&registry);
        let w = web.clone();
        let waiter = tokio::spawn(async move { wait_ready(&reg, &w, deadline).await });
        tokio::task::yield_now().await;
        publish(&parts, AppStatus::Errored);
        let err = waiter.await.unwrap().unwrap_err();
        assert!(
            matches!(&err, Error::DependencyFailed(m) if m == "dependency 'db' errored"),
            "{err}"
        );

        publish(&parts, AppStatus::Starting);
        let reg = Arc::clone(&registry);
        let waiter = tokio::spawn(async move { wait_ready(&reg, &web, deadline).await });
        tokio::task::yield_now().await;
        registry.remove(1);
        drop(parts);
        let err = waiter.await.unwrap().unwrap_err();
        assert!(
            matches!(&err, Error::DependencyFailed(m) if m.contains("no longer exists")),
            "{err}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cluster_dependency_wakes_on_group_snapshot() {
        // the group publishes a derived status like any other handle: starting, then running
        let registry = Registry::new();
        let cluster = spec("web", &[]);
        let mut snap = Snapshot::new(Arc::new(cluster), Desired::Running, None);
        snap.state.status = AppStatus::Starting;
        let (handle, parts) = ProcessHandle::channel(7, HandleKind::Group, snap);
        registry.insert("web", handle, None).unwrap();

        let api = spec("api", &["web"]);
        let reg = Arc::clone(&registry);
        let deadline = Instant::now() + Duration::from_secs(60);
        let started = Instant::now();
        let waiter = tokio::spawn(async move { wait_ready(&reg, &api, deadline).await });
        tokio::task::yield_now().await;
        publish(&parts, AppStatus::Running);
        assert!(waiter.await.unwrap().unwrap());
        assert!(started.elapsed() < Duration::from_millis(10));
    }

    #[test]
    fn preflight_rejects_missing_stopped_and_errored_dependencies() {
        let registry = Registry::new();
        let (db, _p1) = fake(1, &spec("db", &[]), AppStatus::Stopped);
        let (bad, _p2) = fake(2, &spec("bad", &[]), AppStatus::Errored);
        let (up, _p3) = fake(3, &spec("up", &[]), AppStatus::Running);
        registry.insert("db", db, None).unwrap();
        registry.insert("bad", bad, None).unwrap();
        registry.insert("up", up, None).unwrap();
        let none = HashSet::new();

        let err = preflight(&registry, &spec("w", &["nope"]), &none).unwrap_err();
        assert!(matches!(err, Error::DependencyMissing(_)), "{err}");
        let err = preflight(&registry, &spec("w", &["db"]), &none).unwrap_err();
        assert!(
            matches!(&err, Error::DependencyFailed(m) if m == "dependency 'db' is stopped; start it first"),
            "{err}"
        );
        let err = preflight(&registry, &spec("w", &["bad"]), &none).unwrap_err();
        assert!(matches!(err, Error::DependencyFailed(_)), "{err}");
        preflight(&registry, &spec("w", &["up"]), &none).unwrap();
        // inside the same batch a stopped or unknown dependency is about to start
        let batch: HashSet<&str> = ["db", "later"].into_iter().collect();
        preflight(&registry, &spec("w", &["db", "later"]), &batch).unwrap();
    }

    #[test]
    fn cycle_members_names_every_app_on_a_cycle_and_no_other() {
        let nodes: Vec<Node> = vec![
            ("a".into(), vec!["b".into()]),
            ("b".into(), vec!["a".into()]),
            ("c".into(), vec!["a".into()]),
            ("x".into(), vec!["y".into()]),
            ("y".into(), vec!["z".into()]),
            ("z".into(), vec!["x".into()]),
            ("ok".into(), vec![]),
        ];
        let members = cycle_members(&nodes);
        let mut names: Vec<&String> = members.keys().collect();
        names.sort();
        assert_eq!(names, ["a", "b", "x", "y", "z"]);
        assert!(members["a"].contains("a -> b -> a"), "{}", members["a"]);
    }

    #[test]
    fn a_new_spec_that_closes_a_cycle_through_the_registry_is_found() {
        let registry = Registry::new();
        let (a, _pa) = fake(1, &spec("a", &["b"]), AppStatus::Running);
        let (b, _pb) = fake(2, &spec("b", &[]), AppStatus::Stopped);
        registry.insert("a", a, None).unwrap();
        registry.insert("b", b, None).unwrap();
        // b is restarted with a spec that depends on a, which depends on b
        let new_b = spec("b", &["a"]);
        let cycles = cycles_through_registry(&registry, &[&new_b]);
        assert!(cycles["b"].contains("dependency cycle"), "{cycles:?}");
        // an unrelated new app is fine
        assert!(cycles_through_registry(&registry, &[&spec("c", &["a"])]).is_empty());
    }
}
