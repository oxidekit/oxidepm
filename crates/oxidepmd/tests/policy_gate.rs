//! The gate in process: the apps a request may act on are the ones the policy looked at, even
//! when the registry changes between the check and the action (the test hook holds the request
//! between the two). Private home, real supervisor.

mod common;

use common::*;
use oxidepm_core::actor::Via;
use oxidepm_core::{AppStatus, Selector};
use oxidepm_ipc::{Envelope, Request, Response};
use oxidepmd::handlers::RequestHandler;
use oxidepmd::policy::{PeerCred, ReqCtx};
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
