//! The gate in process: the apps a request may act on are the ones the policy looked at, even
//! when the registry changes between the check and the action (the test hook holds the request
//! between the two). Private home, real supervisor.

mod common;

use common::*;
use oxidepm_adapter_proto::{HostLevel, HostScope};
use oxidepm_core::actor::Via;
use oxidepm_core::{AppStatus, Desired, Selector};
use oxidepm_db::Database;
use oxidepm_ipc::{Envelope, Request, Response};
use oxidepmd::handlers::RequestHandler;
use oxidepmd::policy::{AdapterGrant, PeerCred, ReqCtx};
use serde_json::json;
use std::sync::Arc;

fn ctx(claim: Option<&str>, request: &Request) -> ReqCtx {
    let mut envelope = Envelope::new("req-gate", request.clone());
    envelope.actor = claim.map(str::to_string);
    ReqCtx::new(PeerCred::default(), Via::Main, &envelope)
}

fn sleeper(h: &Harness, name: &str, extra: serde_json::Value) -> oxidepm_core::AppSpec {
    h.cmd_with(name, &format!("exec sleep {}", h.marker()), extra)
}

#[tokio::test]
async fn app_tagged_between_authorize_and_execute_is_not_touched() {
    let h = Harness::new().await;
    h.start(sleeper(&h, "early", json!({ "tags": ["t"] })))
        .await
        .unwrap();
    // an app at rest, without the tag
    h.start(sleeper(&h, "late", json!({}))).await.unwrap();
    h.stop("late").await.unwrap();
    h.wait_status("late", AppStatus::Stopped).await;

    let handler = Arc::new(RequestHandler::new(Arc::clone(&h.sup)));
    let pause = handler.pause_after_authorize();
    let request = Request::Stop {
        selector: Selector::ByTag("t".into()),
    };
    let task = {
        let (handler, ctx, request) = (Arc::clone(&handler), ctx(None, &request), request.clone());
        tokio::spawn(async move { handler.guarded(&ctx, request).await })
    };
    pause.reached.notified().await;
    // the request has been authorized for `early` alone; now `late` gets the tag and runs
    h.start(sleeper(&h, "late", json!({ "tags": ["t"] })))
        .await
        .unwrap();
    h.wait_status("late", AppStatus::Running).await;
    pause.release.notify_one();

    let response = task.await.unwrap().expect("the stop was carried out");
    match response {
        Response::Stopped { count, items } => {
            assert_eq!(count, 1, "{items:?}");
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].name, "early");
        }
        other => panic!("{other:?}"),
    }
    h.wait_status("early", AppStatus::Stopped).await;
    assert_eq!(
        h.state("late").await.status,
        AppStatus::Running,
        "the app tagged in between runs on"
    );
}

#[tokio::test]
async fn app_created_between_authorize_and_execute_is_not_touched() {
    let h = Harness::new().await;
    h.start(sleeper(&h, "first", json!({}))).await.unwrap();

    let handler = Arc::new(RequestHandler::new(Arc::clone(&h.sup)));
    let pause = handler.pause_after_authorize();
    let request = Request::Stop {
        selector: Selector::All,
    };
    let task = {
        let (handler, ctx, request) = (
            Arc::clone(&handler),
            ctx(Some("human:alice"), &request),
            request.clone(),
        );
        tokio::spawn(async move { handler.guarded(&ctx, request).await })
    };
    pause.reached.notified().await;
    h.start(sleeper(&h, "fresh", json!({}))).await.unwrap();
    pause.release.notify_one();

    let response = task.await.unwrap().expect("the stop was carried out");
    assert!(
        matches!(response, Response::Stopped { count: 1, .. }),
        "{response:?}"
    );
    h.wait_status("first", AppStatus::Stopped).await;
    assert_eq!(
        h.state("fresh").await.status,
        AppStatus::Running,
        "the app created in between runs on"
    );
}

/// The gate refuses before anything is resolved or run, in process too.
#[tokio::test]
async fn the_gate_refuses_an_ai_actor_before_it_acts() {
    let h = Harness::new().await;
    h.start(sleeper(&h, "closed", json!({}))).await.unwrap();
    let handler = RequestHandler::new(Arc::clone(&h.sup));
    let request = Request::Stop {
        selector: Selector::ByName("closed".into()),
    };
    let err = handler
        .guarded(&ctx(Some("ai:test"), &request), request)
        .await
        .unwrap_err();
    assert_eq!(err.code().as_str(), "E_POLICY_DENIED");
    assert_eq!(h.state("closed").await.status, AppStatus::Running);
}

#[tokio::test]
async fn refused_start_does_not_record_launch_details() {
    let h = Harness::new().await;
    let handler = RequestHandler::new(Arc::clone(&h.sup));
    let mut spec = h.cmd("sample", "true");
    spec.command = "/path/to/example-program".into();
    spec.env
        .insert("EXAMPLE_VALUE".into(), "example-program".into());
    let request = Request::Start { spec };
    let err = handler
        .guarded(&ctx(Some("ai:test"), &request), request)
        .await
        .unwrap_err();
    assert_eq!(err.code().as_str(), "E_POLICY_DENIED");
    assert!(h.status().await.is_empty());
    handler.close_audit().await;

    let log = std::fs::read_to_string(h.paths.audit_dir().join("audit-security.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(&log).unwrap();
    assert_eq!(record["decision"], "deny");
    assert_eq!(record["selector"], "sample");
    assert!(record.get("program").is_none(), "{record}");
    assert!(record.get("args_sha256").is_none(), "{record}");
}

#[tokio::test]
async fn broken_stored_apps_preserve_protection() {
    let h = Harness::new().await;
    let db = Database::new(&h.paths.db()).await.unwrap();
    for (name, protected) in [("protected", true), ("plain", false), ("unknown", false)] {
        let spec = sleeper(&h, name, json!({ "protected": protected }));
        db.apps()
            .insert_with(&spec, None, Desired::Stopped)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE apps SET spec_json = 'invalid' WHERE name IN ('protected', 'plain')")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE apps SET protected = 'invalid' WHERE name = 'unknown'")
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(h.sup.load_registry().await.unwrap().broken, 3);

    let handler = RequestHandler::new(Arc::clone(&h.sup));
    let adapter = ReqCtx::for_adapter(
        "deployer",
        AdapterGrant {
            level: HostLevel::Admin,
            scope: HostScope::All,
        },
        Via::AdapterChannel,
        "req-gate".into(),
        false,
        None,
    );
    for name in ["protected", "unknown", "plain"] {
        let selector = Selector::ByName(name.into());
        let app = h.sup.show(&selector).await.unwrap().unwrap();
        assert_eq!(app.spec.protected, name != "plain", "{name}");
        assert!(!app.spec.ai_control, "{name}");
        let result = handler
            .guarded(&adapter, Request::Delete { selector })
            .await;
        if name == "plain" {
            assert!(
                matches!(result, Ok(Response::Deleted { count: 1, .. })),
                "{result:?}"
            );
        } else {
            assert_eq!(result.unwrap_err().code().as_str(), "E_PROTECTED", "{name}");
        }
    }
}
