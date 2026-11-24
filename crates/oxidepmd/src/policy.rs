//! Who may ask for what.
//!
//! Who is asking is decided from the kind of connection and the claim in the envelope
//! (`oxidepm_core::actor`); the client never names its own class. What each class may ask for is
//! decided by the tier of the request ([`classify`]) and, for a request that acts on apps, by
//! the markers of those apps.
//!
//! - A person (`human`) and the daemon's own supervision (`system`) may do everything.
//! - An `ai` actor may make every safe request (reads, on every app), and nothing dangerous
//!   (starting an app, deleting, flushing, killing, saving, resurrecting, revealing values).
//!   Its caution requests (stop, restart, reload) are decided per app by the markers.
//! - An `adapter` actor acts under its grant (`AdapterGrant`: a level and a scope, what the user
//!   accepted at `enable`): `read` makes safe requests; `lifecycle` also stops, restarts and
//!   reloads apps that are in its scope and not protected; `admin` also does the dangerous and
//!   global requests on unprotected apps in scope. `kill`, `set_policy` and `reveal` are for a
//!   person only, at any level. An adapter with no grant is `read`.

use oxidepm_adapter_proto::{HostLevel, HostScope};
use oxidepm_core::actor::{ActorClass, ActorId, Via};
use oxidepm_core::{Error, Result, Tier};
use oxidepm_ipc::{Envelope, Request};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Who is on the other end of a connection, as far as the kernel says.
#[derive(Debug, Clone, Copy, Default)]
pub struct PeerCred {
    pub pid: Option<i32>,
    pub uid: Option<u32>,
}

/// What the user allowed an adapter at `enable` (possibly less than its manifest asked for): the
/// host level and the scope. The daemon attaches it to the requests of that adapter; a client
/// cannot name one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterGrant {
    pub level: HostLevel,
    pub scope: HostScope,
}

/// Who a main-socket peer turned out to be, when an adapter process was alive at accept time.
/// A peer that is not in an adapter's process tree gets no label and stays what
/// the claim says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerLabel {
    /// The peer is the adapter or one of its descendants: it acts as that adapter, under its grant.
    Adapter { name: String, grant: AdapterGrant },
    /// The peer could not be looked at (it was gone, or the walk could not finish): stricter than
    /// a person, `ai:unresolved`.
    Unresolved,
}

/// The longest `end_user` label the audit log keeps.
pub const MAX_END_USER_CHARS: usize = oxidepm_adapter_proto::messages::MAX_END_USER_CHARS;

/// Everything the daemon knows about one request besides the request itself.
#[derive(Debug, Clone)]
pub struct ReqCtx {
    pub peer: PeerCred,
    pub request_id: String,
    /// Who is asking, as the daemon decided it.
    pub actor: ActorId,
    /// The client asked for environment values and hook scripts (honoured by `Show` and
    /// `Describe` only).
    pub reveal: bool,
    /// The client asked for a plan instead of the action. Honoured by requests that change
    /// something; `RequestHandler::is_dry_run` decides.
    pub dry_run: bool,
    /// The grant of the adapter that is asking, if the actor is one. Set by the daemon only.
    pub grant: Option<AdapterGrant>,
    /// The label an adapter gave for its own end user: written to the audit log, never used in a
    /// decision.
    pub end_user: Option<String>,
}

impl ReqCtx {
    /// The context of a request that arrived on a connection of kind `via`.
    pub fn new(peer: PeerCred, via: Via, envelope: &Envelope) -> Self {
        Self::attributed(peer, via, envelope, None)
    }

    /// Like [`new`](Self::new), for a connection whose peer was looked at: `label`
    /// replaces what the claim would have made of the request. The agent socket is `ai` by its
    /// own rule and is not relabelled.
    pub fn attributed(
        peer: PeerCred,
        via: Via,
        envelope: &Envelope,
        label: Option<&PeerLabel>,
    ) -> Self {
        let (actor, grant) = match (label, via) {
            (Some(PeerLabel::Adapter { name, grant }), Via::Main) => {
                (ActorId::adapter(name, via), Some(*grant))
            }
            (Some(PeerLabel::Unresolved), Via::Main) => (ActorId::unresolved(), None),
            _ => (
                ActorId::from_connection(via, envelope.actor.as_deref()),
                None,
            ),
        };
        Self {
            peer,
            request_id: envelope.request_id.clone(),
            actor,
            reveal: envelope.reveal,
            dry_run: envelope.dry_run,
            grant,
            end_user: None,
        }
    }

    /// The context of a `host.request`: the adapter is named by the connection (its protocol
    /// channel), the grant is the one stored for it, and nothing the adapter wrote can change
    /// either. `reveal` is never set.
    pub fn for_adapter(
        name: &str,
        grant: AdapterGrant,
        via: Via,
        request_id: String,
        dry_run: bool,
        end_user: Option<String>,
    ) -> Self {
        Self {
            peer: PeerCred::default(),
            request_id,
            actor: ActorId::adapter(name, via),
            reveal: false,
            dry_run,
            grant: Some(grant),
            end_user,
        }
    }
}

/// What a request is, for the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    pub tier: Tier,
    /// The name used in messages (`stop`, `show --reveal`).
    pub op: &'static str,
    /// The request changes something (everything but the reads).
    pub mutates: bool,
    /// The request acts on every app, not on the ones it names.
    pub global: bool,
    /// `Show` or `Describe` that asks for the real environment values and scripts.
    pub reveal: bool,
    /// Only a person may make this request, whatever an adapter was granted: `kill`,
    /// `set_policy` and `reveal` and adapter management.
    pub human_only: bool,
}

/// The tier of a request. An exhaustive `match`: a request that is added without being
/// classified does not compile.
///
/// - `Start` and `StartBatch` carry a spec, and a spec is arbitrary code: dangerous for every
///   actor that is not a person.
/// - `Show` and `Describe` with `reveal` hand out environment values and hook scripts.
pub fn classify(request: &Request, reveal: bool) -> Classified {
    let (tier, op, mutates, global) = match request {
        Request::Hello { .. } => (Tier::Safe, "hello", false, false),
        Request::Ping => (Tier::Safe, "ping", false, false),
        Request::Status => (Tier::Safe, "status", false, false),
        Request::Show { .. } if reveal => (Tier::Dangerous, "show --reveal", false, false),
        Request::Show { .. } => (Tier::Safe, "show", false, false),
        Request::Describe { .. } if reveal => (Tier::Dangerous, "describe --reveal", false, false),
        Request::Describe { .. } => (Tier::Safe, "describe", false, false),
        Request::Logs { .. } => (Tier::Safe, "logs", false, false),
        Request::DaemonStatus => (Tier::Safe, "daemon_status", false, false),
        Request::Stop { .. } => (Tier::Caution, "stop", true, false),
        Request::Restart { .. } => (Tier::Caution, "restart", true, false),
        Request::Reload { .. } => (Tier::Caution, "reload", true, false),
        Request::Start { .. } => (Tier::Dangerous, "start", true, false),
        Request::StartBatch { .. } => (Tier::Dangerous, "start_batch", true, false),
        Request::Save { .. } => (Tier::Caution, "save", true, true),
        Request::Resurrect => (Tier::Caution, "resurrect", true, true),
        Request::Delete { .. } => (Tier::Dangerous, "delete", true, false),
        Request::Flush { .. } => (Tier::Dangerous, "flush", true, false),
        Request::Kill => (Tier::Dangerous, "kill", true, true),
        // changes who may do what: a person's decision
        Request::SetPolicy { .. } => (Tier::Dangerous, "set_policy", true, false),
        // listing and describing the installed adapters is a read; everything else installs,
        // starts, stops or deletes code that runs as the user
        Request::Adapter(a) if a.is_read() => (Tier::Safe, a.op(), false, false),
        Request::Adapter(a) => (Tier::Dangerous, a.op(), true, false),
    };
    let reveal = reveal && matches!(request, Request::Show { .. } | Request::Describe { .. });
    Classified {
        tier,
        op,
        mutates,
        global,
        reveal,
        human_only: reveal
            || matches!(request, Request::Kill | Request::SetPolicy { .. })
            || matches!(request, Request::Adapter(a) if !a.is_read()),
    }
}

/// The name of a host level, as the grant and the messages write it.
pub fn level_name(level: HostLevel) -> &'static str {
    match level {
        HostLevel::Read => "read",
        HostLevel::Lifecycle => "lifecycle",
        HostLevel::Admin => "admin",
    }
}

/// The word for a verb's tier in a message.
pub fn tier_word(tier: oxidepm_adapter_proto::VerbTier) -> &'static str {
    match tier {
        oxidepm_adapter_proto::VerbTier::Safe => "safe",
        oxidepm_adapter_proto::VerbTier::Caution => "caution",
        oxidepm_adapter_proto::VerbTier::Dangerous => "dangerous",
    }
}

/// What an adapter's grant allows, before anything is read: `Err` is the reason. A safe
/// request is open to every grant; one that only a person may make is closed to every grant; a
/// caution request on named apps needs `lifecycle`; a dangerous request, or one that acts on
/// every app (`save`, `resurrect`), needs `admin`, and the global ones also a scope of `all`.
fn adapter_stage1(
    grant: Option<&AdapterGrant>,
    classified: &Classified,
) -> std::result::Result<(), String> {
    if classified.human_only {
        return Err("only a person may make it".to_string());
    }
    if classified.tier == Tier::Safe {
        return Ok(());
    }
    let level = grant.map_or(HostLevel::Read, |g| g.level);
    let needs = match (classified.tier, classified.global) {
        (Tier::Caution, false) => HostLevel::Lifecycle,
        _ => HostLevel::Admin,
    };
    if level < needs {
        return Err(format!(
            "its grant is '{}' and the request needs '{}'",
            level_name(level),
            level_name(needs)
        ));
    }
    if classified.global && grant.map_or(true, |g| g.scope != HostScope::All) {
        return Err(
            "it acts on every app and the grant covers only the apps that list the adapter"
                .to_string(),
        );
    }
    Ok(())
}

/// Stage 1 of the gate: the actor's class against the tier of the request, before anything is
/// read. A caution request of an `ai` actor passes (the markers of the apps decide it later);
/// a global caution request (`save`, `resurrect`) acts on every app and never passes. An adapter
/// is held to its `grant` (`None` is `read`).
pub fn stage1(
    actor: &ActorId,
    grant: Option<&AdapterGrant>,
    classified: &Classified,
) -> Result<()> {
    let allowed = match actor.class {
        ActorClass::Human | ActorClass::System => true,
        ActorClass::Ai => match classified.tier {
            Tier::Safe => true,
            Tier::Caution => !classified.global,
            Tier::Dangerous => false,
        },
        ActorClass::Adapter => match adapter_stage1(grant, classified) {
            Ok(()) => true,
            Err(why) => {
                return Err(Error::PolicyDenied(format!(
                    "{} may not make the request '{}': {}",
                    actor.wire(),
                    classified.op,
                    why
                )))
            }
        },
    };
    if allowed {
        return Ok(());
    }
    let why = if classified.global && classified.tier == Tier::Caution {
        "it acts on every app".to_string()
    } else {
        format!("it is a {} request", classified.tier.as_str())
    };
    Err(Error::PolicyDenied(format!(
        "{} may not make the request '{}': {}. Ask a person to run it",
        actor.wire(),
        classified.op,
        why
    )))
}

/// Reads are logged at debug (a monitor polls them every second); everything else at info.
fn is_read(request: &Request) -> bool {
    matches!(
        request,
        Request::Ping
            | Request::Hello { .. }
            | Request::Status
            | Request::DaemonStatus
            | Request::Show { .. }
            | Request::Logs { .. }
            | Request::Describe { .. }
    ) || matches!(request, Request::Adapter(a) if a.is_read())
}

/// Called once per request before it is dispatched: logs the request with its actor.
pub fn log_request(ctx: &ReqCtx, request: &Request) {
    let pid = ctx
        .peer
        .pid
        .map_or_else(|| "?".to_string(), |p| p.to_string());
    let line = format!(
        "request {} {} actor={} class={} via={} pid={}",
        ctx.request_id,
        request.name(),
        ctx.actor.wire(),
        ctx.actor.class.as_str(),
        ctx.actor.via.as_str(),
        pid
    );
    if is_read(request) {
        debug!("{}", line);
    } else {
        info!("{}", line);
    }
}

/// What the gate needs to know about one app a request acts on, read from the markers on the
/// handle (the one source of truth) and, for an instance, the markers of its cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetView {
    pub id: u32,
    pub name: String,
    /// The app or, for an instance, its cluster is protected.
    pub protected: bool,
    /// The app and, for an instance, its cluster both allow `ai` control.
    pub ai_control: bool,
    /// The names of the adapters the app (or, for an instance, its cluster) lists under
    /// `adapters`: what an `opted-in` scope is measured against.
    pub adapters: Vec<String>,
}

/// The proof that a request passed the gate, carrying the apps it may act on. It can only be
/// made by [`authorize`]: the handler's one public way to act, `RequestHandler::execute`, takes
/// it, and acts on `ids` and on nothing else, so a request cannot touch an app that was not
/// looked at (an app that gained a tag, or was created, between the check and the action).
#[derive(Debug)]
pub struct Authorized {
    ids: Vec<u32>,
    reveal: bool,
    /// The asker is not a person (this is how the audit log and the limiter count it): what it
    /// reads is scrubbed and bounded.
    limited_reader: Option<String>,
}

impl Authorized {
    /// The request may show environment values and scripts (only a person gets this).
    pub fn reveal(&self) -> bool {
        self.reveal
    }

    /// The asker is an AI or an adapter (the key it is counted under): `logs` are scrubbed of
    /// the app's environment values and bounded.
    pub fn limited_reader(&self) -> Option<&str> {
        self.limited_reader.as_deref()
    }

    /// The apps the request named, resolved once (a cluster is its parent; its instances were
    /// checked with it).
    pub fn ids(&self) -> &[u32] {
        &self.ids
    }
}

/// Most apps one request of a non-human actor may name.
pub const MAX_NAMED_APPS: usize = 10;

/// Is `selector` one an `ai` actor may use: a name or an id (`all` and tags name more than the
/// actor looked at)?
pub fn selector_is_named(selector: &oxidepm_core::Selector) -> bool {
    matches!(
        selector,
        oxidepm_core::Selector::ById(_) | oxidepm_core::Selector::ByName(_)
    )
}

/// The refusal of a selector that is not a name or an id.
pub fn name_the_apps(actor: &ActorId, classified: &Classified) -> Error {
    Error::PolicyDenied(format!(
        "{} may not use 'all' or a tag with '{}': name the apps",
        actor.wire(),
        classified.op
    ))
}

/// Stage 2 of the gate: the actor against the apps the request acts on. `ids` are the apps the
/// selector named, `targets` those and their instances (`TargetView`s). For an actor that is not
/// a person the check is all or nothing: one denied app refuses the request and the error names
/// the denied apps.
///
/// - A protected app is read-only for a non-human actor: `E_PROTECTED`, whatever `ai_control`
///   says.
/// - An `ai` actor needs `ai_control` on an app to stop, restart or reload it, and names at most
///   [`MAX_NAMED_APPS`] apps.
/// - An adapter whose grant has the scope `opted-in` acts only on apps that list it under
///   `adapters`.
/// - Persons and the daemon's own supervision are never limited here.
pub fn authorize(
    actor: &ActorId,
    grant: Option<&AdapterGrant>,
    classified: &Classified,
    ids: Vec<u32>,
    targets: &[TargetView],
) -> Result<Authorized> {
    stage1(actor, grant, classified)?;
    let human = matches!(actor.class, ActorClass::Human | ActorClass::System);
    let allowed = |ids| Authorized {
        ids,
        reveal: classified.reveal && human,
        limited_reader: (!human).then(|| crate::audit::actor_key(actor)),
    };
    if human || !classified.mutates {
        return Ok(allowed(ids));
    }
    if actor.class == ActorClass::Ai && ids.len() > MAX_NAMED_APPS {
        return Err(Error::PolicyDenied(format!(
            "{} may name at most {} apps in one request (this one names {})",
            actor.wire(),
            MAX_NAMED_APPS,
            ids.len()
        )));
    }
    let names = |views: Vec<&TargetView>| {
        views
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let protected: Vec<&TargetView> = targets.iter().filter(|t| t.protected).collect();
    if !protected.is_empty() {
        return Err(Error::Protected(format!(
            "{} may not {} protected app(s): {}. A protected app is read-only for anything but a person",
            actor.wire(),
            classified.op,
            names(protected)
        )));
    }
    match actor.class {
        ActorClass::Ai => {
            let closed: Vec<&TargetView> = targets.iter().filter(|t| !t.ai_control).collect();
            if !closed.is_empty() {
                return Err(Error::PolicyDenied(format!(
                    "{} may not {} app(s) without ai_control: {}. Nothing was changed",
                    actor.wire(),
                    classified.op,
                    names(closed)
                )));
            }
        }
        ActorClass::Adapter => {
            if grant.is_some_and(|g| g.scope == HostScope::OptedIn) {
                let outside: Vec<&TargetView> = targets
                    .iter()
                    .filter(|t| !t.adapters.iter().any(|a| a.as_str() == &*actor.label))
                    .collect();
                if !outside.is_empty() {
                    return Err(outside_scope(actor, classified, &names(outside)));
                }
            }
        }
        ActorClass::Human | ActorClass::System => {}
    }
    Ok(allowed(ids))
}

fn outside_scope(actor: &ActorId, classified: &Classified, apps: &str) -> Error {
    Error::PolicyDenied(format!(
        "{} may not {} app(s) that do not list it under 'adapters' (its scope is opted-in): {}. Nothing was changed",
        actor.wire(),
        classified.op,
        apps
    ))
}

/// The scope check for the specs of a `start` that name no app yet: an adapter with the scope
/// `opted-in` may create only apps that list it. `new_apps` are (name, lists the adapter).
pub fn authorize_new_apps(
    actor: &ActorId,
    grant: Option<&AdapterGrant>,
    classified: &Classified,
    new_apps: &[(String, bool)],
) -> Result<()> {
    if actor.class != ActorClass::Adapter || !grant.is_some_and(|g| g.scope == HostScope::OptedIn) {
        return Ok(());
    }
    let outside: Vec<&str> = new_apps
        .iter()
        .filter(|(_, listed)| !listed)
        .map(|(n, _)| n.as_str())
        .collect();
    if outside.is_empty() {
        return Ok(());
    }
    Err(outside_scope(actor, classified, &outside.join(", ")))
}

/// How soon a non-human actor may restart or reload the same app again, and how often.
pub const RESTART_COOLDOWN: Duration = Duration::from_secs(10);
pub const RESTART_WINDOW: Duration = Duration::from_secs(600);
pub const RESTARTS_PER_WINDOW: usize = 5;

/// The restarts and reloads non-human actors made, per app, so that a looping agent cannot keep
/// one app permanently down. Time is passed in.
#[derive(Debug, Default)]
pub struct RestartLimiter {
    history: HashMap<u32, VecDeque<Instant>>,
}

impl RestartLimiter {
    /// Record a restart or reload of every app in `ids` at `now`, or refuse all of them: the
    /// error names the first app that is inside its cooldown or over its count, and how long to
    /// wait. A refused request records nothing.
    pub fn admit(&mut self, ids: &[u32], now: Instant) -> std::result::Result<(), (u32, Duration)> {
        self.check(ids, now)?;
        for id in ids {
            self.history.entry(*id).or_default().push_back(now);
        }
        Ok(())
    }

    /// What [`admit`](Self::admit) would answer at `now`, without recording anything (a dry run
    /// must not use up the allowance of the real request).
    pub fn check(&mut self, ids: &[u32], now: Instant) -> std::result::Result<(), (u32, Duration)> {
        self.history.retain(|_, times| {
            while times
                .front()
                .is_some_and(|t| now.duration_since(*t) >= RESTART_WINDOW)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        for id in ids {
            let Some(times) = self.history.get(id) else {
                continue;
            };
            if let Some(last) = times.back() {
                let since = now.duration_since(*last);
                if since < RESTART_COOLDOWN {
                    return Err((*id, RESTART_COOLDOWN - since));
                }
            }
            if times.len() >= RESTARTS_PER_WINDOW {
                let oldest = times.front().copied().unwrap_or(now);
                return Err((
                    *id,
                    RESTART_WINDOW.saturating_sub(now.duration_since(oldest)),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::{AppSpec, Runtime, Selector};
    use oxidepm_ipc::AdapterRequest;

    fn spec() -> AppSpec {
        AppSpec::new("a".into(), Runtime::CMD, "true".into(), "/".into())
    }

    fn sel() -> Selector {
        Selector::ByName("a".into())
    }

    /// Every request there is, with the tier it must have. The helper below has no wildcard arm,
    /// so a request that is added without being listed here does not compile.
    fn all_requests() -> Vec<(Request, Tier, bool)> {
        fn _every_variant_is_handled(request: &Request) {
            match request {
                Request::Hello { .. }
                | Request::Ping
                | Request::Start { .. }
                | Request::StartBatch { .. }
                | Request::Stop { .. }
                | Request::Restart { .. }
                | Request::Delete { .. }
                | Request::Status
                | Request::Show { .. }
                | Request::Logs { .. }
                | Request::Save { .. }
                | Request::Resurrect
                | Request::Kill
                | Request::Reload { .. }
                | Request::Flush { .. }
                | Request::Describe { .. }
                | Request::SetPolicy { .. }
                | Request::Adapter(_)
                | Request::DaemonStatus => {}
            }
            fn _every_adapter_request_is_handled(request: &AdapterRequest) {
                match request {
                    AdapterRequest::Install(_)
                    | AdapterRequest::Enable(_)
                    | AdapterRequest::Disable { .. }
                    | AdapterRequest::Remove { .. }
                    | AdapterRequest::Rollback { .. }
                    | AdapterRequest::List
                    | AdapterRequest::Info { .. }
                    | AdapterRequest::IndexUpdate { .. }
                    | AdapterRequest::Invoke(_)
                    | AdapterRequest::SessionOpen(_) => {}
                }
            }
        }
        vec![
            (Request::Hello { client: "t".into() }, Tier::Safe, false),
            (Request::Ping, Tier::Safe, false),
            (Request::Status, Tier::Safe, false),
            (Request::DaemonStatus, Tier::Safe, false),
            (Request::Show { selector: sel() }, Tier::Safe, false),
            (Request::Describe { selector: sel() }, Tier::Safe, false),
            (
                Request::Logs {
                    selector: sel(),
                    lines: 1,
                    follow: false,
                    stdout: true,
                    stderr: true,
                },
                Tier::Safe,
                false,
            ),
            (Request::Stop { selector: sel() }, Tier::Caution, true),
            (Request::Restart { selector: sel() }, Tier::Caution, true),
            (Request::Reload { selector: sel() }, Tier::Caution, true),
            (Request::Save { force: false }, Tier::Caution, true),
            (Request::Resurrect, Tier::Caution, true),
            (Request::Start { spec: spec() }, Tier::Dangerous, true),
            (Request::StartBatch { specs: vec![] }, Tier::Dangerous, true),
            (Request::Delete { selector: sel() }, Tier::Dangerous, true),
            (Request::Flush { selector: sel() }, Tier::Dangerous, true),
            (Request::Kill, Tier::Dangerous, true),
            (
                Request::SetPolicy {
                    selector: sel(),
                    protected: Some(true),
                    ai_control: None,
                },
                Tier::Dangerous,
                true,
            ),
            (
                Request::Adapter(Box::new(AdapterRequest::List)),
                Tier::Safe,
                false,
            ),
            (
                Request::Adapter(Box::new(AdapterRequest::Info { name: "a".into() })),
                Tier::Safe,
                false,
            ),
            (
                Request::Adapter(Box::new(AdapterRequest::Install(Box::default()))),
                Tier::Dangerous,
                true,
            ),
            (
                Request::Adapter(Box::new(AdapterRequest::Enable(Box::default()))),
                Tier::Dangerous,
                true,
            ),
            (
                Request::Adapter(Box::new(AdapterRequest::Disable { name: "a".into() })),
                Tier::Dangerous,
                true,
            ),
            (
                Request::Adapter(Box::new(AdapterRequest::Remove {
                    name: "a".into(),
                    purge: false,
                })),
                Tier::Dangerous,
                true,
            ),
            (
                Request::Adapter(Box::new(AdapterRequest::Rollback { name: "a".into() })),
                Tier::Dangerous,
                true,
            ),
            (
                Request::Adapter(Box::new(AdapterRequest::IndexUpdate {
                    index_bytes: String::new(),
                    index_sig: String::new(),
                })),
                Tier::Dangerous,
                true,
            ),
            // a verb's real tier is its manifest's; asked for by the generic path (an adapter's
            // host.request, say) it is a person's only
            (
                Request::Adapter(Box::new(AdapterRequest::Invoke(Box::default()))),
                Tier::Dangerous,
                true,
            ),
            // opening a session is a person's request too; the one exception is made where the
            // verb is known (an `ai` claim may open a verb its adapter marks `ai_driven`)
            (
                Request::Adapter(Box::new(AdapterRequest::SessionOpen(Box::default()))),
                Tier::Dangerous,
                true,
            ),
        ]
    }

    #[test]
    fn tier_of_every_request_is_listed() {
        let all = all_requests();
        let mut names: Vec<&str> = all.iter().map(|(r, ..)| r.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 19, "one entry per request: {names:?}");
        for (request, tier, mutates) in &all {
            let c = classify(request, false);
            assert_eq!((c.tier, c.mutates), (*tier, *mutates), "{}", request.name());
            match request {
                // an adapter request names its operation
                Request::Adapter(a) => assert_eq!(c.op, a.op()),
                _ => assert_eq!(c.op, request.name(), "the op is the wire name"),
            }
            // everything that changes adapters is a person's request, whatever an adapter was granted
            assert_eq!(
                c.human_only,
                matches!(request, Request::Adapter(a) if !a.is_read())
                    || matches!(request, Request::Kill | Request::SetPolicy { .. })
            );
        }
    }

    #[test]
    fn reveal_makes_show_and_describe_dangerous() {
        for request in [
            Request::Show { selector: sel() },
            Request::Describe { selector: sel() },
        ] {
            assert_eq!(classify(&request, false).tier, Tier::Safe);
            assert_eq!(classify(&request, true).tier, Tier::Dangerous);
        }
        // the flag means nothing to the other requests
        assert_eq!(classify(&Request::Status, true).tier, Tier::Safe);
    }

    #[test]
    fn stage1_by_class_and_tier() {
        let actor = |class| ActorId::new(class, "x", Via::Main);
        let tiers = [
            (Request::Status, Tier::Safe),
            (Request::Stop { selector: sel() }, Tier::Caution),
            (Request::Save { force: false }, Tier::Caution),
            (Request::Delete { selector: sel() }, Tier::Dangerous),
        ];
        // (class, [status, stop, save, delete] allowed)
        let matrix = [
            (ActorClass::Human, [true, true, true, true]),
            (ActorClass::System, [true, true, true, true]),
            (ActorClass::Ai, [true, true, false, false]),
            (ActorClass::Adapter, [true, false, false, false]),
        ];
        for (class, allowed) in matrix {
            for ((request, tier), want) in tiers.iter().zip(allowed) {
                let c = classify(request, false);
                assert_eq!(c.tier, *tier);
                let got = stage1(&actor(class), None, &c);
                assert_eq!(got.is_ok(), want, "{class:?} {}: {got:?}", request.name());
                if let Err(e) = got {
                    assert_eq!(e.code(), oxidepm_core::ErrorCode::PolicyDenied);
                }
            }
        }
    }

    fn view(id: u32, name: &str, protected: bool, ai_control: bool) -> TargetView {
        TargetView {
            id,
            name: name.into(),
            protected,
            ai_control,
            adapters: Vec::new(),
        }
    }

    fn ai() -> ActorId {
        ActorId::new(ActorClass::Ai, "test", Via::Main)
    }

    fn stop_class() -> Classified {
        classify(&Request::Stop { selector: sel() }, false)
    }

    #[test]
    fn authorize_is_all_or_nothing_and_names_the_denied_apps() {
        let ok = view(1, "open", false, true);
        let closed = view(2, "closed", false, false);
        let also_closed = view(3, "also-closed", false, false);

        let auth = authorize(
            &ai(),
            None,
            &stop_class(),
            vec![1],
            std::slice::from_ref(&ok),
        )
        .unwrap();
        assert_eq!(auth.ids(), &[1]);

        let err = authorize(
            &ai(),
            None,
            &stop_class(),
            vec![1, 2, 3],
            &[ok.clone(), closed, also_closed],
        )
        .unwrap_err();
        assert_eq!(err.code(), oxidepm_core::ErrorCode::PolicyDenied);
        let text = err.to_string();
        assert!(
            text.contains("closed") && text.contains("also-closed"),
            "{text}"
        );
        assert!(
            !text.contains("open,") && !text.contains(": open"),
            "the allowed app is not named: {text}"
        );
    }

    #[test]
    fn protected_wins_over_ai_control_and_is_named() {
        let both = view(1, "both", true, true);
        let err = authorize(&ai(), None, &stop_class(), vec![1], &[both]).unwrap_err();
        assert_eq!(err.code(), oxidepm_core::ErrorCode::Protected);
        assert!(err.to_string().contains("both"), "{err}");
        // a protected app and a closed one: protected is what the error says
        let err = authorize(
            &ai(),
            None,
            &stop_class(),
            vec![1, 2],
            &[view(1, "p", true, true), view(2, "c", false, false)],
        )
        .unwrap_err();
        assert_eq!(err.code(), oxidepm_core::ErrorCode::Protected);
    }

    #[test]
    fn humans_and_system_are_never_limited_by_markers() {
        let all_denied = [view(1, "p", true, false), view(2, "c", false, false)];
        for class in [ActorClass::Human, ActorClass::System] {
            let actor = ActorId::new(class, "x", Via::Main);
            let auth = authorize(&actor, None, &stop_class(), vec![1, 2], &all_denied).unwrap();
            assert_eq!(auth.ids(), &[1, 2]);
        }
    }

    #[test]
    fn an_ai_actor_names_at_most_ten_apps() {
        let ids: Vec<u32> = (1..=11).collect();
        let views: Vec<TargetView> = ids
            .iter()
            .map(|i| view(*i, &format!("a{i}"), false, true))
            .collect();
        assert!(authorize(&ai(), None, &stop_class(), ids[..10].to_vec(), &views[..10]).is_ok());
        let err = authorize(&ai(), None, &stop_class(), ids, &views).unwrap_err();
        assert_eq!(err.code(), oxidepm_core::ErrorCode::PolicyDenied);
        assert!(err.to_string().contains("at most 10"), "{err}");
    }

    #[test]
    fn reads_need_no_marker() {
        let c = classify(&Request::Status, false);
        let auth = authorize(&ai(), None, &c, vec![], &[view(1, "p", true, false)]).unwrap();
        assert!(auth.ids().is_empty() && !auth.reveal());
        // reveal is carried only for a request a person made
        let c = classify(&Request::Show { selector: sel() }, true);
        let human = ActorId::new(ActorClass::Human, "cli", Via::Main);
        assert!(authorize(&human, None, &c, vec![], &[]).unwrap().reveal());
        assert!(authorize(&ai(), None, &c, vec![], &[]).is_err());
    }

    #[test]
    fn only_names_and_ids_are_named_apps() {
        assert!(selector_is_named(&Selector::ByName("a".into())));
        assert!(selector_is_named(&Selector::ById(3)));
        assert!(!selector_is_named(&Selector::All));
        assert!(!selector_is_named(&Selector::ByTag("t".into())));
    }

    #[test]
    fn restart_limiter_cooldown_and_window() {
        let mut limiter = RestartLimiter::default();
        let t0 = Instant::now();
        assert!(limiter.admit(&[1], t0).is_ok());
        // inside the cooldown: refused, and the wait is what is left
        let (id, wait) = limiter
            .admit(&[1], t0 + Duration::from_secs(3))
            .unwrap_err();
        assert_eq!((id, wait), (1, Duration::from_secs(7)));
        // another app is independent
        assert!(limiter.admit(&[2], t0 + Duration::from_secs(3)).is_ok());
        // after the cooldown the same app may go on, up to five in ten minutes
        for n in 1..5 {
            assert!(
                limiter
                    .admit(&[1], t0 + Duration::from_secs(10 * n))
                    .is_ok(),
                "restart {}",
                n + 1
            );
        }
        let (id, wait) = limiter
            .admit(&[1], t0 + Duration::from_secs(60))
            .unwrap_err();
        assert_eq!(id, 1);
        assert_eq!(
            wait,
            Duration::from_secs(540),
            "until the oldest leaves the window"
        );
        // ten minutes after the first, one place is free
        assert!(limiter.admit(&[1], t0 + RESTART_WINDOW).is_ok());
    }

    #[test]
    fn a_refused_request_records_nothing() {
        let mut limiter = RestartLimiter::default();
        let t0 = Instant::now();
        assert!(limiter.admit(&[1], t0).is_ok());
        // app 2 is fine but app 1 is in its cooldown: nothing is recorded for either
        assert!(limiter.admit(&[2, 1], t0 + Duration::from_secs(1)).is_err());
        assert!(limiter.admit(&[2], t0 + Duration::from_secs(2)).is_ok());
        // and the history of an app that has not been touched for ten minutes is dropped
        assert!(limiter
            .admit(&[3], t0 + RESTART_WINDOW + Duration::from_secs(3))
            .is_ok());
        assert_eq!(limiter.history.len(), 1);
    }

    // ---- adapters: the grant ---------------------------------------------------------

    fn grant(level: HostLevel, scope: HostScope) -> AdapterGrant {
        AdapterGrant { level, scope }
    }

    fn adapter(name: &str) -> ActorId {
        ActorId::new(ActorClass::Adapter, name, Via::AdapterChannel)
    }

    fn viewed(id: u32, name: &str, protected: bool, adapters: &[&str]) -> TargetView {
        TargetView {
            id,
            name: name.into(),
            protected,
            ai_control: false,
            adapters: adapters.iter().map(|a| a.to_string()).collect(),
        }
    }

    #[test]
    fn adapter_stage1_follows_the_grant() {
        use HostLevel::{Admin, Lifecycle, Read};
        let requests = [
            ("status", Request::Status),
            ("stop", Request::Stop { selector: sel() }),
            ("save", Request::Save { force: false }),
            ("delete", Request::Delete { selector: sel() }),
            ("start", Request::Start { spec: spec() }),
            ("kill", Request::Kill),
            (
                "set_policy",
                Request::SetPolicy {
                    selector: sel(),
                    protected: Some(false),
                    ai_control: None,
                },
            ),
            ("show --reveal", Request::Show { selector: sel() }),
        ];
        // (grant, [status, stop, save, delete, start, kill, set_policy, show --reveal] allowed)
        let matrix = [
            (
                None,
                [true, false, false, false, false, false, false, false],
            ),
            (
                Some(grant(Read, HostScope::All)),
                [true, false, false, false, false, false, false, false],
            ),
            (
                Some(grant(Lifecycle, HostScope::All)),
                [true, true, false, false, false, false, false, false],
            ),
            (
                Some(grant(Admin, HostScope::All)),
                [true, true, true, true, true, false, false, false],
            ),
            // a global request needs the whole fleet in scope
            (
                Some(grant(Admin, HostScope::OptedIn)),
                [true, true, false, true, true, false, false, false],
            ),
        ];
        for (grant, allowed) in matrix {
            for ((name, request), want) in requests.iter().zip(allowed) {
                let c = classify(request, *name == "show --reveal");
                let got = stage1(&adapter("ui"), grant.as_ref(), &c);
                assert_eq!(got.is_ok(), want, "{grant:?} {name}: {got:?}");
                if let Err(e) = got {
                    assert_eq!(e.code(), oxidepm_core::ErrorCode::PolicyDenied);
                    assert!(e.to_string().contains("adapter:ui"), "{e}");
                }
            }
        }
    }

    #[test]
    fn human_only_requests_are_closed_to_every_grant() {
        for request in [
            Request::Kill,
            Request::SetPolicy {
                selector: sel(),
                protected: None,
                ai_control: Some(true),
            },
        ] {
            assert!(classify(&request, false).human_only, "{}", request.name());
        }
        assert!(
            classify(&Request::Show { selector: sel() }, true).human_only,
            "reveal"
        );
        assert!(!classify(&Request::Show { selector: sel() }, false).human_only);
        assert!(!classify(&Request::Stop { selector: sel() }, false).human_only);
    }

    #[test]
    fn lifecycle_adapter_with_scope_all_acts_on_unprotected_apps() {
        let g = grant(HostLevel::Lifecycle, HostScope::All);
        let open = viewed(1, "open", false, &[]);
        let auth = authorize(
            &adapter("ui"),
            Some(&g),
            &stop_class(),
            vec![1],
            std::slice::from_ref(&open),
        )
        .unwrap();
        assert_eq!(auth.ids(), &[1]);
        assert!(
            auth.limited_reader().is_some_and(|k| k == "adapter:ui"),
            "an adapter reads scrubbed and bounded"
        );
        // more than ten apps are fine for an adapter (the cap is an AI's)
        let ids: Vec<u32> = (1..=25).collect();
        let views: Vec<TargetView> = ids
            .iter()
            .map(|i| viewed(*i, &format!("a{i}"), false, &[]))
            .collect();
        assert!(authorize(&adapter("ui"), Some(&g), &stop_class(), ids, &views).is_ok());
    }

    #[test]
    fn protected_app_is_read_only_for_an_adapter_and_is_named() {
        for level in [HostLevel::Lifecycle, HostLevel::Admin] {
            let g = grant(level, HostScope::All);
            let err = authorize(
                &adapter("ui"),
                Some(&g),
                &stop_class(),
                vec![1, 2],
                &[viewed(1, "free", false, &[]), viewed(2, "vault", true, &[])],
            )
            .unwrap_err();
            assert_eq!(err.code(), oxidepm_core::ErrorCode::Protected);
            let text = err.to_string();
            assert!(text.contains("vault") && !text.contains("free"), "{text}");
        }
    }

    #[test]
    fn opted_in_scope_reaches_only_apps_that_list_the_adapter() {
        let g = grant(HostLevel::Lifecycle, HostScope::OptedIn);
        let listed = viewed(1, "mine", false, &["watch", "other"]);
        let unlisted = viewed(2, "theirs", false, &["other"]);
        assert!(authorize(
            &adapter("watch"),
            Some(&g),
            &stop_class(),
            vec![1],
            std::slice::from_ref(&listed)
        )
        .is_ok());
        let err = authorize(
            &adapter("watch"),
            Some(&g),
            &stop_class(),
            vec![1, 2],
            &[listed, unlisted],
        )
        .unwrap_err();
        assert_eq!(err.code(), oxidepm_core::ErrorCode::PolicyDenied);
        let text = err.to_string();
        assert!(text.contains("theirs") && !text.contains("mine"), "{text}");
        // the scope is about this adapter's name, not about any adapter
        assert!(authorize(
            &adapter("watch"),
            Some(&g),
            &stop_class(),
            vec![2],
            &[viewed(2, "theirs", false, &["watcher"])]
        )
        .is_err());
    }

    #[test]
    fn opted_in_scope_limits_new_apps_to_those_that_list_the_adapter() {
        let c = classify(&Request::Start { spec: spec() }, false);
        let all = grant(HostLevel::Admin, HostScope::All);
        let opted = grant(HostLevel::Admin, HostScope::OptedIn);
        let new_apps = [("a".to_string(), true), ("b".to_string(), false)];
        assert!(authorize_new_apps(&adapter("watch"), Some(&all), &c, &new_apps).is_ok());
        let err = authorize_new_apps(&adapter("watch"), Some(&opted), &c, &new_apps).unwrap_err();
        assert!(
            err.to_string().contains('b') && !err.to_string().contains(": a"),
            "{err}"
        );
        assert!(authorize_new_apps(&adapter("watch"), Some(&opted), &c, &new_apps[..1]).is_ok());
    }

    #[test]
    fn a_peer_label_makes_the_connection_an_adapter_or_unresolved_never_a_person() {
        let envelope = Envelope::new("r", Request::Status);
        let mut claiming = envelope.clone();
        claiming.actor = Some("human:root".to_string());
        let g = grant(HostLevel::Lifecycle, HostScope::OptedIn);
        let label = PeerLabel::Adapter {
            name: "ui".to_string(),
            grant: g,
        };

        let ctx = ReqCtx::attributed(PeerCred::default(), Via::Main, &claiming, Some(&label));
        assert_eq!(
            (ctx.actor.wire().as_str(), ctx.actor.via, ctx.grant),
            ("adapter:ui", Via::Main, Some(g))
        );
        assert!(!ctx.reveal && ctx.end_user.is_none());

        let ctx = ReqCtx::attributed(
            PeerCred::default(),
            Via::Main,
            &claiming,
            Some(&PeerLabel::Unresolved),
        );
        assert_eq!(
            (ctx.actor.wire().as_str(), ctx.actor.via, ctx.grant),
            ("ai:unresolved", Via::Unattributed, None)
        );

        // no label: the claim decides, as it did
        let ctx = ReqCtx::attributed(PeerCred::default(), Via::Main, &claiming, None);
        assert_eq!(ctx.actor.wire(), "human:root");
        // the agent socket is ai by its own rule and is not relabelled
        let ctx = ReqCtx::attributed(PeerCred::default(), Via::Agent, &envelope, Some(&label));
        assert_eq!((ctx.actor.class, ctx.grant), (ActorClass::Ai, None));

        // a host.request is the adapter on its channel, with the grant stored for it
        let ctx = ReqCtx::for_adapter(
            "ui",
            g,
            Via::AdapterChannel,
            "ui#1".into(),
            true,
            Some("alice".into()),
        );
        assert_eq!(
            (
                ctx.actor.wire().as_str(),
                ctx.actor.via,
                ctx.grant,
                ctx.reveal,
                ctx.dry_run
            ),
            ("adapter:ui", Via::AdapterChannel, Some(g), false, true)
        );
    }
}
