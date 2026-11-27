//! The host API: what an adapter may ask of the daemon through `host.request`
//! and `host.audit_note` (27a), and who the peer of a main-socket connection is while an adapter
//! runs (27b).
//!
//! Three kinds of test. Most run the real policy, audit log and supervisor in the process, with
//! the adapter's host API called as the connection calls it (the daemon's request entry point is
//! `RequestHandler::guarded`, the part of `Core::dispatch` that decides). A few put the fixture
//! adapter on a pipe in front of the host API. The ones that need the daemon's own sockets, its
//! shutdown or its boot run the real `oxidepmd` with an installed fixture service.

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::daemon::{DaemonProc, Signal};
use common::fixture::{
    install, launcher_bin, wait_adapter_state, wait_state, wait_until, Fixture, InstallSpec,
};
use common::*;
use oxidepm_adapter_proto::codes::rpc;
use oxidepm_adapter_proto::{HostLevel, HostScope, Params};
use oxidepm_core::{ErrorCode, Paths};
use oxidepm_ipc::{Request, Response};
use oxidepmd::adapters::conn::{CloseReason, ConnConfig};
use oxidepmd::adapters::entry::EntryState;
use oxidepmd::adapters::host::{AdapterHost, HostConfig};
use oxidepmd::adapters::hostapi::{Dispatch, HostApi, HostLink};
use oxidepmd::audit::{Audit, AuditConfig, Rates};
use oxidepmd::handlers::RequestHandler;
use oxidepmd::policy::{AdapterGrant, PeerLabel, ReqCtx};
use oxidepmd::supervisor::CURRENT_ACTOR;
use parking_lot::Mutex;
use serde_json::{json, Value};
use tempfile::TempDir;

// ---- helpers ---------------------------------------------------------------------------------

fn grant(level: HostLevel, scope: HostScope) -> AdapterGrant {
    AdapterGrant { level, scope }
}

fn by_name(name: &str) -> Value {
    json!({ "by": "name", "value": name })
}

fn stop(name: &str) -> Value {
    json!({ "type": "stop", "selector": by_name(name) })
}

/// The daemon's entry point for a test that runs the policy and the audit log in the process:
/// what `Core::dispatch` does for a request that is not `ping`, `hello` or `kill`.
struct Local {
    handler: Arc<RequestHandler>,
    seen: Mutex<Vec<ReqCtx>>,
}

#[async_trait]
impl Dispatch for Local {
    async fn dispatch(&self, ctx: &ReqCtx, request: Request) -> Response {
        self.seen.lock().push(ctx.clone());
        CURRENT_ACTOR
            .scope(
                Arc::from(ctx.actor.wire()),
                self.handler.guarded(ctx, request),
            )
            .await
            .unwrap_or_else(|e| Response::from_error(&e))
    }
}

/// The host API of one adapter over a real supervisor and a real audit log in a private home.
struct Rig {
    h: Harness,
    api: Arc<HostApi>,
    local: Arc<Local>,
    audit: Arc<Audit>,
}

fn quick(paths: &Paths, rates: Option<Rates>) -> AuditConfig {
    let mut config = AuditConfig::new(paths);
    for writer in [&mut config.main, &mut config.security] {
        writer.retry = Duration::from_millis(60);
        writer.sync_interval = Duration::from_millis(20);
    }
    if let Some(rates) = rates {
        config.rates = rates;
    }
    config
}

impl Rig {
    async fn new(name: &str, g: AdapterGrant) -> Rig {
        Rig::with_rates(name, g, None).await
    }

    async fn with_rates(name: &str, g: AdapterGrant, rates: Option<Rates>) -> Rig {
        let h = Harness::new().await;
        let audit = Arc::new(Audit::with_config(quick(&h.paths, rates)));
        let handler = Arc::new(RequestHandler::with_audit(
            Arc::clone(&h.sup),
            Arc::clone(&audit),
        ));
        let local = Arc::new(Local {
            handler,
            seen: Mutex::new(Vec::new()),
        });
        let link = HostLink::new();
        link.bind(Arc::clone(&local) as Arc<dyn Dispatch>);
        let api = HostApi::new(name, g, link, Some(Arc::clone(&audit)));
        Rig {
            h,
            api,
            local,
            audit,
        }
    }

    /// `host.request` with these params; the answer's `response`.
    async fn call(&self, params: Value) -> Value {
        let Value::Object(params) = params else {
            panic!("params are an object")
        };
        use oxidepmd::adapters::conn::Inbound;
        let result = self
            .api
            .request("host.request", params)
            .await
            .unwrap_or_else(|e| panic!("host.request failed: {e:?}"));
        result["response"].clone()
    }

    async fn ask(&self, request: Value) -> Value {
        self.call(json!({ "request": request })).await
    }

    fn audit_dir(&self) -> std::path::PathBuf {
        self.h.paths.home().join("audit")
    }

    /// Wait for the audit lines of `file` to satisfy `pick`.
    async fn audit_line(&self, file: &str, pick: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let all = read_lines(&self.audit_dir().join(file));
            if let Some(found) = all.iter().find(|l| pick(l)) {
                return found.clone();
            }
            assert!(
                Instant::now() < deadline,
                "no such line in {file}: {all:#?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn sleeper(&self, name: &str, extra: Value) -> oxidepm_core::AppSpec {
        self.h
            .cmd_with(name, &format!("exec sleep {}", self.h.marker()), extra)
    }
}

fn read_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn code_of(response: &Value) -> &str {
    assert_eq!(response["type"], "error", "expected an error: {response}");
    response["code"].as_str().unwrap_or("")
}

// ---- 27a: the grant ----------------------------------------------------------------------------

#[tokio::test]
async fn host_request_outside_the_grant_is_denied_and_audited() {
    let rig = Rig::new("ui-web", grant(HostLevel::Read, HostScope::All)).await;
    rig.h.start(rig.sleeper("victim", json!({}))).await.unwrap();

    // a read is fine
    let status = rig.ask(json!({ "type": "status" })).await;
    assert_eq!(status["type"], "status", "{status}");
    // a stop is above the grant: a normal error answer, not a protocol error
    let denied = rig.ask(stop("victim")).await;
    assert_eq!(code_of(&denied), "E_POLICY_DENIED", "{denied}");
    assert!(
        denied["message"]
            .as_str()
            .unwrap()
            .contains("adapter:ui-web"),
        "{denied}"
    );
    assert_eq!(
        rig.h.state("victim").await.status,
        oxidepm_core::AppStatus::Running
    );

    // the denial is on record, in the file that rotation of routine reads cannot reach, with the
    // adapter's name
    let line = rig
        .audit_line("audit-security.jsonl", |l| {
            l["decision"] == "deny" && l["op"] == "stop"
        })
        .await;
    assert_eq!(line["actor"]["class"], "adapter", "{line}");
    assert_eq!(line["actor"]["label"], "ui-web", "{line}");
    assert_eq!(line["actor"]["via"], "adapter", "{line}");
    assert_eq!(line["deny"]["code"], "E_POLICY_DENIED", "{line}");
}

#[tokio::test]
async fn forged_fields_in_host_request_are_ignored() {
    let rig = Rig::new("ui-web", grant(HostLevel::Read, HostScope::All)).await;
    let mut spec = rig.sleeper("secretive", json!({}));
    spec.env.insert(
        "DB_DSN".to_string(),
        "postgres://user:planted-secret-dsn@db/app".to_string(),
    );
    rig.h.start(spec).await.unwrap();

    // a full envelope's worth of forged fields, inside the request object and beside it
    let forged = json!({ "actor": "human:root", "reveal": true, "dry_run": false, "protocol": 2, "request_id": "forged-id" });
    let mut request = json!({ "type": "show", "selector": by_name("secretive") });
    for (k, v) in forged.as_object().unwrap() {
        request[k] = v.clone();
    }
    let mut params = json!({ "request": request, "unknown_extra": { "grant": "admin" } });
    for (k, v) in forged.as_object().unwrap() {
        params[k] = v.clone();
    }
    let shown = rig.call(params).await;
    assert_eq!(shown["type"], "show", "{shown}");
    let text = shown.to_string();
    assert!(
        !text.contains("planted-secret-dsn"),
        "reveal was honoured: {text}"
    );
    assert!(text.contains("<redacted>"), "{text}");

    // the context the daemon built: the adapter, on its channel, with the stored grant
    let seen = rig.local.seen.lock().clone();
    let ctx = seen
        .last()
        .expect("the request reached the daemon's entry point");
    assert_eq!(ctx.actor.wire(), "adapter:ui-web");
    assert_eq!(ctx.actor.via.as_str(), "adapter");
    assert!(!ctx.reveal && !ctx.dry_run, "{ctx:?}");
    assert_eq!(ctx.grant, Some(grant(HostLevel::Read, HostScope::All)));
    assert_ne!(ctx.request_id, "forged-id");

    // and the audit line says who really asked
    let line = rig.audit_line("audit.jsonl", |l| l["op"] == "show").await;
    assert_eq!(line["actor"]["class"], "adapter", "{line}");
    assert_eq!(line["actor"]["label"], "ui-web", "{line}");
    assert!(!line.to_string().contains("human:root"), "{line}");

    // an admin-looking claim beside a request that needs more than the grant changes nothing
    let mut stop_request = stop("secretive");
    stop_request["actor"] = json!("system:cron");
    let denied = rig
        .call(json!({ "request": stop_request, "actor": "system:cron" }))
        .await;
    assert_eq!(code_of(&denied), "E_POLICY_DENIED", "{denied}");
}

#[tokio::test]
async fn lifecycle_grant_can_restart_but_not_delete() {
    let rig = Rig::new("ui-web", grant(HostLevel::Lifecycle, HostScope::All)).await;
    rig.h.start(rig.sleeper("web", json!({}))).await.unwrap();
    let before = rig.h.pid_of("web").await;

    let restarted = rig
        .ask(json!({ "type": "restart", "selector": by_name("web") }))
        .await;
    assert_eq!(restarted["type"], "restarted", "{restarted}");
    assert_eq!(restarted["count"], 1, "{restarted}");
    assert!(
        wait_until(10, || std::fs::metadata(format!("/proc/{before}")).is_err()).await,
        "the old process is gone"
    );

    for denied in [
        json!({ "type": "delete", "selector": by_name("web") }),
        json!({ "type": "flush", "selector": by_name("web") }),
        json!({ "type": "save", "force": false }),
        json!({ "type": "start", "spec": serde_json::to_value(rig.sleeper("fresh", json!({}))).unwrap() }),
        json!({ "type": "kill" }),
        json!({ "type": "set_policy", "selector": by_name("web"), "protected": false }),
    ] {
        let answer = rig.ask(denied.clone()).await;
        assert_eq!(code_of(&answer), "E_POLICY_DENIED", "{denied}: {answer}");
    }
    // nothing was deleted, started or flushed
    assert_eq!(rig.h.status().await.len(), 1);
}

#[tokio::test]
async fn admin_grant_can_start_and_delete_but_never_kill_or_reveal() {
    let rig = Rig::new("deployer", grant(HostLevel::Admin, HostScope::All)).await;
    let spec = serde_json::to_value(rig.sleeper("made-by-adapter", json!({}))).unwrap();
    let started = rig.ask(json!({ "type": "start", "spec": spec })).await;
    assert_eq!(started["type"], "started", "{started}");
    let deleted = rig
        .ask(json!({ "type": "delete", "selector": by_name("made-by-adapter") }))
        .await;
    assert_eq!(deleted["type"], "deleted", "{deleted}");

    rig.h.start(rig.sleeper("keep", json!({}))).await.unwrap();
    for human_only in [
        json!({ "type": "kill" }),
        json!({ "type": "set_policy", "selector": by_name("keep"), "protected": true }),
    ] {
        let answer = rig.ask(human_only.clone()).await;
        assert_eq!(
            code_of(&answer),
            "E_POLICY_DENIED",
            "{human_only}: {answer}"
        );
        assert!(
            answer["message"]
                .as_str()
                .unwrap()
                .contains("only a person"),
            "{answer}"
        );
    }
    // reveal is the envelope's flag and an adapter cannot set it
    let shown = rig.call(json!({ "request": { "type": "show", "selector": by_name("keep"), "reveal": true }, "reveal": true })).await;
    assert_eq!(shown["type"], "show");
    assert_eq!(
        rig.h.status().await.len(),
        1,
        "the daemon is up and the app is untouched"
    );
}

#[tokio::test]
async fn opted_in_scope_cannot_touch_other_apps() {
    let rig = Rig::new("watch", grant(HostLevel::Lifecycle, HostScope::OptedIn)).await;
    rig.h
        .start(rig.sleeper("listed", json!({ "adapters": { "watch": {} } })))
        .await
        .unwrap();
    rig.h
        .start(rig.sleeper("unlisted", json!({ "adapters": { "someone-else": {} } })))
        .await
        .unwrap();
    rig.h.start(rig.sleeper("plain", json!({}))).await.unwrap();

    let ok = rig
        .ask(json!({ "type": "restart", "selector": by_name("listed") }))
        .await;
    assert_eq!(ok["type"], "restarted", "{ok}");

    for name in ["unlisted", "plain"] {
        let pid = rig.h.pid_of(name).await;
        let answer = rig.ask(stop(name)).await;
        assert_eq!(code_of(&answer), "E_POLICY_DENIED", "{answer}");
        assert!(
            answer["message"].as_str().unwrap().contains(name),
            "{answer}"
        );
        assert_eq!(rig.h.pid_of(name).await, pid);
        assert_eq!(
            rig.h.state(name).await.status,
            oxidepm_core::AppStatus::Running
        );
    }
    // a tag that reaches both a listed and an unlisted app is refused as a whole
    rig.h
        .start(rig.sleeper(
            "tagged-in",
            json!({ "tags": ["t"], "adapters": { "watch": {} } }),
        ))
        .await
        .unwrap();
    rig.h
        .start(rig.sleeper("tagged-out", json!({ "tags": ["t"] })))
        .await
        .unwrap();
    let answer = rig
        .ask(json!({ "type": "stop", "selector": { "by": "tag", "value": "t" } }))
        .await;
    assert_eq!(code_of(&answer), "E_POLICY_DENIED", "{answer}");
    assert_eq!(
        rig.h.state("tagged-in").await.status,
        oxidepm_core::AppStatus::Running,
        "nothing ran"
    );
}

#[tokio::test]
async fn protected_app_is_read_only_for_a_lifecycle_adapter() {
    let rig = Rig::new("ui-web", grant(HostLevel::Lifecycle, HostScope::All)).await;
    rig.h
        .start(rig.sleeper("vault", json!({ "protected": true })))
        .await
        .unwrap();
    rig.h.start(rig.sleeper("open", json!({}))).await.unwrap();

    for request in [
        stop("vault"),
        json!({ "type": "restart", "selector": by_name("vault") }),
        json!({ "type": "reload", "selector": by_name("vault") }),
        json!({ "type": "stop", "selector": { "by": "all" } }),
    ] {
        let answer = rig.ask(request.clone()).await;
        assert_eq!(code_of(&answer), "E_PROTECTED", "{request}: {answer}");
    }
    // all or nothing: the open app was not stopped by the `all` above
    assert_eq!(
        rig.h.state("open").await.status,
        oxidepm_core::AppStatus::Running
    );
    // reads are fine
    assert_eq!(
        rig.ask(json!({ "type": "show", "selector": by_name("vault") }))
            .await["type"],
        "show"
    );
    let line = rig
        .audit_line("audit-security.jsonl", |l| {
            l["deny"]["code"] == "E_PROTECTED"
        })
        .await;
    assert_eq!(line["actor"]["label"], "ui-web");
}

#[tokio::test]
async fn admin_adapter_cannot_start_over_a_protected_app_or_outside_its_scope() {
    let rig = Rig::new("deployer", grant(HostLevel::Admin, HostScope::OptedIn)).await;
    rig.h
        .start(rig.sleeper(
            "vault",
            json!({ "protected": true, "adapters": { "deployer": {} } }),
        ))
        .await
        .unwrap();
    // a start of a spec whose name is a protected app's: the app is read-only
    let spec =
        serde_json::to_value(rig.sleeper("vault", json!({ "adapters": { "deployer": {} } })))
            .unwrap();
    let answer = rig.ask(json!({ "type": "start", "spec": spec })).await;
    assert_eq!(code_of(&answer), "E_PROTECTED", "{answer}");
    // a new app that does not list the adapter is outside the scope; one that does is not
    let outside = serde_json::to_value(rig.sleeper("outside", json!({}))).unwrap();
    let answer = rig.ask(json!({ "type": "start", "spec": outside })).await;
    assert_eq!(code_of(&answer), "E_POLICY_DENIED", "{answer}");
    assert!(
        answer["message"].as_str().unwrap().contains("outside"),
        "{answer}"
    );
    let inside =
        serde_json::to_value(rig.sleeper("inside", json!({ "adapters": { "deployer": {} } })))
            .unwrap();
    let answer = rig.ask(json!({ "type": "start", "spec": inside })).await;
    assert_eq!(answer["type"], "started", "{answer}");
    assert!(rig
        .h
        .status()
        .await
        .iter()
        .all(|a| a.spec.name != "outside"));
    // `save` and `resurrect` act on every app, which an opted-in scope does not cover
    for global in [
        json!({ "type": "save", "force": false }),
        json!({ "type": "resurrect" }),
    ] {
        assert_eq!(code_of(&rig.ask(global).await), "E_POLICY_DENIED");
    }
}

#[tokio::test]
async fn global_requests_of_an_admin_adapter_stop_at_a_protected_app() {
    let rig = Rig::new("deployer", grant(HostLevel::Admin, HostScope::All)).await;
    rig.h.start(rig.sleeper("plain", json!({}))).await.unwrap();
    let saved = rig.ask(json!({ "type": "save", "force": false })).await;
    assert_eq!(saved["type"], "saved", "{saved}");
    rig.h
        .start(rig.sleeper("vault", json!({ "protected": true })))
        .await
        .unwrap();
    let refused = rig.ask(json!({ "type": "save", "force": false })).await;
    assert_eq!(code_of(&refused), "E_PROTECTED", "{refused}");
}

#[tokio::test]
async fn admin_adapter_resurrect_refuses_protected_saved_apps() {
    let rig = Rig::new("deployer", grant(HostLevel::Admin, HostScope::All)).await;
    let specs = [
        rig.sleeper("plain", json!({})),
        rig.sleeper("vault", json!({ "protected": true })),
    ];
    let path = rig.h.paths.saved();
    std::fs::write(&path, saved_json(&specs)).unwrap();

    let request = json!({ "type": "resurrect" });
    let refused = rig.ask(request.clone()).await;
    assert_eq!(code_of(&refused), "E_PROTECTED", "{refused}");
    assert!(refused["message"].as_str().unwrap().contains("vault"));

    let plan = rig
        .call(json!({ "request": request, "dry_run": true }))
        .await;
    assert_eq!(plan["type"], "dry_run", "{plan}");
    assert_eq!(plan["plan"]["decision"]["code"], "E_PROTECTED", "{plan}");

    let mut unreadable: Value = serde_json::from_str(&saved_json(&specs)).unwrap();
    unreadable["apps"][1]["protected"] = json!("unknown");
    std::fs::write(&path, unreadable.to_string()).unwrap();
    let refused = rig.ask(json!({ "type": "resurrect" })).await;
    assert_eq!(code_of(&refused), "E_POLICY_DENIED", "{refused}");

    assert!(rig.h.status().await.is_empty());
    let db = oxidepm_db::Database::new(&rig.h.paths.db()).await.unwrap();
    assert!(db.apps().records().await.unwrap().is_empty());

    let line = rig
        .audit_line("audit-security.jsonl", |line| {
            line["op"] == "resurrect" && line["decision"] == "deny"
        })
        .await;
    assert_eq!(line["deny"]["code"], "E_PROTECTED", "{line}");
}

#[tokio::test]
async fn admin_adapter_resurrect_uses_checked_saved_apps() {
    let rig = Rig::new("deployer", grant(HostLevel::Admin, HostScope::All)).await;
    let path = rig.h.paths.saved();
    std::fs::write(&path, saved_json(&[rig.sleeper("checked", json!({}))])).unwrap();
    let pause = rig.local.handler.pause_after_authorize();
    let answer = async { rig.ask(json!({ "type": "resurrect" })).await };
    let replace_saved = async {
        tokio::time::timeout(WAIT_CAP, pause.reached.notified())
            .await
            .expect("resurrect reached the gate");
        let later = rig.sleeper("later", json!({}));
        std::fs::write(&path, saved_json(&[later])).unwrap();
        let db = oxidepm_db::Database::new(&rig.h.paths.db()).await.unwrap();
        db.apps()
            .insert_with(
                &rig.sleeper("late-registered", json!({})),
                None,
                oxidepm_core::Desired::Running,
            )
            .await
            .unwrap();
        assert_eq!(rig.h.sup.load_registry().await.unwrap().loaded, 1);
        pause.release.notify_one();
    };
    let (answer, ()) = tokio::join!(answer, replace_saved);
    assert_eq!(answer["type"], "resurrected", "{answer}");
    assert_eq!(answer["imported"], 1, "{answer}");
    assert_eq!(answer["started"], 1, "{answer}");
    let apps = rig.h.status().await;
    assert_eq!(apps.len(), 2, "{apps:?}");
    assert_eq!(
        rig.h.state("checked").await.status,
        oxidepm_core::AppStatus::Running
    );
    assert_eq!(
        rig.h.state("late-registered").await.status,
        oxidepm_core::AppStatus::Stopped
    );
}

// ---- 27a: rates, in-flight, notes, params --------------------------------------------------------

#[tokio::test]
async fn rate_limit_returns_e_rate_limited() {
    // the production limits with a fixed clock: a burst of 100 reads, 10 caution
    // operations
    let rig = Rig::with_rates(
        "ui-web",
        grant(HostLevel::Lifecycle, HostScope::All),
        Some(Rates::default()),
    )
    .await;
    rig.h.start(rig.sleeper("web", json!({}))).await.unwrap();
    let mut limited_at = None;
    for n in 1..=130 {
        let answer = rig
            .ask(json!({ "type": "show", "selector": by_name("web") }))
            .await;
        if answer["type"] == "error" {
            assert_eq!(answer["code"], "E_RATE_LIMITED", "{answer}");
            limited_at = Some(n);
            break;
        }
    }
    let n = limited_at.expect("the burst ended");
    assert!(
        (95..=115).contains(&n),
        "the first refusal came at request {n}: the burst is 100"
    );

    // caution operations have their own, slower allowance (a fresh adapter, so the general
    // bucket is full): the 11th inside the burst is refused, and it is a stop of an app that is
    // not there, so the allowance goes whether or not anything changes
    let rig = Rig::with_rates(
        "watch",
        grant(HostLevel::Lifecycle, HostScope::All),
        Some(Rates::default()),
    )
    .await;
    rig.h.start(rig.sleeper("web", json!({}))).await.unwrap();
    let mut answers = Vec::new();
    for _ in 0..12 {
        answers.push(rig.ask(stop("web")).await);
    }
    let refused: Vec<&Value> = answers
        .iter()
        .filter(|a| a["type"] == "error" && a["code"] == "E_RATE_LIMITED")
        .collect();
    assert!(!refused.is_empty() && refused.len() <= 3, "{answers:#?}");
    assert!(
        answers[..10].iter().all(|a| a["code"] != "E_RATE_LIMITED"),
        "the first ten were inside the burst"
    );
    // the refusals are one line per actor and minute, not one each; it is written when the minute
    // is over or the log is closed
    rig.audit.close(Duration::from_secs(5)).await;
    let counted: Vec<Value> = read_lines(&rig.audit_dir().join("audit.jsonl"))
        .into_iter()
        .filter(|l| l["phase"] == "rate_limited")
        .collect();
    assert_eq!(counted.len(), 1, "{counted:?}");
    assert_eq!(counted[0]["actor"]["label"], "watch", "{counted:?}");
    assert_eq!(counted[0]["count"], refused.len() as u64, "{counted:?}");
}

/// A dispatcher that answers every request with `E_RATE_LIMITED`, as the audit log's limiter does
/// for an adapter that is over its rate.
struct AlwaysLimited;

#[async_trait]
impl Dispatch for AlwaysLimited {
    async fn dispatch(&self, _ctx: &ReqCtx, _request: Request) -> Response {
        Response::error(ErrorCode::RateLimited, "asked too often")
    }
}

#[tokio::test]
async fn a_rate_limit_strike_belongs_to_the_connection_that_requested_work() {
    let link = HostLink::new();
    link.bind(Arc::new(AlwaysLimited));
    let api = HostApi::new("shared", grant(HostLevel::Read, HostScope::All), link, None);
    let mut first = Fixture::spawn(
        json!([{"on": "initialize", "do": ["reply", {"host_request": {"request": {"type": "status"}, "repeat": 20}}]}]),
    );
    let first_conn = first.connect_with(ConnConfig::protocol1(), api.clone());
    let mut second = Fixture::spawn(json!([]));
    let second_conn = second.connect_with(ConnConfig::protocol1(), api);
    let params = common::fixture::initialize_params("shared", "1.0.0", "tool");
    first_conn
        .call("initialize", &params, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        wait_until(10, || first
            .report()
            .iter()
            .filter(|r| r["what"] == "host_answer")
            .count()
            == 20)
        .await
    );
    assert_eq!(first_conn.counters().strikes, 1);
    assert_eq!(second_conn.counters().strikes, 0);
}

#[tokio::test]
async fn an_adapter_that_keeps_asking_over_its_rate_earns_strikes_and_is_cut_off() {
    let link = HostLink::new();
    link.bind(Arc::new(AlwaysLimited));
    let api = HostApi::new("noisy", grant(HostLevel::Read, HostScope::All), link, None);
    // 100 requests, all refused for their rate: 20 inside ten seconds is a strike, three strikes
    // end the connection
    let mut fx = Fixture::spawn(
        json!([{"on": "initialize", "do": ["reply", {"host_request": {"request": {"type": "status"}, "repeat": 100}}]}]),
    );
    // room for all of them in flight at once, so that none is answered "busy" instead
    let mut cfg = ConnConfig::protocol1();
    cfg.limits.host_in_flight = 128;
    let conn = fx.connect_with(cfg, api);
    conn.call(
        "initialize",
        &common::fixture::initialize_params("noisy", "1.0.0", "service"),
        Duration::from_secs(10),
    )
    .await
    .unwrap();

    let closed = tokio::time::timeout(Duration::from_secs(15), conn.closed())
        .await
        .expect("the connection was cut off");
    assert_eq!(closed, CloseReason::Strikes);
    assert!(conn.counters().strikes >= 3, "{:?}", conn.counters());
}

/// A dispatcher that holds every request until released.
struct Held {
    started: AtomicUsize,
    release: tokio::sync::Notify,
}

#[async_trait]
impl Dispatch for Held {
    async fn dispatch(&self, _ctx: &ReqCtx, _request: Request) -> Response {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.release.notified().await;
        Response::Pong
    }
}

#[tokio::test]
async fn host_requests_in_flight_are_capped_at_the_grants_number() {
    let held = Arc::new(Held {
        started: AtomicUsize::new(0),
        release: tokio::sync::Notify::new(),
    });
    let link = HostLink::new();
    link.bind(Arc::clone(&held) as Arc<dyn Dispatch>);
    let api = HostApi::new("slow", grant(HostLevel::Read, HostScope::All), link, None);
    let mut fx = Fixture::spawn(
        json!([{"on": "initialize", "do": ["reply", {"host_request": {"request": {"type": "status"}, "repeat": 40}}]}]),
    );
    let conn = fx.connect_with(ConnConfig::protocol1(), api);
    conn.call(
        "initialize",
        &common::fixture::initialize_params("slow", "1.0.0", "service"),
        Duration::from_secs(10),
    )
    .await
    .unwrap();

    // 32 are being served; the other 8 were answered at once with E_ADAPTER_BUSY
    assert!(
        wait_until(10, || held.started.load(Ordering::SeqCst) == 32).await,
        "{}",
        held.started.load(Ordering::SeqCst)
    );
    let busy = || {
        fx.report()
            .iter()
            .filter(|r| {
                r["what"] == "host_answer"
                    && r["frame"]["error"]["data"]["code"] == "E_ADAPTER_BUSY"
            })
            .count()
    };
    assert!(
        wait_until(10, || busy() == 8).await,
        "{} busy answers",
        busy()
    );
    assert_eq!(conn.counters().strikes, 0, "being busy is not a violation");
    for _ in 0..32 {
        held.release.notify_one();
    }
    assert!(
        wait_until(10, || fx
            .report()
            .iter()
            .filter(|r| r["what"] == "host_answer")
            .count()
            == 40)
        .await
    );
}

#[test]
fn the_grant_may_raise_the_host_request_cap_up_to_the_ceiling() {
    use oxidepmd::adapters::host::host_in_flight_for;
    let limits = oxidepm_adapter_proto::Limits::PROTOCOL_1;
    assert_eq!(host_in_flight_for(None, &limits), 32);
    assert_eq!(host_in_flight_for(Some(64), &limits), 64);
    assert_eq!(host_in_flight_for(Some(128), &limits), 128);
    assert_eq!(host_in_flight_for(Some(5000), &limits), 128, "the ceiling");
    assert_eq!(host_in_flight_for(Some(0), &limits), 1);
}

#[tokio::test]
async fn bad_params_are_an_error_answer_not_a_strike() {
    use oxidepmd::adapters::conn::Inbound;
    let rig = Rig::new("ui-web", grant(HostLevel::Read, HostScope::All)).await;
    for params in [
        json!({}),
        json!({ "request": "stop" }),
        json!({ "request": { "type": "no_such_request" } }),
        json!({ "request": { "type": "status" }, "end_user": 5 }),
    ] {
        let Value::Object(map) = params.clone() else {
            unreachable!()
        };
        let err = rig
            .api
            .request("host.request", map)
            .await
            .expect_err(&format!("{params} is not a request"));
        assert_eq!(err.code, rpc::INVALID_PARAMS, "{params}: {err:?}");
    }
    let err = rig
        .api
        .request("host.audit_note", Params::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, rpc::INVALID_PARAMS);
    let err = rig
        .api
        .request("host.nothing", Params::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, rpc::METHOD_NOT_FOUND);
    assert!(
        rig.local.seen.lock().is_empty(),
        "nothing reached the daemon"
    );
}

#[tokio::test]
async fn audit_note_is_structured_and_rate_limited() {
    use oxidepmd::adapters::conn::Inbound;
    let rig = Rig::with_rates(
        "notifier",
        grant(HostLevel::Read, HostScope::All),
        Some(Rates {
            per_sec: 1.0,
            burst: 3.0,
            ..Rates::default()
        }),
    )
    .await;
    let mut text = Params::new();
    text.insert("text".into(), json!("example private option"));
    rig.api
        .request("host.audit_note", text.clone())
        .await
        .expect("a note");
    let line = rig
        .audit_line("audit.jsonl", |l| l["phase"] == "note")
        .await;
    assert_eq!(line["adapter"]["name"], "notifier", "{line}");
    assert_eq!(line["text"], "Adapter reported a note");
    assert!(!line.to_string().contains("example private option"));
    // over the rate: an error answer with the stable name
    let mut last = Ok(Params::new());
    for _ in 0..10 {
        last = rig.api.request("host.audit_note", text.clone()).await;
        if last.is_err() {
            break;
        }
    }
    let err = last.expect_err("the notes ran into the rate limit");
    assert_eq!(
        (err.code, err.name()),
        (rpc::RATE_LIMITED, Some("E_RATE_LIMITED"))
    );
}

#[tokio::test]
async fn end_user_label_is_recorded_but_never_used() {
    let rig = Rig::new("ui-web", grant(HostLevel::Read, HostScope::All)).await;
    rig.h.start(rig.sleeper("web", json!({}))).await.unwrap();
    // the label says what it likes; the decision is the grant's
    for (n, label) in ["alice", "root", "human:cli", "admin\u{1b}[31m\nsecond line"]
        .iter()
        .enumerate()
    {
        let answer = rig
            .call(json!({ "request": stop("web"), "end_user": label }))
            .await;
        assert_eq!(code_of(&answer), "E_POLICY_DENIED", "{label}: {answer}");
        let want: String = label.chars().filter(|c| !c.is_control()).collect();
        let line = rig
            .audit_line("audit-security.jsonl", |l| l["end_user"] == want.as_str())
            .await;
        assert_eq!(line["actor"]["label"], "ui-web", "line {n}: {line}");
    }
    // at most 64 characters
    let _ = rig
        .call(json!({ "request": stop("web"), "end_user": "x".repeat(500) }))
        .await;
    let line = rig
        .audit_line("audit-security.jsonl", |l| {
            l["end_user"].as_str().is_some_and(|e| e.starts_with("xxx"))
        })
        .await;
    assert_eq!(line["end_user"].as_str().unwrap().len(), 64);
    // and an allowed request is allowed whoever the label says is behind it
    let ok = rig
        .call(json!({ "request": { "type": "status" }, "end_user": "nobody" }))
        .await;
    assert_eq!(ok["type"], "status");
    assert_eq!(
        rig.h.state("web").await.status,
        oxidepm_core::AppStatus::Running
    );
}

// ---- 27a: logs --------------------------------------------------------------------------------

#[tokio::test]
async fn logs_for_an_adapter_are_scrubbed() {
    let rig = Rig::new("log-insight", grant(HostLevel::Read, HostScope::All)).await;
    // an app that prints its own DSN, a short value and a value it was not given
    let mut spec = rig.h.cmd("leaky", "echo \"connecting to postgres://user:planted-secret-dsn@db/app with key planted-api-key\"; echo \"mode=on port=80\"; exec sleep 300.5");
    spec.env.insert(
        "DB_DSN".to_string(),
        "postgres://user:planted-secret-dsn@db/app".to_string(),
    );
    spec.env
        .insert("API_KEY".to_string(), "planted-api-key".to_string());
    spec.env.insert("MODE".to_string(), "on".to_string());
    spec.env
        .insert("EXACTLY6".to_string(), "sixsix".to_string());
    rig.h.start(spec).await.unwrap();
    rig.h.wait_log("leaky", "out", "mode=on").await;

    let logs = rig.ask(json!({ "type": "logs", "selector": by_name("leaky"), "lines": 50, "follow": false, "stdout": true, "stderr": false })).await;
    assert_eq!(logs["type"], "log_lines", "{logs}");
    let text = logs["lines"].to_string();
    assert!(
        !text.contains("planted-secret-dsn") && !text.contains("planted-api-key"),
        "{text}"
    );
    assert!(
        text.contains("<redacted>") && text.contains("connecting to"),
        "{text}"
    );
    assert!(
        text.contains("mode=on"),
        "a value under 6 characters is not hunted for: {text}"
    );

    // a person gets the file as it is
    let human = rig
        .h
        .sup
        .logs(
            &oxidepm_core::Selector::ByName("leaky".into()),
            50,
            true,
            false,
        )
        .await
        .unwrap();
    assert!(human.join("\n").contains("planted-secret-dsn"));
}

#[tokio::test]
async fn logs_for_an_adapter_are_bounded() {
    let rig = Rig::new("log-insight", grant(HostLevel::Read, HostScope::All)).await;
    // 12 000 short lines, then 3000 lines of 600 bytes
    let spec = rig.h.cmd(
        "chatty",
        "i=0; while [ $i -lt 12000 ]; do echo line-$i; i=$((i+1)); done; echo short-done; exec sleep 300.6",
    );
    rig.h.start(spec).await.unwrap();
    rig.h.wait_log("chatty", "out", "short-done").await;
    let answer = rig.ask(json!({ "type": "logs", "selector": by_name("chatty"), "lines": 1_000_000, "follow": false, "stdout": true, "stderr": false })).await;
    let lines = answer["lines"]
        .as_array()
        .unwrap_or_else(|| panic!("{answer}"));
    assert!(lines.len() <= 10_000, "{} lines", lines.len());
    assert!(
        lines
            .last()
            .unwrap()
            .as_str()
            .unwrap()
            .contains("short-done"),
        "the newest lines are the ones kept"
    );

    let fat = rig.h.cmd(
        "fat",
        "pad=$(head -c 600 /dev/zero | tr '\\0' 'p'); i=0; while [ $i -lt 3000 ]; do echo \"$i $pad\"; i=$((i+1)); done; echo fat-done; exec sleep 300.7",
    );
    rig.h.start(fat).await.unwrap();
    rig.h.wait_log("fat", "out", "fat-done").await;
    let answer = rig.ask(json!({ "type": "logs", "selector": by_name("fat"), "lines": 5000, "follow": false, "stdout": true, "stderr": false })).await;
    let lines = answer["lines"].as_array().unwrap();
    let bytes: usize = lines.iter().map(|l| l.as_str().unwrap().len() + 1).sum();
    assert!(
        bytes <= 1024 * 1024 && !lines.is_empty(),
        "{bytes} bytes in {} lines",
        lines.len()
    );
    assert!(lines.last().unwrap().as_str().unwrap().contains("fat-done"));
}

#[tokio::test]
async fn an_oversize_log_reply_is_an_error_and_never_a_buffer() {
    // through a connection: a reply over the frame limit is replaced by E_REQUEST_TOO_LARGE
    let rig = Rig::new("log-insight", grant(HostLevel::Read, HostScope::All)).await;
    let fat = rig.h.cmd(
        "fat",
        "pad=$(head -c 600 /dev/zero | tr '\\0' 'p'); i=0; while [ $i -lt 1000 ]; do echo \"$i $pad\"; i=$((i+1)); done; echo fat-done; exec sleep 300.8",
    );
    rig.h.start(fat).await.unwrap();
    rig.h.wait_log("fat", "out", "fat-done").await;
    let link = HostLink::new();
    link.bind(Arc::clone(&rig.local) as Arc<dyn Dispatch>);
    let api = HostApi::new(
        "log-insight",
        grant(HostLevel::Read, HostScope::All),
        link,
        None,
    );
    let request = json!({ "type": "logs", "selector": by_name("fat"), "lines": 1000, "follow": false, "stdout": true, "stderr": false });
    let mut fx = Fixture::spawn(
        json!([{"on": "initialize", "do": ["reply", {"host_request": {"request": request}}]}]),
    );
    let conn = fx.connect_with(ConnConfig::protocol1(), api);
    conn.call(
        "initialize",
        &common::fixture::initialize_params("log-insight", "1.0.0", "service"),
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    let answer = fx.wait_report("host_answer").await.expect("an answer");
    assert_eq!(
        answer["frame"]["error"]["data"]["code"], "E_REQUEST_TOO_LARGE",
        "{answer}"
    );
    assert!(!conn.is_closed());
}

#[tokio::test]
async fn log_reads_of_one_reader_run_one_at_a_time() {
    // a reader that asks for the logs of many apps at once is served, one read after the other,
    // and every answer is whole
    let rig = Rig::new("log-insight", grant(HostLevel::Read, HostScope::All)).await;
    rig.h
        .start(
            rig.h
                .cmd("talker", "echo hello-from-talker; exec sleep 300.9"),
        )
        .await
        .unwrap();
    rig.h.wait_log("talker", "out", "hello-from-talker").await;
    let request = json!({ "type": "logs", "selector": by_name("talker"), "lines": 10, "follow": false, "stdout": true, "stderr": false });
    let api = Arc::clone(&rig.api);
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let (api, request) = (Arc::clone(&api), request.clone());
        tasks.push(tokio::spawn(async move {
            use oxidepmd::adapters::conn::Inbound;
            let Value::Object(map) = json!({ "request": request }) else {
                unreachable!()
            };
            api.request("host.request", map)
                .await
                .map(|r| r["response"].clone())
        }));
    }
    let mut ok = 0;
    for task in tasks {
        let answer = task.await.unwrap().expect("an answer");
        // over the rate or served: never a half answer
        if answer["type"] == "log_lines" {
            assert!(answer["lines"].to_string().contains("hello-from-talker"));
            ok += 1;
        } else {
            assert_eq!(answer["code"], "E_RATE_LIMITED", "{answer}");
        }
    }
    assert!(ok >= 10, "{ok} reads served");
}

// ---- 27b: attribution at accept ------------------------------------------------------------------

/// A line for the main socket: `request` in an envelope that claims to be a person.
fn claiming_human(request: Value) -> String {
    json!({ "protocol": 2, "request_id": "dial", "actor": "human:cli", "request": request })
        .to_string()
}

struct Dialing {
    _tmp: TempDir,
    daemon: DaemonProc,
    fx: common::fixture::Installed,
}

/// A daemon with an installed fixture service that listens to lifecycle events; `action` is what
/// the service does on the first event (the start of the app `victim`, which the caller makes).
async fn dialing(
    action: impl FnOnce(&str) -> Value,
    grant_level: &str,
    env: &[(&str, &str)],
) -> Dialing {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let socket = home.join("daemon.sock").display().to_string();
    let fx = install(
        &home,
        InstallSpec {
            perms: Some(json!({
                "events": ["lifecycle"],
                "host": { "level": grant_level, "scope": "all" },
                // the service may reach the daemon's sockets (a write path is how Landlock lets a
                // process connect to a pathname socket where it can restrict that)
                "fs": { "read": ["$STATE", "$IO"], "write": ["$STATE", "$IO", home.display().to_string()] },
                "spawn": { "mode": "any" }
            })),
            ..InstallSpec::service("dialer", json!([{ "on": "event", "times": 1, "do": action(&socket) }]))
        },
    )
    .await;
    let mut vars: Vec<(String, String)> = fx.env();
    vars.extend(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = DaemonProc::spawn_with_env(&home, &vars);
    assert!(
        wait_adapter_state(&daemon, "dialer", "ready", 20).await,
        "{:?}",
        fx.report()
    );
    Dialing {
        _tmp: tmp,
        daemon,
        fx,
    }
}

/// An app that sleeps, as the wire carries it.
fn victim_spec(cwd: &Path, secs: &str) -> Value {
    let mut spec = oxidepm_core::AppSpec::new(
        "victim".to_string(),
        oxidepm_core::Runtime::CMD,
        "sleep".to_string(),
        cwd.to_path_buf(),
    );
    spec.args = vec![secs.to_string()];
    serde_json::to_value(spec).unwrap()
}

fn start_victim(d: &Dialing) -> serde_json::Value {
    let spec = victim_spec(d._tmp.path(), "301.5");
    let started = d.daemon.rpc(json!({ "type": "start", "spec": spec }));
    assert_eq!(started["type"], "started", "{started}");
    started
}

fn victim_status(d: &Dialing) -> String {
    let status = d.daemon.rpc(json!({ "type": "status" }));
    status["apps"]
        .as_array()
        .and_then(|apps| apps.iter().find(|a| a["spec"]["name"] == "victim"))
        .and_then(|a| a["state"]["status"].as_str())
        .unwrap_or("missing")
        .to_string()
}

async fn security_line(d: &Dialing, pick: impl Fn(&Value) -> bool) -> Value {
    let file = d.daemon.home().join("audit").join("audit-security.jsonl");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let all = read_lines(&file);
        if let Some(found) = all.iter().find(|l| pick(l)) {
            return found.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no such line: {all:#?} / report {:?}",
            d.fx.report()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn adapter_dialing_the_main_socket_is_relabelled() {
    let d = dialing(
        |socket| json!([{ "connect_socket": { "path": socket, "send": claiming_human(stop("victim")) } }]),
        "read",
        &[],
    )
    .await;
    start_victim(&d);
    let exchange =
        d.fx.wait_report("socket")
            .await
            .expect("the adapter dialed");
    assert_eq!(exchange["ok"], true, "{exchange}");
    // the claim of a person did not make the adapter one: its grant (read) is what applies
    let answer: Value = serde_json::from_str(exchange["answer"].as_str().unwrap()).unwrap();
    assert_eq!(answer["response"]["code"], "E_POLICY_DENIED", "{answer}");
    assert!(
        answer["response"]["message"]
            .as_str()
            .unwrap()
            .contains("adapter:dialer"),
        "{answer}"
    );
    let line = security_line(&d, |l| l["decision"] == "deny" && l["op"] == "stop").await;
    assert_eq!(
        (
            line["actor"]["class"].as_str(),
            line["actor"]["label"].as_str()
        ),
        (Some("adapter"), Some("dialer")),
        "{line}"
    );
    assert_eq!(victim_status(&d), "running");
}

#[tokio::test]
async fn a_child_of_the_adapter_dialing_the_main_socket_is_relabelled() {
    let d = dialing(
        |socket| json!([{ "dial_from_child": { "path": socket, "send": claiming_human(stop("victim")) } }]),
        "read",
        &[],
    )
    .await;
    start_victim(&d);
    let exchange = d.fx.wait_report("socket").await.expect("the child dialed");
    assert_eq!(exchange["from"], "child", "{exchange}");
    assert_eq!(exchange["ok"], true, "{exchange}");
    let answer: Value = serde_json::from_str(exchange["answer"].as_str().unwrap()).unwrap();
    assert_eq!(answer["response"]["code"], "E_POLICY_DENIED", "{answer}");
    let line = security_line(&d, |l| l["decision"] == "deny" && l["op"] == "stop").await;
    assert_eq!(
        (
            line["actor"]["class"].as_str(),
            line["actor"]["label"].as_str()
        ),
        (Some("adapter"), Some("dialer")),
        "{line}"
    );
    assert_eq!(victim_status(&d), "running");
}

#[tokio::test]
async fn a_relabelled_adapter_gets_its_grant_and_no_more() {
    // the same dial, with a lifecycle grant: the stop is its right, and it happens
    let d = dialing(
        |socket| json!([{ "dial_from_child": { "path": socket, "send": claiming_human(stop("victim")) } }]),
        "lifecycle",
        &[],
    )
    .await;
    start_victim(&d);
    let exchange = d.fx.wait_report("socket").await.expect("the child dialed");
    let answer: Value = serde_json::from_str(exchange["answer"].as_str().unwrap()).unwrap();
    assert_eq!(answer["response"]["type"], "stopped", "{answer}");
    assert_eq!(victim_status(&d), "stopped");
    // not the ai class: the audit line is the adapter's
    let line = security_line(&d, |l| l["op"] == "stop" && l["decision"] == "allow").await;
    assert_eq!(
        (
            line["actor"]["class"].as_str(),
            line["actor"]["label"].as_str()
        ),
        (Some("adapter"), Some("dialer")),
        "{line}"
    );
}

#[tokio::test]
async fn connect_write_exit_from_adapter_child_is_not_human() {
    // the daemon looks 700 ms after it accepts (a test build only), when the child has long
    // gone: the peer cannot be read, and an unreadable peer is not a person
    let d = dialing(
        |socket| json!([{ "connect_write_exit": { "path": socket, "send": claiming_human(stop("victim")) } }]),
        "lifecycle",
        &[("OXIDEPM_TEST_PEER_WALK_DELAY_MS", "700")],
    )
    .await;
    start_victim(&d);
    d.fx.wait_report("child")
        .await
        .expect("the child ran and left");
    let line = security_line(&d, |l| l["decision"] == "deny" && l["op"] == "stop").await;
    assert_eq!(line["actor"]["class"], "ai", "{line}");
    assert_eq!(line["actor"]["label"], "unresolved", "{line}");
    assert_eq!(line["actor"]["via"], "unattributed", "{line}");
    assert_eq!(
        victim_status(&d),
        "running",
        "the stop of an unresolved peer did not run"
    );
}

#[tokio::test]
async fn a_cli_that_is_not_the_adapters_is_unaffected_while_an_adapter_runs() {
    let d = dialing(|_| json!([]), "lifecycle", &[]).await;
    start_victim(&d);
    // this process is not in the adapter's tree: a person, as before
    let stopped = d
        .daemon
        .rpc(json!({ "type": "stop", "selector": by_name("victim") }));
    assert_eq!(stopped["type"], "stopped", "{stopped}");
    // a claim of an AI is still honoured as a claim (stricter)
    let ai = d.daemon.rpc_on(
        &d.daemon.socket(),
        "r",
        Some("ai:example-agent"),
        json!({ "type": "restart", "selector": by_name("victim") }),
    );
    assert_eq!(ai["response"]["code"], "E_POLICY_DENIED", "{ai}");
}

// ---- 27b: the host's look at a peer ---------------------------------------------------------------

struct HostRig {
    _tmp: TempDir,
    host: Arc<AdapterHost>,
    fx: common::fixture::Installed,
}

async fn host_with(service: bool, script: Value) -> HostRig {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let spec = if service {
        InstallSpec::service("svc", script)
    } else {
        InstallSpec::tool("svc", script)
    };
    let fx = install(
        &home,
        InstallSpec {
            perms: Some(json!({ "host": { "level": "lifecycle", "scope": "opted-in" } })),
            ..spec
        },
    )
    .await;
    let env: std::collections::HashMap<String, String> = fx.env().into_iter().collect();
    let cfg = HostConfig {
        env_source: Arc::new(move |key: &str| env.get(key).cloned()),
        launcher: Some(launcher_bin()),
        boot_stagger: Duration::from_millis(1),
        ..HostConfig::production()
    };
    let paths = Arc::new(Paths::new(home));
    let host = AdapterHost::load(&paths, None, cfg).await;
    HostRig {
        _tmp: tmp,
        host,
        fx,
    }
}

#[tokio::test]
async fn no_proc_walk_when_no_adapter_runs() {
    // a daemon with no adapter at all
    let tmp = TempDir::new().unwrap();
    let paths = Arc::new(Paths::new(tmp.path().join("home")));
    let host = AdapterHost::load(&paths, None, HostConfig::production()).await;
    for pid in [
        Some(std::process::id() as i32),
        Some(1),
        None,
        Some(i32::MAX),
    ] {
        assert_eq!(host.label_peer(pid).await, None);
    }
    assert_eq!(host.peer_proc_reads(), 0, "not one /proc file was opened");

    // an adapter that is installed but not running (a tool waits for its first call)
    let rig = host_with(false, json!([])).await;
    assert_eq!(rig.host.len(), 1);
    for pid in [Some(std::process::id() as i32), None] {
        assert_eq!(rig.host.label_peer(pid).await, None);
    }
    assert_eq!(
        rig.host.peer_proc_reads(),
        0,
        "no adapter process, no /proc"
    );
    rig.host.shutdown(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn unresolvable_peer_is_ai_while_an_adapter_runs() {
    let rig = host_with(
        true,
        json!([{"on": "initialize", "do": ["reply", {"spawn_grandchild": {"cmd": "sleep 120"}}]}]),
    )
    .await;
    rig.host.start_services();
    let entry = rig.host.entry("svc").unwrap();
    assert!(
        wait_state(&entry, 20, |s| *s == EntryState::Ready).await,
        "{:?}",
        entry.view()
    );
    let adapter_pid = entry.view().pid.expect("a pid") as i32;
    let grandchild = rig
        .fx
        .wait_report("grandchild")
        .await
        .expect("a grandchild");
    let grandchild_pid = grandchild["child_pid"].as_u64().expect("its pid") as i32;
    let expected = PeerLabel::Adapter {
        name: "svc".to_string(),
        grant: grant(HostLevel::Lifecycle, HostScope::OptedIn),
    };

    // the adapter and its descendant are the adapter, under the grant that was stored
    assert_eq!(
        rig.host.label_peer(Some(adapter_pid)).await,
        Some(expected.clone())
    );
    assert_eq!(
        rig.host.label_peer(Some(grandchild_pid)).await,
        Some(expected)
    );
    // this process is a live process outside the tree: no label, the claim decides
    assert_eq!(
        rig.host.label_peer(Some(std::process::id() as i32)).await,
        None
    );
    // a pid that is not there, no pid at all, and a pid that cannot be one: not a person
    assert_eq!(
        rig.host.label_peer(Some(i32::MAX - 7)).await,
        Some(PeerLabel::Unresolved)
    );
    assert_eq!(rig.host.label_peer(None).await, Some(PeerLabel::Unresolved));
    assert_eq!(
        rig.host.label_peer(Some(-4)).await,
        Some(PeerLabel::Unresolved)
    );
    assert!(rig.host.peer_proc_reads() > 0);

    unsafe {
        libc::kill(grandchild_pid, libc::SIGKILL);
    }
    rig.host.shutdown(Duration::from_secs(5)).await;
}

// ---- 27a/27b through the real daemon ---------------------------------------------------------------

#[tokio::test]
async fn host_requests_wait_for_a_valid_initialize_reply() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let request = json!({"host_request": {"request": {"type": "status"}}});
    let fx = install(
        &home,
        InstallSpec::service(
            "asker",
            json!([
                {"on": "initialize", "do": [request.clone(), "reply", request]}
            ]),
        ),
    )
    .await;
    let vars = fx.env();
    let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut daemon = DaemonProc::spawn_with_env(&home, &vars);
    assert!(wait_adapter_state(&daemon, "asker", "ready", 20).await);
    assert!(
        wait_until(10, || fx
            .report()
            .iter()
            .filter(|r| r["what"] == "host_answer")
            .count()
            >= 2)
        .await
    );
    let mut answers: Vec<_> = fx
        .report()
        .into_iter()
        .filter(|r| r["what"] == "host_answer")
        .collect();
    answers.sort_by_key(|r| r["id"].as_u64());
    assert_eq!(
        answers[0]["frame"]["error"]["code"],
        rpc::PERMISSION_DENIED,
        "{answers:?}"
    );
    assert_eq!(
        answers[1]["frame"]["result"]["response"]["type"], "status",
        "{answers:?}"
    );
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}

#[tokio::test]
async fn host_request_through_the_daemon_is_the_adapters_own() {
    // the fixture asks for the status, a stop (above a read grant) and, forging, a reveal
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let fx = install(
        &home,
        InstallSpec {
            perms: Some(json!({ "events": ["lifecycle"] })),
            ..InstallSpec::service(
                "asker",
                json!([{ "on": "event", "times": 1, "do": [
                    { "host_request": { "request": { "type": "status" } } },
                    { "host_request": { "request": stop("victim") } },
                    { "forge_actor": { "actor": "human:root", "request": { "type": "show", "selector": by_name("victim") } } },
                    { "host_request": { "request": stop("victim"), "dry_run": true, "end_user": "alice" } }
                ] }]),
            )
        },
    )
    .await;
    let vars = fx.env();
    let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut daemon = DaemonProc::spawn_with_env(&home, &vars);
    assert!(wait_adapter_state(&daemon, "asker", "ready", 20).await);
    let mut spec = victim_spec(tmp.path(), "302.5");
    spec["env"] = json!({ "DB_DSN": "postgres://user:planted-secret-dsn@db/app" });
    assert_eq!(
        daemon.rpc(json!({ "type": "start", "spec": spec }))["type"],
        "started"
    );

    assert!(
        wait_until(20, || fx
            .report()
            .iter()
            .filter(|r| r["what"] == "host_answer")
            .count()
            >= 4)
        .await,
        "{:?}",
        fx.report()
    );
    // the answers come in the order they were ready, not the order they were asked: by request id
    let mut answered: Vec<(u64, Value)> = fx
        .report()
        .into_iter()
        .filter(|r| r["what"] == "host_answer")
        .map(|r| {
            (
                r["id"].as_u64().unwrap(),
                r["frame"]["result"]["response"].clone(),
            )
        })
        .collect();
    answered.sort_by_key(|(id, _)| *id);
    let answers: Vec<Value> = answered.into_iter().map(|(_, a)| a).collect();
    assert_eq!(answers[0]["type"], "status", "{answers:?}");
    assert_eq!(answers[1]["code"], "E_POLICY_DENIED", "{answers:?}");
    assert_eq!(answers[2]["type"], "show", "{answers:?}");
    assert!(
        !answers[2].to_string().contains("planted-secret-dsn"),
        "{answers:?}"
    );
    // a rehearsal of a stop is a plan, whose decision is the denial (a read grant)
    assert_eq!(answers[3]["type"], "dry_run", "{answers:?}");

    let line = {
        let file = home.join("audit").join("audit-security.jsonl");
        assert!(
            wait_until(15, || read_lines(&file)
                .iter()
                .any(|l| l["end_user"] == "alice"))
            .await,
            "{:?}",
            read_lines(&file)
        );
        read_lines(&file)
            .into_iter()
            .find(|l| l["end_user"] == "alice")
            .unwrap()
    };
    assert_eq!(
        (
            line["actor"]["class"].as_str(),
            line["actor"]["label"].as_str()
        ),
        (Some("adapter"), Some("asker")),
        "{line}"
    );
    assert_eq!(line["dry_run"], true, "{line}");
    assert_eq!(
        daemon.rpc(json!({ "type": "status" }))["apps"][0]["state"]["status"],
        "running"
    );
    daemon.signal(Signal::SIGTERM);
    assert!(daemon.wait_exit().success());
}

#[tokio::test]
async fn host_request_during_shutdown_is_refused() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    // the second event of the service is the stop of the app, which the shutdown makes: then the
    // adapter asks for the status
    let fx = install(
        &home,
        InstallSpec {
            perms: Some(json!({ "events": ["lifecycle"] })),
            ..InstallSpec::service(
                "late",
                // it stays up through the polite request, so that the answer can still reach it
                json!([
                    { "on": "event", "after": 1, "times": 1, "do": [{ "host_request": { "request": { "type": "status" } } }] },
                    { "on": "shutdown", "do": "ignore_shutdown" }
                ]),
            )
        },
    )
    .await;
    let vars = fx.env();
    let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut daemon = DaemonProc::spawn_with_env(&home, &vars);
    assert!(wait_adapter_state(&daemon, "late", "ready", 20).await);
    let spec = victim_spec(tmp.path(), "303.5");
    assert_eq!(
        daemon.rpc(json!({ "type": "start", "spec": spec }))["type"],
        "started"
    );
    // the shutdown must find the app started and the service told so (its first event); the second
    // event is the stop the shutdown makes
    let told = || {
        fx.report()
            .iter()
            .any(|r| r["what"] == "frame" && r["dir"] == "in" && r["frame"]["method"] == "event")
    };
    assert!(wait_until(20, told).await, "the service heard of the start");
    daemon.signal(Signal::SIGTERM);
    let answer = match fx.wait_report("host_answer").await {
        Some(a) => a,
        None => panic!(
            "the adapter was answered during the shutdown\nreport: {:#?}\ndaemon: {}",
            fx.report(),
            daemon.stderr()
        ),
    };
    assert_eq!(
        answer["frame"]["result"]["response"]["code"], "E_SHUTTING_DOWN",
        "{answer}"
    );
    assert!(daemon.wait_exit().success());
}

#[test]
fn the_golden_host_request_is_a_request_the_daemon_can_read() {
    // the proto crate keeps the request as a JSON value, so this is where the example is checked
    // against the real type
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../oxidepm-adapter-proto/tests/golden/host.request.request.json");
    let frame: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let request: Request =
        serde_json::from_value(frame["params"]["request"].clone()).expect("a real request");
    assert!(
        matches!(request, Request::Stop { selector: oxidepm_core::Selector::ByName(ref n) } if n == "web")
    );
}
