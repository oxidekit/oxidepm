//! IPC request handlers
//!
//! The way in is [`RequestHandler::guarded`]: the policy decides, once, which apps the request
//! may act on ([`RequestHandler::authorize`], which resolves the selector a single time), and
//! [`RequestHandler::execute`] acts on exactly those apps. Every other method is private, so a
//! request cannot be carried out without the witness the policy hands out.

use oxidepm_adapter_proto::messages::{ActorView as ActorWire, OutputStream};
use oxidepm_adapter_proto::{HostLevel, VerbTier};
use oxidepm_core::actor::ActorClass;
use oxidepm_core::Tier;
use oxidepm_core::{redact_spec, scrub_text, AppInfo, AppSpec, Error, Result, Selector};
use oxidepm_ipc::protocol::changed_count;
use oxidepm_ipc::{
    AdapterReply, AdapterRequest, DryRunPlan, InvokeArgs, ItemOutcome, ItemResult, Request,
    Response,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};

use crate::adapters::conn::Output;
use crate::adapters::host::AdapterHost;
use crate::adapters::invoke::{InvokeRun, ResolvedVerb};
use crate::adapters::session::SessionRun;
use crate::audit::{self, ActorView, Audit, Deny, Lanes, Phase, Record, Target};
use crate::policy::{self, log_field, Authorized, ReqCtx, RestartLimiter};
use crate::supervisor::Supervisor;

/// Test hook: holds the next request between its authorization and its execution.
#[cfg(feature = "test-hooks")]
#[derive(Default)]
pub struct GatePause {
    /// Notified when the request has been authorized and waits.
    pub reached: tokio::sync::Notify,
    /// Notify it to let the request go on.
    pub release: tokio::sync::Notify,
}

/// The most log lines a reader that is not a person gets in one reply.
pub const MAX_LOG_LINES_LIMITED: usize = 10_000;
/// The most bytes of log lines in one such reply.
pub const MAX_LOG_BYTES_LIMITED: usize = 1024 * 1024;
/// Log reads of such readers at the same time in the daemon; each reader also has one at a time.
pub const LOG_READERS: usize = 4;
/// Environment values shorter than this are not hunted for in a log line a limited reader gets.
pub const LOG_SCRUB_MIN: usize = 6;
/// Most readers whose one-at-a-time slot is remembered.
const MAX_READER_SLOTS: usize = 512;

/// What a request leaves behind between its decision and its result: the number the daemon gave
/// it, when it began, and the files its result line goes to. Made by [`RequestHandler::begin`]
/// for a request that was recorded, handed back to [`RequestHandler::finish`].
#[derive(Debug)]
pub struct Ticket {
    req: String,
    started: Instant,
    lanes: Lanes,
    /// The request is a rehearsal: its result line says so.
    dry_run: bool,
}

/// Request handler for IPC commands. Every handler returns the response, or the error that
/// `dispatch` turns into one (the only place a failure becomes a response).
pub struct RequestHandler {
    supervisor: Arc<Supervisor>,
    /// Restarts and reloads by non-human actors (the per-app cooldown).
    limiter: Mutex<RestartLimiter>,
    audit: Arc<Audit>,
    /// The adapter host, once the daemon has loaded it (`DaemonStatus` lists its adapters).
    adapters: Option<Arc<AdapterHost>>,
    /// `daemon.toml` asks for the agent socket, and whether it is bound.
    agent_socket: (bool, bool),
    /// Log reads of readers that are not persons, in the daemon (`LOG_READERS`) ...
    log_readers: Arc<Semaphore>,
    /// ... and one at a time for each reader (an adapter, or the way in of an AI).
    reader_slots: Mutex<HashMap<String, Arc<Semaphore>>>,
    #[cfg(feature = "test-hooks")]
    pause: Mutex<Option<Arc<GatePause>>>,
}

impl RequestHandler {
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        let audit = Arc::new(Audit::new(supervisor.paths()));
        Self::with_audit(supervisor, audit)
    }

    /// A handler that records into `audit` (a test sets one up with its own limits).
    pub fn with_audit(supervisor: Arc<Supervisor>, audit: Arc<Audit>) -> Self {
        Self {
            supervisor,
            limiter: Mutex::new(RestartLimiter::default()),
            audit,
            adapters: None,
            agent_socket: (false, false),
            log_readers: Arc::new(Semaphore::new(LOG_READERS)),
            reader_slots: Mutex::new(HashMap::new()),
            #[cfg(feature = "test-hooks")]
            pause: Mutex::new(None),
        }
    }

    /// Tell `DaemonStatus` whether the agent socket is wanted and whether it is bound.
    pub fn set_agent_socket(&mut self, wanted: bool, bound: bool) {
        self.agent_socket = (wanted, bound);
    }

    /// Tell `DaemonStatus` where the adapters are.
    pub fn set_adapter_host(&mut self, host: Arc<AdapterHost>) {
        self.adapters = Some(host);
    }

    /// The audit log of this handler.
    pub fn audit(&self) -> &Arc<Audit> {
        &self.audit
    }

    /// Write what is folded and drain the audit queues (at shutdown), within the drain budget.
    pub async fn close_audit(&self) {
        self.audit.close(audit::DRAIN_BUDGET).await;
    }

    /// Charge a `ping` or `hello` and count it into the folded polling line.
    pub(crate) fn admit_poll(&self, ctx: &ReqCtx, op: &str) -> Result<()> {
        if !matches!(ctx.actor.class, ActorClass::Human | ActorClass::System) {
            let view = view_of(ctx);
            self.audit.check_rate(&ctx.actor, &view, Tier::Safe)?;
            self.audit.poll(&ctx.actor, &view, op);
        }
        Ok(())
    }

    /// Test hook: the next request that passes the gate waits between `authorize` and `execute`
    /// until the returned pause is released.
    #[cfg(feature = "test-hooks")]
    pub fn pause_after_authorize(&self) -> Arc<GatePause> {
        let pause = Arc::new(GatePause::default());
        *self.pause.lock() = Some(Arc::clone(&pause));
        pause
    }

    /// The gate and the action for a request: authorize and record the decision ([`begin`]),
    /// execute what was authorized, record the result ([`finish`]).
    ///
    /// [`begin`]: Self::begin
    /// [`finish`]: Self::finish
    pub async fn guarded(&self, ctx: &ReqCtx, request: Request) -> Result<Response> {
        let dry = Self::is_dry_run(ctx, &request);
        let (auth, ticket) = match self.begin(ctx, &request).await {
            Ok(begun) => begun,
            // a rehearsal of a request the policy refuses answers with the refusal as its plan
            Err(e) if dry && is_refusal(&e) => return Ok(denied_plan(&request, &e)),
            Err(e) => return Err(e),
        };
        #[cfg(feature = "test-hooks")]
        {
            let pause = self.pause.lock().take();
            if let Some(pause) = pause {
                pause.reached.notify_one();
                pause.release.notified().await;
            }
        }
        let result = if dry {
            self.plan(auth, request).await
        } else {
            self.execute(auth, request).await
        };
        self.finish(ticket, &result);
        result
    }

    /// Is this request a rehearsal? The client asked for one and the request changes something:
    /// a read-only request ignores the flag.
    pub fn is_dry_run(ctx: &ReqCtx, request: &Request) -> bool {
        ctx.dry_run && policy::classify(request, ctx.reveal).mutates
    }

    /// Everything before the action. The actor's rate is checked; the gate decides; the decision
    /// is recorded (for a non-human actor's caution or dangerous request, on disk before this
    /// returns: if that fails the request is refused with `E_AUDIT_UNAVAILABLE` and nothing
    /// runs; a person's request never waits for the audit log). The ticket is for [`finish`].
    ///
    /// [`finish`]: Self::finish
    pub async fn begin(
        &self,
        ctx: &ReqCtx,
        request: &Request,
    ) -> Result<(Authorized, Option<Ticket>)> {
        let classified = policy::classify(request, ctx.reveal);
        self.begin_with(
            ctx,
            request,
            classified,
            ctx.dry_run && classified.mutates,
            None,
            false,
        )
        .await
    }

    /// [`begin`](Self::begin) for a request whose class is not a function of the request alone: a
    /// relayed verb has the tier its manifest declares. `denial`, if there is one, is the reason
    /// the policy refuses it, decided by the caller (it needs the daemon's state); it is applied
    /// after the class check and recorded like any other refusal.
    ///
    /// `dry_run` is the rehearsal flag to record. `rate_checked` says the caller already took
    /// this actor's token for the request.
    async fn begin_with(
        &self,
        ctx: &ReqCtx,
        request: &Request,
        classified: policy::Classified,
        dry_run: bool,
        denial: Option<String>,
        rate_checked: bool,
    ) -> Result<(Authorized, Option<Ticket>)> {
        let class = ctx.actor.class;
        let human = matches!(class, ActorClass::Human | ActorClass::System);
        let view = view_of(ctx);
        if !rate_checked {
            self.audit.check_rate(&ctx.actor, &view, classified.tier)?;
        }

        // repeated status polls of a non-human actor are one line a minute, not one each
        if !human && matches!(request, Request::Status | Request::DaemonStatus) {
            self.audit.poll(&ctx.actor, &view, classified.op);
            return Ok((self.authorize(ctx, request).await?, None));
        }

        let outcome = self.gate_with(ctx, request, &classified, denial).await;
        let mut record = decision_record(
            self.audit.next_req(),
            ctx,
            request,
            &classified,
            view,
            dry_run,
        );
        match outcome {
            Ok((auth, targets)) => {
                let Some(lanes) = audit::lanes_for(class, classified.tier, false) else {
                    return Ok((auth, None));
                };
                record.decision = Some("allow".to_string());
                if !targets.is_empty() {
                    record.targets = Some(targets);
                }
                let req = record.req.clone().unwrap_or_default();
                self.audit
                    .decision(record, lanes, audit::must_wait(class, classified.tier))
                    .await?;
                Ok((
                    auth,
                    Some(Ticket {
                        req,
                        started: Instant::now(),
                        lanes,
                        dry_run,
                    }),
                ))
            }
            Err(e) if is_refusal(&e) => {
                record.decision = Some("deny".to_string());
                record.deny = Some(Deny {
                    code: e.code().as_str().to_string(),
                    why: e.to_string(),
                });
                let lanes = audit::lanes_for(class, classified.tier, true).unwrap_or(Lanes {
                    main: true,
                    security: true,
                });
                self.audit.queue(record, lanes);
                Err(e)
            }
            Err(e) => {
                // the policy had nothing against it, the request failed before it could run
                // (an app that does not exist): the decision and its result, nothing waited for
                if let Some(lanes) = audit::lanes_for(class, classified.tier, false) {
                    let mut result = Record::new(Phase::Result);
                    result.req = record.req.clone();
                    result.ok = Some(false);
                    result.code = Some(e.code().as_str().to_string());
                    result.ms = Some(0);
                    record.decision = Some("allow".to_string());
                    self.audit.queue(record, lanes);
                    self.audit.queue(result, lanes);
                }
                Err(e)
            }
        }
    }

    /// Record what a request did. Never waits and never fails the request.
    pub fn finish(&self, ticket: Option<Ticket>, result: &Result<Response>) {
        let Some(ticket) = ticket else { return };
        let ms = ticket.started.elapsed().as_millis() as u64;
        let mut record = result_record(ticket.req, result, ms);
        if ticket.dry_run {
            record.dry_run = Some(true);
        }
        self.audit.queue(record, ticket.lanes);
    }

    /// The gate. Stage 1 (the actor's class against the request's tier) needs nothing from the
    /// daemon's state. Stage 2 resolves the request's selector once, looks at the markers of
    /// every app it reaches (a cluster's instances too) and decides. The apps are carried in
    /// the [`Authorized`] and nothing resolves a selector again.
    pub async fn authorize(&self, ctx: &ReqCtx, request: &Request) -> Result<Authorized> {
        self.gate(ctx, request).await.map(|(auth, _)| auth)
    }

    /// [`authorize`](Self::authorize), with the apps the request reaches named (for the audit
    /// log).
    async fn gate(&self, ctx: &ReqCtx, request: &Request) -> Result<(Authorized, Vec<Target>)> {
        self.gate_with(ctx, request, &policy::classify(request, ctx.reveal), None)
            .await
    }

    async fn gate_with(
        &self,
        ctx: &ReqCtx,
        request: &Request,
        classified: &policy::Classified,
        denial: Option<String>,
    ) -> Result<(Authorized, Vec<Target>)> {
        let grant = ctx.grant.as_ref();
        let denied = |e: Error| {
            warn!(
                "request {} denied: {}",
                log_field(&ctx.request_id),
                log_field(&e)
            );
            e
        };
        policy::stage1(&ctx.actor, grant, classified).map_err(denied)?;
        if let Some(reason) = denial {
            return Err(denied(Error::PolicyDenied(reason)));
        }
        let is_adapter = ctx.actor.class == ActorClass::Adapter;

        let selector = match request {
            Request::Stop { selector }
            | Request::Restart { selector }
            | Request::Reload { selector }
            | Request::Delete { selector }
            | Request::Flush { selector }
            | Request::SetPolicy { selector, .. } => selector,
            // an adapter that may start apps is held to the apps it names, and to its scope
            Request::Start { .. } | Request::StartBatch { .. } if is_adapter => {
                return self
                    .gate_adapter_start(ctx, request, classified)
                    .await
                    .map_err(denied);
            }
            // `save` and `resurrect` act on every app: for an adapter, on all of them or none
            Request::Save { .. } | Request::Resurrect if is_adapter => &Selector::All,
            _ => {
                let auth = policy::authorize(&ctx.actor, grant, classified, Vec::new(), &[])
                    .map_err(denied)?;
                return Ok((auth, Vec::new()));
            }
        };
        let human = matches!(ctx.actor.class, ActorClass::Human | ActorClass::System);
        // an ai actor names its apps: 'all' and a tag name more than it looked at
        if ctx.actor.class == ActorClass::Ai && !policy::selector_is_named(selector) {
            return Err(denied(policy::name_the_apps(&ctx.actor, classified)));
        }
        let ids = self.supervisor.resolve_selector(selector).await?;
        let targets = self.supervisor.target_views(&ids);
        let mut auth =
            policy::authorize(&ctx.actor, grant, classified, ids, &targets).map_err(denied)?;
        let mut named = targets
            .iter()
            .map(|t| Target {
                id: Some(t.id),
                name: t.name.clone(),
            })
            .collect::<Vec<_>>();

        if matches!(request, Request::Resurrect) {
            let (saved, targets) = self.supervisor.read_saved_for_resurrect().map_err(denied)?;
            policy::authorize(&ctx.actor, grant, classified, Vec::new(), &targets)
                .map_err(denied)?;
            auth.saved_resurrect = Some(saved);
            named.extend(targets.into_iter().map(|t| Target {
                id: None,
                name: t.name,
            }));
        }

        // a looping agent cannot keep an app permanently down
        if !human && matches!(request, Request::Restart { .. } | Request::Reload { .. }) {
            let now = Instant::now();
            // a rehearsal asks the question and leaves the allowance alone
            let verdict = if ctx.dry_run && classified.mutates {
                self.limiter.lock().check(auth.ids(), now)
            } else {
                self.limiter.lock().admit(auth.ids(), now)
            };
            if let Err((id, wait)) = verdict {
                let name = targets
                    .iter()
                    .find(|t| t.id == id)
                    .map_or_else(|| id.to_string(), |t| t.name.clone());
                return Err(denied(Error::RateLimited(format!(
                    "{} restarted or reloaded '{}' too often: wait {} s (one every {} s, at most {} in {} min)",
                    ctx.actor.wire(),
                    name,
                    wait.as_secs() + 1,
                    policy::RESTART_COOLDOWN.as_secs(),
                    policy::RESTARTS_PER_WINDOW,
                    policy::RESTART_WINDOW.as_secs() / 60
                ))));
            }
        }
        Ok((auth, named))
    }

    /// The gate for an adapter that starts apps (a `start` of a spec is dangerous: `admin`). An
    /// app that exists is held to its markers and, for the scope `opted-in`, to listing the
    /// adapter; a spec that names no app yet is held to the scope by what the spec lists.
    /// Only a person may set policy markers or opt into inheriting the daemon environment.
    async fn gate_adapter_start(
        &self,
        ctx: &ReqCtx,
        request: &Request,
        classified: &policy::Classified,
    ) -> Result<(Authorized, Vec<Target>)> {
        let specs = match request {
            Request::Start { spec } => std::slice::from_ref(spec),
            Request::StartBatch { specs } => specs.as_slice(),
            _ => &[],
        };
        let mut ids = Vec::new();
        let mut new_apps = Vec::new();
        for spec in specs {
            match self
                .supervisor
                .resolve_selector(&Selector::ByName(spec.name.clone()))
                .await
            {
                Ok(found) => ids.extend(found),
                Err(Error::AppNotFound(_)) => new_apps.push((
                    spec.name.clone(),
                    spec.adapters.contains_key(&*ctx.actor.label),
                )),
                Err(e) => return Err(e),
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let targets = self.supervisor.target_views(&ids);
        let auth = policy::authorize(&ctx.actor, ctx.grant.as_ref(), classified, ids, &targets)?;
        policy::authorize_new_apps(&ctx.actor, ctx.grant.as_ref(), classified, &new_apps)?;
        if let Some(spec) = specs
            .iter()
            .find(|spec| spec.protected || spec.ai_control || spec.env_inherit)
        {
            return Err(Error::PolicyDenied(format!(
                "{} may not set protected, ai_control or env_inherit when starting '{}'",
                ctx.actor.wire(),
                spec.name
            )));
        }
        Ok((auth, Vec::new()))
    }

    /// Carry out an authorized request. It acts on the apps in `auth` and on nothing else.
    /// `Ping`, `Hello` and `Kill` are answered by the daemon itself and never get here.
    pub async fn execute(&self, auth: Authorized, request: Request) -> Result<Response> {
        let ids = auth.ids();
        match request {
            Request::Ping | Request::Hello { .. } | Request::Kill => Err(Error::Internal(format!(
                "'{}' is not carried out by the handler",
                request.name()
            ))),
            Request::Start { spec } => self.start(spec).await,
            Request::StartBatch { specs } => self.start_batch(specs).await,
            Request::Stop { selector } => self.stop(&selector, ids).await,
            Request::Restart { selector } => self.restart(&selector, ids).await,
            Request::Delete { selector } => self.delete(&selector, ids).await,
            Request::Status => self.status().await,
            Request::Show { selector } => self.show(selector, auth.reveal()).await,
            Request::Logs {
                selector,
                lines,
                follow,
                stdout,
                stderr,
            } => {
                self.logs(
                    selector,
                    lines,
                    follow,
                    stdout,
                    stderr,
                    auth.limited_reader(),
                )
                .await
            }
            Request::Save { force } => self.save(force).await,
            Request::Resurrect => self.resurrect(auth).await,
            Request::Reload { selector } => self.reload(&selector, ids).await,
            Request::Flush { selector } => self.flush(&selector, ids).await,
            Request::Describe { selector } => self.describe(selector, auth.reveal()).await,
            Request::DaemonStatus => self.daemon_status(),
            Request::SetPolicy {
                selector,
                protected,
                ai_control,
            } => self.set_policy(&selector, ids, protected, ai_control).await,
            Request::Adapter(request) => {
                self.adapter(*request, auth.limited_reader().is_some())
                    .await
            }
        }
    }

    /// An `adapter` request: the host does the work. Removing an adapter that provides a runtime
    /// says which apps use it (they keep running; their next start fails).
    ///
    /// What an adapter wrote to stderr stays with a person: `limited` (an AI asks) blanks it.
    async fn adapter(&self, request: AdapterRequest, limited: bool) -> Result<Response> {
        let host = self.adapters.as_ref().ok_or_else(|| {
            Error::Unsupported("this daemon runs without the adapter host".to_string())
        })?;
        let runtimes: Vec<String> = match &request {
            AdapterRequest::Remove { name, .. } => match host
                .handle(AdapterRequest::Info { name: name.clone() })
                .await
            {
                Ok(AdapterReply::Info { adapter }) => {
                    oxidepm_adapter_proto::Manifest::parse(adapter.manifest_json.as_bytes())
                        .map(|m| m.provides.runtimes)
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let mut reply = host.handle(request).await?;
        if limited {
            match &mut reply {
                AdapterReply::Info { adapter } => adapter.stderr_tail.clear(),
                AdapterReply::List { adapters } => {
                    adapters.iter_mut().for_each(|a| a.stderr_tail.clear())
                }
                _ => {}
            }
        }
        if let (AdapterReply::Done { warnings, .. }, false) = (&mut reply, runtimes.is_empty()) {
            if let Ok(apps) = self.supervisor.status().await {
                for app in apps {
                    let runtime = app.spec.mode.to_string();
                    if runtimes.contains(&runtime) {
                        warnings.push(format!(
                            "app '{}' uses the runtime '{runtime}', which this adapter provided: it keeps running, and its next start fails with E_ADAPTER_MISSING",
                            app.spec.name
                        ));
                    }
                }
            }
        }
        Ok(Response::Adapter(Box::new(reply)))
    }

    /// What an authorized request would do, built from read-only state: nothing is sent to an
    /// actor, written or spawned. It acts on the apps in `auth` and on nothing else, like
    /// [`execute`](Self::execute).
    pub async fn plan(&self, auth: Authorized, request: Request) -> Result<Response> {
        let ids = auth.ids();
        let op = match &request {
            Request::Adapter(a) => a.op(),
            other => other.name(),
        };
        let (items, warnings) = match request {
            Request::Start { spec } => self.supervisor.plan_start(spec)?,
            Request::StartBatch { specs } => self.supervisor.plan_start_batch(specs)?,
            Request::Stop { .. } => (self.supervisor.plan_stop(ids), Vec::new()),
            Request::Restart { .. } => (self.supervisor.plan_restart(ids, false)?, Vec::new()),
            Request::Reload { .. } => (self.supervisor.plan_restart(ids, true)?, Vec::new()),
            Request::Delete { .. } => (self.supervisor.plan_delete(ids), Vec::new()),
            Request::Flush { .. } => (self.supervisor.plan_flush(ids), Vec::new()),
            Request::Save { force } => self.supervisor.plan_save(force).await?,
            Request::Resurrect => self.supervisor.plan_resurrect().await?,
            Request::Kill => self.supervisor.plan_kill(),
            Request::SetPolicy {
                protected,
                ai_control,
                ..
            } => (
                self.supervisor.plan_markers(ids, protected, ai_control)?,
                Vec::new(),
            ),
            Request::Adapter(request) => {
                let host = self.adapters.as_ref().ok_or_else(|| {
                    Error::Unsupported("this daemon runs without the adapter host".to_string())
                })?;
                host.plan(&request).await?
            }
            // reads ignore a rehearsal and never get here
            other => {
                return Err(Error::Internal(format!(
                    "'{}' has no plan: it changes nothing",
                    other.name()
                )))
            }
        };
        Ok(Response::DryRun {
            plan: DryRunPlan::allowed(op, items, warnings),
        })
    }

    // ---- relayed verbs -------------------------------------------------------------------------

    /// The decision for a relayed verb. The verb's own tier is the request's tier: a person
    /// may run any verb; an `ai` actor only a safe verb of an adapter that holds no more than
    /// `read`, in the working directory of an app it may control (there is no `on_behalf_of`
    /// before the AI adapters, so the adapter's own grant must not be usable for an AI); an adapter
    /// never. The decision is recorded like any other (a refusal too).
    pub async fn begin_invoke(
        &self,
        ctx: &ReqCtx,
        request: &Request,
        resolved: &ResolvedVerb,
        cwd: &std::path::Path,
        rate_checked: bool,
    ) -> Result<Option<Ticket>> {
        let classified = verb_classified(&resolved.verb);
        // a rate token taken for a safe request does not cover a caution one
        let rate_checked = rate_checked && classified.tier == Tier::Safe;
        let denial = self.invoke_denial(ctx, resolved, cwd).await;
        let dry_run = invoke_dry_run(ctx, &resolved.verb);
        self.begin_with(ctx, request, classified, dry_run, denial, rate_checked)
            .await
            .map(|(_, ticket)| ticket)
    }

    /// Everything before a relayed verb runs: the verb and the directory are looked up and the
    /// policy decides. For a person that is the lookup and then the policy. For anything else
    /// the class and the rate limit come first, and only then does the daemon look at the
    /// adapters, the verbs and the directories, so an AI cannot find out what exists without a
    /// line in the audit log: an adapter never runs a verb, an AI is held to its rate, and a
    /// lookup that fails for either is a recorded refusal.
    pub async fn prepare_invoke(
        &self,
        ctx: &ReqCtx,
        request: &Request,
        args: &InvokeArgs,
    ) -> Result<(ResolvedVerb, std::path::PathBuf, Option<Ticket>)> {
        let host = self.adapters.as_ref().ok_or_else(|| {
            Error::Unsupported("this daemon runs without the adapter host".to_string())
        })?;
        let class = ctx.actor.class;
        if matches!(class, ActorClass::Human | ActorClass::System) {
            let resolved = host.resolve_verb(&args.verb)?;
            let cwd = host.check_cwd(&args.cwd)?;
            let ticket = self
                .begin_invoke(ctx, request, &resolved, &cwd, false)
                .await?;
            return Ok((resolved, cwd, ticket));
        }
        // the lowest tier: what an actor may ask whatever the verb turns out to be
        let before_lookup = policy::Classified {
            tier: Tier::Safe,
            op: "adapter invoke",
            mutates: false,
            global: false,
            reveal: false,
            human_only: false,
        };
        if class == ActorClass::Adapter {
            let denial = format!(
                "{} may not run the verb '{}': an adapter cannot invoke a verb",
                ctx.actor.wire(),
                args.verb
            );
            let refused = self
                .begin_with(ctx, request, before_lookup, false, Some(denial), false)
                .await;
            return Err(refused.err().unwrap_or_else(|| {
                Error::Internal("an adapter was allowed to invoke a verb".to_string())
            }));
        }
        let view = view_of(ctx);
        self.audit.check_rate(&ctx.actor, &view, Tier::Safe)?;
        let found = host
            .resolve_verb(&args.verb)
            .and_then(|resolved| host.check_cwd(&args.cwd).map(|cwd| (resolved, cwd)));
        let (resolved, cwd) = match found {
            Ok(found) => found,
            Err(e) => {
                warn!(
                    "request {} denied: {}",
                    log_field(&ctx.request_id),
                    log_field(&e)
                );
                let mut record = decision_record(
                    self.audit.next_req(),
                    ctx,
                    request,
                    &before_lookup,
                    view,
                    false,
                );
                record.decision = Some("deny".to_string());
                record.deny = Some(Deny {
                    code: e.code().as_str().to_string(),
                    why: e.to_string(),
                });
                let lanes = audit::lanes_for(class, Tier::Safe, true).unwrap_or(Lanes {
                    main: true,
                    security: true,
                });
                self.audit.queue(record, lanes);
                return Err(e);
            }
        };
        let ticket = self
            .begin_invoke(ctx, request, &resolved, &cwd, true)
            .await?;
        Ok((resolved, cwd, ticket))
    }

    /// Why the policy refuses this actor the verb, if it does.
    async fn invoke_denial(
        &self,
        ctx: &ReqCtx,
        resolved: &ResolvedVerb,
        cwd: &std::path::Path,
    ) -> Option<String> {
        let verb = &resolved.verb;
        let who = ctx.actor.wire();
        match ctx.actor.class {
            ActorClass::Human | ActorClass::System => None,
            ActorClass::Adapter => Some(format!(
                "{who} may not run the verb '{}': an adapter cannot invoke a verb",
                verb.name
            )),
            ActorClass::Ai => {
                if verb.tier != VerbTier::Safe {
                    return Some(format!(
                        "{who} may not run the verb '{}': it is a {} verb. Ask a person to run it",
                        verb.name,
                        policy::tier_word(verb.tier)
                    ));
                }
                if resolved.level > HostLevel::Read {
                    return Some(format!(
                        "{who} may not run the verb '{}': its adapter '{}' holds the host level '{}', and an AI may use only the verbs of an adapter that can do no more than read",
                        verb.name,
                        resolved.adapter,
                        policy::level_name(resolved.level)
                    ));
                }
                if !self.ai_may_work_in(cwd).await {
                    return Some(format!(
                        "{who} may not run the verb '{}' in {}: that is not the working directory of an app it may control (ai_control, not protected)",
                        verb.name,
                        cwd.display()
                    ));
                }
                None
            }
        }
    }

    /// Does an app whose working directory is `cwd` allow `ai` control (and is it not protected)?
    async fn ai_may_work_in(&self, cwd: &std::path::Path) -> bool {
        let Ok(apps) = self.supervisor.status().await else {
            return false;
        };
        for app in apps {
            let Ok(real) = std::fs::canonicalize(&app.spec.cwd) else {
                continue;
            };
            if real != cwd {
                continue;
            }
            let Ok(ids) = self
                .supervisor
                .resolve_selector(&Selector::ByName(app.spec.name.clone()))
                .await
            else {
                continue;
            };
            let views = self.supervisor.target_views(&ids);
            if !views.is_empty() && views.iter().all(|t| t.ai_control && !t.protected) {
                return true;
            }
        }
        false
    }

    /// Run a verb that [`begin_invoke`](Self::begin_invoke) allowed. The output goes to `out` as it
    /// comes; the answer is `InvokeDone`. Ask for a rehearsal of verbs that write or declare
    /// dry-run support.
    pub async fn run_invoke(
        &self,
        ctx: &ReqCtx,
        args: &InvokeArgs,
        resolved: &ResolvedVerb,
        cwd: std::path::PathBuf,
        out: tokio::sync::mpsc::Sender<Output>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<Response> {
        let host = self.adapters.as_ref().ok_or_else(|| {
            Error::Unsupported("this daemon runs without the adapter host".to_string())
        })?;
        let dry_run = invoke_dry_run(ctx, &resolved.verb);
        if ctx.dry_run && !dry_run {
            let _ = out
                .send(Output {
                    stream: OutputStream::Status,
                    text: format!("The read-only verb '{}' has no dry run, so the daemon is ignoring --dry-run.", resolved.verb.name),
                })
                .await;
        }
        let run = InvokeRun {
            argv: args.argv.clone(),
            cwd,
            dry_run,
            actor: ActorWire {
                class: ctx.actor.class.as_str().to_string(),
                label: ctx.actor.label.to_string(),
            },
        };
        let result = host.invoke(resolved, run, out, cancel).await?;
        Ok(Response::InvokeDone {
            exit_code: result.exit_code,
            result: result.result,
        })
    }

    // ---- sessions --------------------------------------------------------------------------------

    /// Open a session for a verb that owns the terminal and answer with what the CLI needs
    /// to attach. The policy: a person may open any session verb; an `ai` claim only a verb of an
    /// adapter whose manifest says `ai_driven`, and (until the AI adapters bring their second
    /// check) only if the adapter holds no more than `read`; an adapter never. The decision is
    /// recorded like any other, a refusal too. Like a relayed verb, an actor that is not a person
    /// meets the class check and the rate limit before the daemon says what exists.
    pub async fn open_session(
        &self,
        ctx: &ReqCtx,
        request: &Request,
        args: &InvokeArgs,
    ) -> Result<Response> {
        let host = self.adapters.as_ref().ok_or_else(|| {
            Error::Unsupported("this daemon runs without the adapter host".to_string())
        })?;
        if ctx.dry_run {
            return Err(Error::DryRunUnsupported(format!(
                "the verb '{}' is interactive and has no dry run, so no session was opened",
                args.verb
            )));
        }
        let class = ctx.actor.class;
        let person = matches!(class, ActorClass::Human | ActorClass::System);
        // a person's session is a dangerous request; an `ai` claim passes the class check as a
        // caution one and is held to the rules of `session_denial`
        let for_person = policy::Classified {
            tier: Tier::Dangerous,
            op: "adapter session",
            mutates: true,
            global: false,
            reveal: false,
            human_only: true,
        };
        let for_ai = policy::Classified {
            tier: Tier::Caution,
            op: "adapter session",
            mutates: true,
            global: false,
            reveal: false,
            human_only: false,
        };
        let before_lookup = policy::Classified {
            tier: Tier::Safe,
            op: "adapter session",
            mutates: false,
            global: false,
            reveal: false,
            human_only: false,
        };

        let (resolved, cwd, ticket) = if person {
            let resolved = host.resolve_session_verb(&args.verb)?;
            let cwd = host.check_cwd(&args.cwd)?;
            let (_, ticket) = self
                .begin_with(ctx, request, for_person, false, None, false)
                .await?;
            (resolved, cwd, ticket)
        } else if class == ActorClass::Adapter {
            let denial = format!(
                "{} may not open the verb '{}': an adapter cannot open a session",
                ctx.actor.wire(),
                args.verb
            );
            let refused = self
                .begin_with(ctx, request, before_lookup, false, Some(denial), false)
                .await;
            return Err(refused.err().unwrap_or_else(|| {
                Error::Internal("an adapter was allowed to open a session".to_string())
            }));
        } else {
            let view = view_of(ctx);
            self.audit.check_rate(&ctx.actor, &view, Tier::Safe)?;
            let found = host
                .resolve_session_verb(&args.verb)
                .and_then(|resolved| host.check_cwd(&args.cwd).map(|cwd| (resolved, cwd)));
            let (resolved, cwd) = match found {
                Ok(found) => found,
                Err(e) => {
                    warn!(
                        "request {} denied: {}",
                        log_field(&ctx.request_id),
                        log_field(&e)
                    );
                    let mut record = decision_record(
                        self.audit.next_req(),
                        ctx,
                        request,
                        &before_lookup,
                        view,
                        false,
                    );
                    record.decision = Some("deny".to_string());
                    record.deny = Some(Deny {
                        code: e.code().as_str().to_string(),
                        why: e.to_string(),
                    });
                    let lanes = audit::lanes_for(class, Tier::Safe, true).unwrap_or(Lanes {
                        main: true,
                        security: true,
                    });
                    self.audit.queue(record, lanes);
                    return Err(e);
                }
            };
            let denial = self.session_denial(ctx, &resolved);
            let (_, ticket) = self
                .begin_with(ctx, request, for_ai, false, denial, false)
                .await?;
            (resolved, cwd, ticket)
        };

        let run = SessionRun {
            argv: args.argv.clone(),
            cwd,
            actor: ActorWire {
                class: class.as_str().to_string(),
                label: ctx.actor.label.to_string(),
            },
            // what an `ai` claim opens writes nowhere but the adapter's own directory
            restrict_writes: !person,
        };
        let result = host
            .open_session(resolved, run)
            .map(|offer| Response::Adapter(Box::new(AdapterReply::Session(offer))));
        self.finish(ticket, &result);
        result
    }

    /// Why the policy refuses an `ai` claim the session, if it does.
    fn session_denial(&self, ctx: &ReqCtx, resolved: &ResolvedVerb) -> Option<String> {
        let who = ctx.actor.wire();
        if !resolved.ai_driven {
            return Some(format!(
                "{who} may not open the verb '{}': its adapter '{}' is not marked ai_driven. Ask a person to run it",
                resolved.verb.name, resolved.adapter
            ));
        }
        if resolved.level > HostLevel::Read {
            return Some(format!(
                "{who} may not open the verb '{}': its adapter '{}' holds the host level '{}', and an AI may open only the sessions of an adapter that can do no more than read",
                resolved.verb.name,
                resolved.adapter,
                policy::level_name(resolved.level)
            ));
        }
        None
    }

    /// Checkpoint and close the database (graceful shutdown)
    pub async fn close_db(&self) {
        self.supervisor.close_db().await;
    }

    /// The name to report for an app id (the id itself if the app cannot be read).
    async fn name_of(&self, id: u32) -> String {
        match self.supervisor.show(&Selector::ById(id)).await {
            Ok(Some(info)) => info.spec.name,
            _ => id.to_string(),
        }
    }

    /// Run `op` on every authorized app and report what happened to each one. A
    /// failure on one app is recorded and does not stop the others.
    async fn for_each<'a, F, Fut>(
        &'a self,
        ids: &[u32],
        verb: &str,
        op: F,
    ) -> Result<Vec<ItemResult>>
    where
        F: Fn(u32) -> Fut,
        Fut: std::future::Future<Output = Result<bool>> + 'a,
    {
        let mut items = Vec::with_capacity(ids.len());
        for &id in ids {
            let name = self.name_of(id).await;
            items.push(match op(id).await {
                Ok(true) => ItemResult::changed(id, name),
                Ok(false) => ItemResult::unchanged(id, name),
                Err(e) => {
                    error!("Error {} {}: {}", verb, log_field(&name), log_field(&e));
                    ItemResult::failed(id, name, &e)
                }
            });
        }
        Ok(items)
    }

    /// Handle start request
    async fn start(&self, spec: AppSpec) -> Result<Response> {
        info!("Handling start request for: {}", log_field(&spec.name));

        let name = spec.name.clone();
        let id = self.supervisor.start(spec).await?;
        let status = match self.supervisor.show(&Selector::ById(id)).await? {
            Some(app) => app.state.status,
            None => oxidepm_core::AppStatus::Stopped,
        };
        Ok(Response::Started {
            id,
            name,
            status,
            child_ids: self.supervisor.child_ids(id),
        })
    }

    /// Handle a batch start: each spec on its own result (a refused app is that app's error,
    /// not the request's).
    async fn start_batch(&self, specs: Vec<AppSpec>) -> Result<Response> {
        info!("Handling start request for a batch of {} apps", specs.len());

        let items = self.supervisor.start_batch(specs).await?;
        let items = items
            .into_iter()
            .map(|item| match item.result {
                Ok(started) => ItemResult {
                    id: item.id,
                    name: item.name,
                    result: oxidepm_ipc::ItemOutcome::Ok {
                        changed: started.changed,
                        detail: Some(started.status.to_string()),
                    },
                },
                Err(e) => {
                    error!(
                        "Error starting {}: {}",
                        log_field(&item.name),
                        log_field(&e)
                    );
                    ItemResult::failed(item.id, item.name, &e)
                }
            })
            .collect();
        Ok(Response::StartedBatch { items })
    }

    /// Handle stop request
    async fn stop(&self, selector: &Selector, ids: &[u32]) -> Result<Response> {
        info!("Handling stop request for: {}", log_field(selector));

        // dependents before their dependencies, a layer at a time
        let mut items = Vec::with_capacity(ids.len());
        for (id, result) in self.supervisor.stop_many(ids).await {
            let name = self.name_of(id).await;
            items.push(match result {
                Ok(true) => ItemResult::changed(id, name),
                Ok(false) => ItemResult::unchanged(id, name),
                Err(e) => {
                    error!("Error stopping {}: {}", log_field(&name), log_field(&e));
                    ItemResult::failed(id, name, &e)
                }
            });
        }
        Ok(Response::Stopped {
            count: changed_count(&items),
            items,
        })
    }

    /// Handle restart request
    async fn restart(&self, selector: &Selector, ids: &[u32]) -> Result<Response> {
        info!("Handling restart request for: {}", log_field(selector));

        let supervisor = &self.supervisor;
        let items = self
            .for_each(ids, "restarting", |id| supervisor.restart(id))
            .await?;
        Ok(Response::Restarted {
            count: changed_count(&items),
            items,
        })
    }

    /// Handle delete request
    async fn delete(&self, selector: &Selector, ids: &[u32]) -> Result<Response> {
        info!("Handling delete request for: {}", log_field(selector));

        let supervisor = &self.supervisor;
        let items = self
            .for_each(ids, "deleting", |id| supervisor.delete(id))
            .await?;
        Ok(Response::Deleted {
            count: changed_count(&items),
            items,
        })
    }

    /// Handle status request. Env values and scripts never leave the daemon here.
    async fn status(&self) -> Result<Response> {
        let apps = self.supervisor.status().await?;
        Ok(Response::Status {
            apps: apps
                .into_iter()
                .map(|app| redact_info(app, false))
                .collect(),
        })
    }

    /// The one app a `show` or `describe` names.
    async fn one_app(&self, selector: &Selector) -> Result<AppInfo> {
        if matches!(selector, Selector::All) {
            return Err(Error::InvalidSelector(
                "this command needs one app, not 'all'".to_string(),
            ));
        }
        self.supervisor
            .show(selector)
            .await?
            .ok_or_else(|| Error::AppNotFound(selector.to_string()))
    }

    /// Handle show request. `reveal` returns the real env values and hook scripts.
    async fn show(&self, selector: Selector, reveal: bool) -> Result<Response> {
        let app = self.one_app(&selector).await?;
        Ok(Response::Show {
            app: redact_info(app, reveal),
        })
    }

    /// Handle logs request. A person reads as before. Anyone else (an adapter, an AI) gets lines
    /// scrubbed of the app's environment values, at most `MAX_LOG_LINES_LIMITED` lines and
    /// `MAX_LOG_BYTES_LIMITED` bytes (the newest are kept), read on a blocking thread, one read
    /// at a time per reader and `LOG_READERS` in the daemon.
    async fn logs(
        &self,
        selector: Selector,
        lines: usize,
        follow: bool,
        stdout: bool,
        stderr: bool,
        limited: Option<&str>,
    ) -> Result<Response> {
        if follow {
            return Err(Error::Unsupported(
                "following logs over IPC is not available; read the log files (the CLI does)"
                    .to_string(),
            ));
        }
        let Some(reader) = limited else {
            let log_lines = self
                .supervisor
                .logs(&selector, lines, stdout, stderr)
                .await?;
            return Ok(Response::LogLines { lines: log_lines });
        };
        let mine = self.reader_slot(reader);
        // its own slot first, so a reader that waits for itself does not hold a daemon-wide one
        let _mine = mine
            .acquire_owned()
            .await
            .map_err(|_| Error::ShuttingDown)?;
        let _shared = Arc::clone(&self.log_readers)
            .acquire_owned()
            .await
            .map_err(|_| Error::ShuttingDown)?;
        let supervisor = Arc::clone(&self.supervisor);
        let wanted = lines.min(MAX_LOG_LINES_LIMITED);
        let read = tokio::task::spawn_blocking(move || {
            supervisor.read_logs(&selector, wanted, stdout, stderr, Some(LOG_SCRUB_MIN))
        })
        .await
        .map_err(|e| Error::Internal(format!("the log read failed: {e}")))??;
        Ok(Response::LogLines {
            lines: newest_within(read, MAX_LOG_BYTES_LIMITED),
        })
    }

    /// The one-at-a-time slot of a reader.
    fn reader_slot(&self, reader: &str) -> Arc<Semaphore> {
        let mut slots = self.reader_slots.lock();
        if slots.len() >= MAX_READER_SLOTS && !slots.contains_key(reader) {
            // forget the readers that are not reading now
            slots.retain(|_, slot| Arc::strong_count(slot) > 1);
        }
        Arc::clone(
            slots
                .entry(reader.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(1))),
        )
    }

    /// Handle save request
    async fn save(&self, force: bool) -> Result<Response> {
        let count = self.supervisor.save(force).await?;
        Ok(Response::Saved {
            count,
            path: self
                .supervisor
                .paths()
                .saved()
                .to_string_lossy()
                .to_string(),
        })
    }

    /// Handle resurrect request
    async fn resurrect(&self, mut auth: Authorized) -> Result<Response> {
        let report = match auth.saved_resurrect.take() {
            Some(saved) => self.supervisor.resurrect_checked(saved, auth.ids()).await?,
            None => self.supervisor.resurrect_report().await?,
        };
        Ok(Response::Resurrected {
            started: report.started,
            imported: report.imported,
            failed: report
                .failed
                .iter()
                .map(|f| ItemResult::failed(f.id, f.name.clone(), &f.error))
                .collect(),
        })
    }

    /// Handle reload request. A reload is a restart (there is no rolling reload), and the
    /// response says so.
    async fn reload(&self, selector: &Selector, ids: &[u32]) -> Result<Response> {
        info!("Handling reload request for: {}", log_field(selector));

        let supervisor = &self.supervisor;
        let items = self
            .for_each(ids, "reloading", |id| supervisor.reload(id))
            .await?;
        Ok(Response::Reloaded {
            count: changed_count(&items),
            items,
            kind: "restart".to_string(),
        })
    }

    /// Handle flush request (truncate log files)
    async fn flush(&self, selector: &Selector, ids: &[u32]) -> Result<Response> {
        info!("Handling flush request for: {}", log_field(selector));

        let items = self
            .for_each(ids, "flushing logs of", |id| self.flush_logs_for_app(id))
            .await?;
        Ok(Response::Flushed {
            count: changed_count(&items),
            items,
        })
    }

    /// Flush logs for a single app: the supervisor empties the files through the writers of the
    /// running capture, or directly when no child runs. A failure is the app's failure.
    async fn flush_logs_for_app(&self, id: u32) -> Result<bool> {
        self.supervisor.flush(id).await
    }

    /// Handle set_policy request: the markers change on the running apps (a cluster's instances
    /// with it), in the database in one transaction, without a restart.
    async fn set_policy(
        &self,
        selector: &Selector,
        ids: &[u32],
        protected: Option<bool>,
        ai_control: Option<bool>,
    ) -> Result<Response> {
        info!("Handling set_policy request for: {}", log_field(selector));
        let items = self
            .supervisor
            .set_markers(ids, protected, ai_control)
            .await?
            .into_iter()
            .map(|change| ItemResult {
                id: change.id,
                name: change.name,
                result: oxidepm_ipc::ItemOutcome::Ok {
                    changed: change.changed,
                    detail: Some(format!(
                        "protected={} ai_control={}",
                        change.protected, change.ai_control
                    )),
                },
            })
            .collect::<Vec<_>>();
        Ok(Response::PolicySet {
            count: changed_count(&items),
            items,
        })
    }

    /// How the daemon itself is.
    fn daemon_status(&self) -> Result<Response> {
        let audit = self.audit.state();
        let mut warnings = audit.warnings;
        let (wanted, bound) = self.agent_socket;
        if wanted && !bound {
            warnings.push(format!(
                "the agent socket {} is not available (the path is too long, or something is in the way); AI clients cannot use it",
                self.supervisor.paths().agent_socket().display()
            ));
        }
        warnings.extend(self.supervisor.runtime_warnings());
        let (adapters, sandbox) = match &self.adapters {
            Some(host) => {
                warnings.extend(host.warnings());
                (host.status(), Some(host.sandbox_status()))
            }
            None => (Vec::new(), None),
        };
        Ok(Response::DaemonStatus {
            audit: audit.state.to_string(),
            audit_lost: audit.lost,
            agent_socket: bound,
            adapters,
            warnings,
            sandbox,
        })
    }

    /// Handle describe request (show what command would run)
    async fn describe(&self, selector: Selector, reveal: bool) -> Result<Response> {
        info!("Handling describe request for: {}", log_field(&selector));

        let app = self.one_app(&selector).await?;
        let spec = redact_spec(&app.spec, reveal);
        Ok(Response::Described {
            name: spec.name,
            command: spec.command,
            args: spec.args,
            cwd: spec.cwd.to_string_lossy().to_string(),
            env: spec.env.into_iter().collect(),
            runtime: spec.mode.to_string(),
        })
    }
}

/// The newest of `lines` that fit in `max` bytes (each line counts with its newline).
fn newest_within(lines: Vec<String>, max: usize) -> Vec<String> {
    let mut used = 0usize;
    let mut keep = 0usize;
    for line in lines.iter().rev() {
        used += line.len() + 1;
        if used > max {
            break;
        }
        keep += 1;
    }
    let skip = lines.len() - keep;
    if skip == 0 {
        return lines;
    }
    lines.into_iter().skip(skip).collect()
}

/// What a client may see: no env values and no scripts unless `reveal`.
fn redact_info(info: AppInfo, reveal: bool) -> AppInfo {
    AppInfo {
        spec: redact_spec(&info.spec, reveal),
        state: info.state,
    }
}

/// The actor of a request as the audit log writes it.
fn view_of(ctx: &ReqCtx) -> ActorView {
    ActorView::of(&ctx.actor, ctx.peer.pid, ctx.peer.uid)
}

/// The class of a relayed verb: the tier its manifest declares.
fn verb_classified(verb: &oxidepm_adapter_proto::Verb) -> policy::Classified {
    let tier = match verb.tier {
        VerbTier::Safe => Tier::Safe,
        VerbTier::Caution => Tier::Caution,
        VerbTier::Dangerous => Tier::Dangerous,
    };
    policy::Classified {
        tier,
        op: "adapter invoke",
        mutates: verb.writes,
        global: false,
        reveal: false,
        human_only: false,
    }
}

fn invoke_dry_run(ctx: &ReqCtx, verb: &oxidepm_adapter_proto::Verb) -> bool {
    ctx.dry_run && (verb.writes || verb.dry_run)
}

/// A refusal by the policy or the rate limit (as opposed to a request that failed to run).
pub fn is_refusal(error: &Error) -> bool {
    matches!(
        error,
        Error::PolicyDenied(_) | Error::Protected(_) | Error::RateLimited(_)
    )
}

/// The answer to a rehearsal of a request the policy refuses: the refusal is the plan.
pub fn denied_plan(request: &Request, error: &Error) -> Response {
    Response::DryRun {
        plan: DryRunPlan::denied(request.name(), error.code(), error.to_string()),
    }
}

/// The common part of a decision line. The program of a `start` is written with its argument
/// count and a digest of the arguments, never the arguments.
fn decision_record(
    req: String,
    ctx: &ReqCtx,
    request: &Request,
    classified: &policy::Classified,
    actor: ActorView,
    dry_run: bool,
) -> Record {
    let mut record = Record::new(Phase::Decision);
    record.req = Some(req);
    record.client_req = Some(ctx.request_id.clone());
    record.actor = Some(actor);
    record.op = Some(classified.op.to_string());
    record.tier = Some(classified.tier.as_str().to_string());
    record.dry_run = Some(dry_run);
    record.end_user = ctx.end_user.clone();
    match request {
        Request::Stop { selector }
        | Request::Restart { selector }
        | Request::Reload { selector }
        | Request::Delete { selector }
        | Request::Flush { selector }
        | Request::Show { selector }
        | Request::Describe { selector }
        | Request::Logs { selector, .. }
        | Request::SetPolicy { selector, .. } => record.selector = Some(selector.to_string()),
        Request::Start { spec } => {
            record.selector = Some(spec.name.clone());
            record.program = Some(scrub_text(&spec.command, spec));
            record.argc = Some(spec.args.len() as u64);
            record.args_sha256 = Some(audit::args_digest(&spec.args));
        }
        Request::StartBatch { specs } => {
            record.targets = Some(
                specs
                    .iter()
                    .map(|spec| Target {
                        id: None,
                        name: spec.name.clone(),
                    })
                    .collect(),
            );
        }
        // the adapter a request is about; never the index bytes or anything else it carries
        Request::Adapter(request) => {
            record.selector = match &**request {
                AdapterRequest::Enable(args) => Some(args.name.clone()),
                AdapterRequest::Disable { name }
                | AdapterRequest::Remove { name, .. }
                | AdapterRequest::Rollback { name }
                | AdapterRequest::Info { name } => Some(name.clone()),
                AdapterRequest::Install(args) => Some(args.staging.clone()),
                AdapterRequest::Invoke(args) | AdapterRequest::SessionOpen(args) => {
                    Some(args.verb.clone())
                }
                AdapterRequest::List | AdapterRequest::IndexUpdate { .. } => None,
            };
            // the arguments of a relayed verb are written as a count and a digest, like a program's
            if let AdapterRequest::Invoke(args) | AdapterRequest::SessionOpen(args) = &**request {
                record.argc = Some(args.argv.len() as u64);
                record.args_sha256 = Some(audit::args_digest(&args.argv));
            }
        }
        _ => {}
    }
    record
}

/// What a request did, as a result line.
fn result_record(req: String, result: &Result<Response>, ms: u64) -> Record {
    let (ok, code, changed, failed) = summarize(result);
    let mut record = Record::new(Phase::Result);
    record.req = Some(req);
    record.ok = Some(ok);
    record.code = code;
    record.changed = Some(changed);
    record.failed = Some(failed);
    record.ms = Some(ms);
    record
}

/// (succeeded, error code, apps changed, apps that failed) of a result.
fn summarize(result: &Result<Response>) -> (bool, Option<String>, u64, u64) {
    let counts = |items: &[ItemResult]| {
        let failed = items
            .iter()
            .filter(|i| matches!(i.result, ItemOutcome::Err { .. }))
            .count() as u64;
        (changed_count(items) as u64, failed)
    };
    match result {
        Err(e) => (false, Some(e.code().as_str().to_string()), 0, 0),
        Ok(Response::Error { code, .. }) => (false, Some(code.as_str().to_string()), 0, 0),
        Ok(response) => {
            let (changed, failed) = match response {
                Response::Stopped { items, .. }
                | Response::Restarted { items, .. }
                | Response::Deleted { items, .. }
                | Response::Reloaded { items, .. }
                | Response::Flushed { items, .. }
                | Response::PolicySet { items, .. }
                | Response::StartedBatch { items } => counts(items),
                Response::Started { .. } => (1, 0),
                // a verb that exits with a failure code did not succeed
                Response::InvokeDone { exit_code, .. } => (0, u64::from(*exit_code != 0)),
                Response::Saved { count, .. } => (*count as u64, 0),
                Response::Resurrected {
                    started, failed, ..
                } => (*started as u64, failed.len() as u64),
                _ => (0, 0),
            };
            (failed == 0, None, changed, failed)
        }
    }
}
