//! The group actor: the parent of a cluster.
//!
//! A cluster is a group app (the parent, no process) plus one ordinary app per instance. The
//! group takes the same commands as a process actor and fans them out to its children, and it
//! publishes a status derived from its children's snapshots every time one of them changes, so
//! a waiter on a cluster dependency wakes when the derived status flips.
//!
//! Starting a cluster that this very call created is all or nothing: if an instance cannot
//! launch, the cluster is deleted (one transaction, then the actors exit) and the instance's
//! error comes back. Starting a cluster that exists starts the instances that are at rest,
//! counts the ones that are up as done and leaves everything alone when an instance fails.
//! Deleting removes the parent's row (its instances' rows go with it) before any actor exits.

use super::actor::{AbortOnDrop, ActorCtx};
use super::handle::{
    AppId, CmdCtx, Command, HandleKind, HandleParts, ProcessHandle, RestartCause, RestartKind,
    RestartOutcome, Snapshot, StartOrigin, StartOutcome, StopOutcome,
};
use super::instance_count_message;
use super::stats::{Counted, Kind};
use oxidepm_core::{AppSpec, AppStatus, Desired, Error, Result, RunState};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tracing::{info, warn};

/// What a group is made of.
pub struct GroupInit {
    pub spec: Arc<AppSpec>,
    pub desired: Desired,
    pub children: Vec<AppId>,
}

/// The port of cluster instance `index`: `start + index` of the range if there is one, else
/// the base port plus the index. `None` if the spec has no port, or (never for a validated
/// spec, which has room for every instance) if the index does not fit.
pub fn instance_port(spec: &AppSpec, index: u32) -> Option<u16> {
    if let Some((start, end)) = spec.port_range {
        let port = u32::from(start).checked_add(index)?;
        return (port <= u32::from(end)).then_some(port as u16);
    }
    let port = u32::from(spec.port?).checked_add(index)?;
    u16::try_from(port).ok()
}

/// Derive the parent's status from its children's: all running is running, all
/// stopped is stopped, all errored is errored; any stopping child makes it stopping; children
/// on their way up (and none failed) make it starting; anything else is a degraded cluster,
/// shown as unhealthy.
pub fn derive_status(children: &[AppStatus]) -> AppStatus {
    if children.is_empty() {
        return AppStatus::Stopped;
    }
    let all = |status: AppStatus| children.iter().all(|s| *s == status);
    if all(AppStatus::Running) {
        return AppStatus::Running;
    }
    if all(AppStatus::Stopped) {
        return AppStatus::Stopped;
    }
    if all(AppStatus::Errored) {
        return AppStatus::Errored;
    }
    if children.contains(&AppStatus::Stopping) {
        return AppStatus::Stopping;
    }
    let failed = children.iter().any(|s| {
        matches!(
            s,
            AppStatus::Errored | AppStatus::Stopped | AppStatus::Unhealthy | AppStatus::Backoff
        )
    });
    let coming_up = children.iter().any(|s| {
        matches!(
            s,
            AppStatus::Waiting | AppStatus::Building | AppStatus::Starting
        )
    });
    if coming_up && !failed {
        AppStatus::Starting
    } else {
        AppStatus::Unhealthy
    }
}

pub struct GroupActor {
    id: AppId,
    spec: Arc<AppSpec>,
    desired: Desired,
    children: Vec<AppId>,
    rx: mpsc::Receiver<Command>,
    snap_tx: watch::Sender<Arc<Snapshot>>,
    kill: tokio_util::sync::CancellationToken,
    closed: Arc<std::sync::atomic::AtomicBool>,
    ctx: Arc<ActorCtx>,
    changed: Arc<Notify>,
    _watchers: Vec<AbortOnDrop>,
    _alive: Counted,
}

/// Create a group actor over children that are already registered, spawn it and return its
/// handle.
pub fn spawn_group_actor(ctx: &Arc<ActorCtx>, init: GroupInit) -> ProcessHandle {
    let GroupInit {
        spec,
        desired,
        children,
    } = init;
    let id = spec.id;
    let mut initial = Snapshot::new(Arc::clone(&spec), desired, None);
    initial.children = children.clone();
    let (mut handle, parts) = ProcessHandle::channel(id, HandleKind::Group, initial);
    let HandleParts {
        rx,
        snap_tx,
        kill,
        closed,
        ..
    } = parts;

    // one forwarder per child: any change of a child's snapshot wakes the group
    let changed = Arc::new(Notify::new());
    let mut watchers = Vec::new();
    if let Some(registry) = ctx.registry() {
        for child in children.iter().filter_map(|id| registry.get(*id)) {
            let id = child.id;
            let mut snapshots = child.subscribe();
            let changed = Arc::clone(&changed);
            let registry = Arc::downgrade(&registry);
            watchers.push(AbortOnDrop::new(tokio::spawn(async move {
                loop {
                    while snapshots.changed().await.is_ok() {
                        changed.notify_one();
                    }
                    changed.notify_one();
                    loop {
                        let Some(current) = registry.upgrade() else {
                            return;
                        };
                        let Some(child) = current.get(id) else { return };
                        let next = child.subscribe();
                        if !snapshots.same_channel(&next) {
                            snapshots = next;
                            changed.notify_one();
                            break;
                        }
                        if current.is_closed() {
                            return;
                        }
                        drop(current);
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
            })));
        }
    }

    let mut actor = GroupActor {
        id,
        spec,
        desired,
        children,
        rx,
        snap_tx,
        kill,
        closed,
        ctx: Arc::clone(ctx),
        changed,
        _watchers: watchers,
        _alive: Counted::new(&ctx.stats, Kind::Actor),
    };
    actor.publish();
    let join = tokio::spawn(actor.run());
    handle.attach_task(join.abort_handle());
    handle
}

impl GroupActor {
    async fn run(mut self) {
        loop {
            let exit = tokio::select! {
                biased;
                cmd = self.rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => true,
                },
                _ = self.changed.notified() => false,
                _ = self.kill.cancelled() => true,
            };
            self.publish();
            if exit {
                break;
            }
        }
        self.rx.close();
        let shutting_down = self.closed.load(std::sync::atomic::Ordering::SeqCst);
        while let Ok(cmd) = self.rx.try_recv() {
            let err = || {
                if shutting_down {
                    Error::ShuttingDown
                } else {
                    Error::AppNotFound(self.spec.name.clone())
                }
            };
            match cmd {
                Command::Start { reply, .. } => {
                    let _ = reply.send(Err(err()));
                }
                Command::Stop { reply, .. } | Command::Delete { reply, .. } => {
                    let _ = reply.send(Err(err()));
                }
                Command::Restart { reply, .. } => {
                    let _ = reply.send(Err(err()));
                }
                Command::Shutdown { reply, .. } => {
                    let _ = reply.send(Ok(false));
                }
                Command::FlushLogs { reply } => {
                    let _ = reply.send(Err(err()));
                }
                _ => {}
            }
        }
    }

    fn child_handles(&self) -> Vec<ProcessHandle> {
        match self.ctx.registry() {
            Some(registry) => self
                .children
                .iter()
                .filter_map(|id| registry.get(*id))
                .collect(),
            None => Vec::new(),
        }
    }

    fn derived(&self) -> (RunState, usize) {
        let handles = self.child_handles();
        let statuses: Vec<AppStatus> = handles.iter().map(|h| h.snapshot().state.status).collect();
        let mut state = RunState::new(self.id);
        state.status = derive_status(&statuses);
        state.port = self.spec.port;
        state.started_at = handles
            .iter()
            .filter_map(|h| h.snapshot().state.started_at)
            .min();
        state.restarts = handles.iter().map(|h| h.snapshot().state.restarts).sum();
        state.healthy = state.status == AppStatus::Running;
        (state, handles.len())
    }

    fn publish(&mut self) {
        let (state, _) = self.derived();
        let snapshot = Snapshot {
            spec: Arc::clone(&self.spec),
            state,
            desired: self.desired,
            parent_id: None,
            children: self.children.clone(),
            generation: 0,
        };
        self.snap_tx.send_replace(Arc::new(snapshot));
    }

    /// True if the actor is done.
    async fn handle(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::Start {
                spec,
                origin,
                created,
                ctx,
                reply,
            } => {
                let (result, exit) = self.on_start(spec, origin, created, ctx).await;
                let _ = reply.send(result);
                exit
            }
            Command::Stop { ctx, reply } => {
                let result = self.on_stop(ctx).await;
                let _ = reply.send(result);
                false
            }
            Command::Restart { cause, ctx, reply } => {
                let result = self.on_restart(cause, ctx).await;
                let _ = reply.send(result);
                false
            }
            Command::Delete {
                row_removed,
                ctx,
                reply,
            } => self.on_delete(row_removed, ctx, reply).await,
            Command::FlushLogs { reply } => {
                let flushes = self.child_handles().into_iter().map(|child| async move {
                    child.request(|reply| Command::FlushLogs { reply }).await
                });
                let result = futures::future::join_all(flushes)
                    .await
                    .into_iter()
                    .collect::<Result<Vec<()>>>()
                    .map(|_| ());
                let _ = reply.send(result);
                false
            }
            Command::Shutdown { reply, .. } => {
                // the children get their own Shutdown; the group just closes
                self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = reply.send(Ok(false));
                true
            }
            // commands for a process; a group has none
            Command::HealthOk { .. }
            | Command::HealthFailed { .. }
            | Command::CronRestart { .. }
            | Command::LimitExceeded { .. } => false,
            #[cfg(feature = "test-hooks")]
            Command::TestPanic => panic!("test panic requested"),
        }
    }

    /// The answer of a start that changed nothing.
    fn outcome(&self, changed: bool) -> StartOutcome {
        StartOutcome {
            id: self.id,
            name: self.spec.name.clone(),
            status: self.derived().0.status,
            child_ids: self.children.clone(),
            changed,
        }
    }

    /// Start the cluster. The second value says the actor is done (a new cluster that was
    /// rolled back).
    async fn on_start(
        &mut self,
        spec: Arc<AppSpec>,
        origin: StartOrigin,
        created: bool,
        ctx: CmdCtx,
    ) -> (Result<StartOutcome>, bool) {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return (Err(Error::ShuttingDown), false);
        }
        // Resurrect starts a cluster only if it is at rest and still meant to run.
        if origin == StartOrigin::Resurrect
            && (self.desired != Desired::Running || self.derived().0.status.is_active())
        {
            return (Ok(self.outcome(false)), false);
        }
        // the number of instances is the cluster's for good
        if !created && spec.instances != self.spec.instances {
            let message = instance_count_message(&self.spec.name, self.spec.instances);
            return (Err(Error::InvalidSpec(message)), false);
        }
        let handles = self.child_handles();
        // A new cluster starts every instance. One that exists starts the instances that are at
        // rest; the ones already up are done, and if all of them are the start has nothing to do.
        let targets: Vec<(usize, &ProcessHandle)> = handles
            .iter()
            .enumerate()
            .filter(|(_, child)| created || child.snapshot().state.status.is_terminal())
            .collect();
        if targets.is_empty() {
            return (Err(Error::AppAlreadyExists(self.spec.name.clone())), false);
        }

        let mut spec = (*spec).clone();
        spec.id = self.id;
        if !created {
            spec.created_at = self.spec.created_at;
            if let Err(e) = self.ctx.store.update_spec(self.id, &spec).await {
                return (Err(e), false);
            }
            // a new cluster's row is `stopped` until every instance has launched
            if let Err(e) = self.ctx.store.set_desired(self.id, Desired::Running).await {
                return (Err(e), false);
            }
            self.desired = Desired::Running;
        }
        let spec = Arc::new(spec);
        self.spec = Arc::clone(&spec);

        let instance_ctx = ctx.for_instance();
        let starts = targets.iter().map(|(index, child)| {
            let child_spec =
                Arc::new(spec.for_instance(*index as u32, instance_port(&spec, *index as u32)));
            let name = child_spec.name.clone();
            let instance_ctx = instance_ctx.clone();
            let request = child.request(move |reply| Command::Start {
                spec: child_spec,
                origin: StartOrigin::Group,
                created,
                ctx: instance_ctx.clone(),
                reply,
            });
            async move { (name, request.await) }
        });
        // The group keeps publishing what its children report while their starts are in flight
        // (a start can take as long as a build or a startup delay), so a dependent of the
        // cluster that waits for its derived status sees it change.
        let all = futures::future::join_all(starts);
        tokio::pin!(all);
        let results = loop {
            tokio::select! {
                results = &mut all => break results,
                _ = self.changed.notified() => self.publish(),
            }
        };

        let mut started = 0;
        let mut failures: Vec<(String, Error)> = Vec::new();
        for (name, result) in results {
            match result {
                Ok(_) => started += 1,
                // up already, or started by someone else a moment ago
                Err(Error::AppAlreadyExists(_)) => {}
                Err(e) => {
                    warn!(
                        "Failed to start instance {} of {}: {}",
                        name, self.spec.name, e
                    );
                    failures.push((name, e));
                }
            }
        }
        if !failures.is_empty() {
            if created {
                // the cluster never worked: it goes, and the first error is what the caller hears
                let (_, first) = failures.swap_remove(0);
                let exit = self.rollback(&ctx).await;
                return (Err(first), exit);
            }
            // an existing cluster keeps what it has; the instances that failed stay errored
            let list: Vec<String> = failures
                .iter()
                .map(|(name, e)| format!("{}: {}", name, e))
                .collect();
            let message = format!(
                "{} of {} instances of '{}' did not start ({}); the others were left as they are",
                failures.len(),
                self.children.len(),
                self.spec.name,
                list.join("; ")
            );
            self.publish();
            return (Err(Error::ProcessStartFailed(message)), false);
        }
        if created {
            // every instance launched: the cluster is meant to run
            match self.ctx.store.set_desired(self.id, Desired::Running).await {
                Ok(()) => self.desired = Desired::Running,
                Err(e) => warn!(
                    "Could not persist the desired state of {}: {}",
                    self.spec.name, e
                ),
            }
        }
        self.publish();
        info!(
            "Cluster '{}' started {} of {} instances",
            self.spec.name,
            started,
            self.children.len()
        );
        (Ok(self.outcome(started > 0)), false)
    }

    /// A new cluster whose start failed: delete it (one transaction, then the actors exit).
    /// True if the actor is done. If the row cannot be removed the instances are stopped and
    /// the cluster stays, registered and at rest, so nothing is half deleted.
    async fn rollback(&mut self, ctx: &CmdCtx) -> bool {
        match self.ctx.store.delete(self.id).await {
            Ok(_) => {
                self.finish_delete(ctx).await;
                true
            }
            Err(e) => {
                warn!(
                    "Could not remove the cluster {} that failed to start: {}",
                    self.spec.name, e
                );
                let _ = self.on_stop(ctx.clone()).await;
                false
            }
        }
    }

    /// The row is gone (the instances' rows with it): every instance that is still there is
    /// deleted without touching the database, and the cluster leaves the registry.
    async fn finish_delete(&mut self, ctx: &CmdCtx) {
        let deletes = self.child_handles().into_iter().map(|child| async move {
            // the rows went with the parent's
            child
                .request_unchecked(|reply| Command::Delete {
                    row_removed: true,
                    ctx: ctx.for_instance(),
                    reply,
                })
                .await
        });
        for result in futures::future::join_all(deletes).await {
            if let Err(e) = result {
                warn!("Could not delete an instance of {}: {}", self.spec.name, e);
            }
        }
        if let Some(registry) = self.ctx.registry() {
            registry.remove(self.id);
        }
    }

    async fn on_stop(&mut self, ctx: CmdCtx) -> Result<StopOutcome> {
        let warning = match self.ctx.store.set_desired(self.id, Desired::Stopped).await {
            Ok(()) => None,
            Err(e) => Some(format!("could not save the desired state: {}", e)),
        };
        self.desired = Desired::Stopped;
        let instance_ctx = ctx.for_instance();
        let stops = self.child_handles().into_iter().map(|child| {
            let instance_ctx = instance_ctx.clone();
            async move {
                child
                    .request(|reply| Command::Stop {
                        ctx: instance_ctx.clone(),
                        reply,
                    })
                    .await
            }
        });
        let mut changed = false;
        let mut failure: Option<Error> = None;
        for result in futures::future::join_all(stops).await {
            match result {
                Ok(outcome) => changed |= outcome.changed,
                Err(e) => {
                    failure.get_or_insert(e);
                }
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(StopOutcome { changed, warning }),
        }
    }

    async fn on_restart(&mut self, cause: RestartCause, ctx: CmdCtx) -> Result<RestartOutcome> {
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::ShuttingDown);
        }
        if self.desired != Desired::Running {
            self.ctx
                .store
                .set_desired(self.id, Desired::Running)
                .await?;
            self.desired = Desired::Running;
        }
        let instance_ctx = ctx.for_instance();
        let restarts = self.child_handles().into_iter().map(|child| {
            let instance_ctx = instance_ctx.clone();
            async move {
                child
                    .request(move |reply| Command::Restart {
                        cause,
                        ctx: instance_ctx.clone(),
                        reply,
                    })
                    .await
            }
        });
        let mut failure: Option<Error> = None;
        for result in futures::future::join_all(restarts).await {
            if let Err(e) = result {
                failure.get_or_insert(e);
            }
        }
        match failure {
            Some(e) => Err(e),
            None => {
                self.publish();
                Ok(RestartOutcome {
                    kind: RestartKind::Restart,
                    status: self.derived().0.status,
                })
            }
        }
    }

    /// One delete removes the parent row (its children go with it) before any actor exits; if
    /// that fails nothing changes.
    async fn on_delete(
        &mut self,
        row_removed: bool,
        ctx: CmdCtx,
        reply: oneshot::Sender<Result<StopOutcome>>,
    ) -> bool {
        if !row_removed {
            if let Err(e) = self.ctx.store.delete(self.id).await {
                let _ = reply.send(Err(e));
                return false;
            }
        }
        let was_active = self.derived().0.status.is_active();
        self.finish_delete(&ctx).await;
        let _ = reply.send(Ok(StopOutcome {
            changed: was_active,
            warning: None,
        }));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_status_follows_the_children() {
        use AppStatus::*;
        assert_eq!(derive_status(&[Running, Running]), Running);
        assert_eq!(derive_status(&[Stopped, Stopped]), Stopped);
        assert_eq!(derive_status(&[Errored, Errored]), Errored);
        assert_eq!(derive_status(&[Running, Stopping]), Stopping);
        assert_eq!(derive_status(&[Running, Building]), Starting);
        assert_eq!(derive_status(&[Waiting, Building, Starting]), Starting);
        // some alive, some not (or not well): degraded
        assert_eq!(derive_status(&[Running, Errored]), Unhealthy);
        assert_eq!(derive_status(&[Running, Stopped]), Unhealthy);
        assert_eq!(derive_status(&[Running, Unhealthy]), Unhealthy);
        assert_eq!(derive_status(&[Running, Backoff]), Unhealthy);
        assert_eq!(derive_status(&[Starting, Errored]), Unhealthy);
        assert_eq!(derive_status(&[]), Stopped);
    }

    #[test]
    fn instance_ports_come_from_the_range_or_count_up_from_the_base() {
        let mut spec = AppSpec::new(
            "p".into(),
            oxidepm_core::Runtime::CMD,
            "true".into(),
            "/".into(),
        );
        assert_eq!(instance_port(&spec, 0), None);
        spec.port = Some(4100);
        assert_eq!(instance_port(&spec, 2), Some(4102));
        spec.port = None;
        spec.port_range = Some((5000, 5001));
        assert_eq!(instance_port(&spec, 1), Some(5001));
    }

    #[test]
    fn instance_ports_never_wrap_or_leave_their_range() {
        let mut spec = AppSpec::new(
            "p".into(),
            oxidepm_core::Runtime::CMD,
            "true".into(),
            "/".into(),
        );
        // counted up from the base: the last port is the last that exists
        spec.port = Some(65_535);
        assert_eq!(instance_port(&spec, 0), Some(65_535));
        assert_eq!(instance_port(&spec, 1), None);
        // an index above u16 does not wrap around to a low port
        spec.port = Some(100);
        assert_eq!(instance_port(&spec, 65_537), None);
        // from a range: no fallback to the base port, the range ends where it ends
        spec.port = None;
        spec.port_range = Some((7000, 7002));
        assert_eq!(instance_port(&spec, 2), Some(7002));
        assert_eq!(instance_port(&spec, 3), None);
        spec.port_range = Some((65_534, 65_535));
        assert_eq!(instance_port(&spec, 1), Some(65_535));
        assert_eq!(instance_port(&spec, 65_537), None);
    }
}
