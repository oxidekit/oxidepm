//! The host API: what an adapter may ask of the daemon.
//!
//! An adapter reaches the daemon through one tunnel on its own protocol channel. `host.request`
//! carries the object the CLI would send; the daemon builds the request context itself (the
//! actor is the adapter, named by the connection, with the grant stored for it) and hands the
//! request to the same entry point as every client request, so the policy, the audit log, the
//! dry-run handling and the limits are the ones that already exist. There is no second
//! implementation of any operation. What the adapter writes beside the request (an `actor`, a
//! `reveal` or a `dry_run` inside the request object, extra fields in the params) is not looked
//! at: the only things taken from the params are the request itself, the `dry_run` flag and the
//! `end_user` label, and the label is only ever written down.
//!
//! The request limits that belong to the connection (requests in flight, the size of an answer)
//! are enforced by [`AdapterConn`](super::conn::AdapterConn); the rate (50 a second, caution
//! requests 5 a minute) is the audit log's limiter, the one every non-human actor goes through.
//! An adapter that keeps asking over its rate earns strikes here.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use oxidepm_adapter_proto::codes::{self, rpc};
use oxidepm_adapter_proto::messages::{
    method, AuditNoteParams, HostRequestParams, HostRequestResult,
};
use oxidepm_adapter_proto::{to_params, ErrorObject, Params};
use oxidepm_core::actor::Via;
use oxidepm_core::{ErrorCode, Tier};
use oxidepm_ipc::{Request, Response};
use parking_lot::Mutex;
use serde_json::Value;

use super::conn::{Inbound, StrikeHandle};
use crate::audit::{ActorView, Audit};
use crate::policy::{AdapterGrant, ReqCtx};

/// Rate-limited answers that, inside [`OVER_RATE_WINDOW`], are a protocol violation.
pub const OVER_RATE_STRIKE: usize = 20;
/// The window those answers are counted in.
pub const OVER_RATE_WINDOW: Duration = Duration::from_secs(10);

/// Where a request goes once the grant has been attached: the daemon's one entry point.
#[async_trait]
pub trait Dispatch: Send + Sync {
    /// Answer `request` as the daemon answers any client request. Never fails: a refusal is a
    /// `Response::Error`.
    async fn dispatch(&self, ctx: &ReqCtx, request: Request) -> Response;
}

/// The daemon's entry point, bound after the host is loaded (the host is made before the core
/// that dispatches requests exists, so the link is filled in later). Every adapter's host API
/// holds the same link.
#[derive(Default)]
pub struct HostLink {
    target: OnceLock<Arc<dyn Dispatch>>,
}

impl HostLink {
    pub fn new() -> Arc<HostLink> {
        Arc::new(HostLink::default())
    }

    /// Bind the daemon. Only the first call counts.
    pub fn bind(&self, target: Arc<dyn Dispatch>) {
        let _ = self.target.set(target);
    }

    fn get(&self) -> Option<&Arc<dyn Dispatch>> {
        self.target.get()
    }
}

/// Clean an `end_user` label: no control characters, at most 64 characters; an empty one is none.
pub fn clean_end_user(label: &str) -> Option<String> {
    let cleaned: String = label
        .chars()
        .filter(|c| !c.is_control())
        .take(oxidepm_adapter_proto::messages::MAX_END_USER_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// The host API of one adapter: answers its `host.request` and `host.audit_note`.
pub struct HostApi {
    name: String,
    /// The channel the requests come in on: the adapter's own, or a session's.
    via: Via,
    grant: AdapterGrant,
    link: Arc<HostLink>,
    audit: Option<Arc<Audit>>,
    /// Numbers the requests of this adapter for the audit log's client id.
    counter: Arc<AtomicU64>,
    strikes: Option<StrikeHandle>,
    /// When this connection was last answered `E_RATE_LIMITED`.
    over_rate: Mutex<VecDeque<Instant>>,
}

impl HostApi {
    pub fn new(
        name: &str,
        grant: AdapterGrant,
        link: Arc<HostLink>,
        audit: Option<Arc<Audit>>,
    ) -> Arc<HostApi> {
        Arc::new(HostApi {
            name: name.to_string(),
            via: Via::AdapterChannel,
            grant,
            link,
            audit,
            counter: Arc::new(AtomicU64::new(1)),
            strikes: None,
            over_rate: Mutex::new(VecDeque::new()),
        })
    }

    fn with_channel(&self, via: Via, strikes: Option<StrikeHandle>) -> Arc<HostApi> {
        Arc::new(HostApi {
            name: self.name.clone(),
            via,
            grant: self.grant,
            link: Arc::clone(&self.link),
            audit: self.audit.clone(),
            counter: Arc::clone(&self.counter),
            strikes,
            over_rate: Mutex::new(VecDeque::new()),
        })
    }

    /// The error answer to a call whose params are not what the method takes. Not a strike.
    fn invalid(why: impl Into<String>) -> ErrorObject {
        ErrorObject::new(rpc::INVALID_PARAMS, why)
    }

    /// Count an answer of `E_RATE_LIMITED` at `now`; true when the adapter has now been answered
    /// that [`OVER_RATE_STRIKE`] times inside [`OVER_RATE_WINDOW`].
    fn over_rate_strike(&self, now: Instant) -> bool {
        let mut times = self.over_rate.lock();
        while times
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > OVER_RATE_WINDOW)
        {
            times.pop_front();
        }
        times.push_back(now);
        if times.len() >= OVER_RATE_STRIKE {
            times.clear();
            return true;
        }
        false
    }

    fn strike(&self, why: &str) {
        if let Some(handle) = &self.strikes {
            handle.strike(why);
        }
    }

    async fn host_request(&self, params: Params) -> Result<Params, ErrorObject> {
        let parsed: HostRequestParams = serde_json::from_value(Value::Object(params))
            .map_err(|e| Self::invalid(format!("host.request: {e}")))?;
        let request: Request = serde_json::from_value(parsed.request)
            .map_err(|e| Self::invalid(format!("host.request: not a request: {e}")))?;
        let end_user = parsed.end_user.as_deref().and_then(clean_end_user);

        let Some(target) = self.link.get() else {
            return Err(ErrorObject::with_name(
                rpc::SHUTTING_DOWN,
                ErrorCode::ShuttingDown.as_str(),
                "the daemon is not ready for requests",
            ));
        };
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let ctx = ReqCtx::for_adapter(
            &self.name,
            self.grant,
            self.via,
            format!("{}#{n}", self.name),
            parsed.dry_run,
            end_user,
        );
        let response = target.dispatch(&ctx, request).await;

        if matches!(
            response,
            Response::Error {
                code: ErrorCode::RateLimited,
                ..
            }
        ) && self.over_rate_strike(Instant::now())
        {
            self.strike("kept asking over its rate");
        }
        let value = serde_json::to_value(&response).map_err(|e| {
            ErrorObject::new(
                rpc::INTERNAL_ERROR,
                format!("the answer cannot be encoded: {e}"),
            )
        })?;
        to_params(&HostRequestResult { response: value }).map_err(|e| {
            ErrorObject::new(
                rpc::INTERNAL_ERROR,
                format!("the answer cannot be encoded: {e}"),
            )
        })
    }

    /// Record the adapter's identity and a fixed event message. Notes count against its rate.
    fn audit_note(&self, params: Params) -> Result<Params, ErrorObject> {
        let _: AuditNoteParams = serde_json::from_value(Value::Object(params))
            .map_err(|e| Self::invalid(format!("host.audit_note: {e}")))?;
        if let Some(audit) = &self.audit {
            let actor = oxidepm_core::actor::ActorId::adapter(&self.name, self.via);
            let view = ActorView::of(&actor, None, None);
            if let Err(e) = audit.check_rate(&actor, &view, Tier::Safe) {
                if self.over_rate_strike(Instant::now()) {
                    self.strike("kept asking over its rate");
                }
                return Err(ErrorObject::with_name(
                    rpc::RATE_LIMITED,
                    codes::E_RATE_LIMITED,
                    e.to_string(),
                ));
            }
            audit.note(&self.name, "Adapter reported a note");
        }
        Ok(Params::new())
    }
}

#[async_trait]
impl Inbound for HostApi {
    fn for_connection(&self, strikes: StrikeHandle) -> Option<Arc<dyn Inbound>> {
        Some(self.with_channel(self.via, Some(strikes)))
    }

    fn for_session(&self) -> Option<Arc<dyn Inbound>> {
        Some(self.with_channel(Via::AdapterSession, None))
    }

    async fn request(&self, name: &str, params: Params) -> Result<Params, ErrorObject> {
        match name {
            method::HOST_REQUEST => self.host_request(params).await,
            method::HOST_AUDIT_NOTE => self.audit_note(params),
            other => Err(ErrorObject::new(
                rpc::METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            )),
        }
    }
}
