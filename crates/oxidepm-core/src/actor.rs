//! Who is asking: the class of the caller, a label, and how the daemon knows.
//!
//! The daemon assigns the class from the kind of connection a request arrived on. What the
//! client writes in the envelope (`OXIDEPM_ACTOR`) is only a claim, and a claim can make a
//! request stricter, never looser (see [`ActorId::from_connection`]).

use std::sync::Arc;

/// Longest label, in characters.
pub const MAX_LABEL_LEN: usize = 32;

/// The label of a request whose claim could not be used.
pub const INVALID_CLAIM_LABEL: &str = "invalid-claim";

/// The label of a connection on the agent socket that names itself with nothing usable.
pub const AGENT_LABEL: &str = "agent";

/// The label of a request on the main socket that claims nothing.
pub const DEFAULT_HUMAN_LABEL: &str = "cli";

/// The label of a connection whose peer could not be attributed while an adapter runs.
pub const UNRESOLVED_LABEL: &str = "unresolved";

/// What kind of caller made a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActorClass {
    /// A person (or a script that does not say otherwise).
    Human,
    /// An AI agent, declared or put there by the agent socket.
    Ai,
    /// A named adapter, acting under its grant.
    Adapter,
    /// The daemon's own supervision: cron, crash restarts, limits, the boot resurrect.
    System,
}

impl ActorClass {
    /// The prefix used on the wire and in logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            ActorClass::Human => "human",
            ActorClass::Ai => "ai",
            ActorClass::Adapter => "adapter",
            ActorClass::System => "system",
        }
    }

    fn from_prefix(prefix: &str) -> Option<Self> {
        match prefix {
            "human" => Some(ActorClass::Human),
            "ai" => Some(ActorClass::Ai),
            "adapter" => Some(ActorClass::Adapter),
            "system" => Some(ActorClass::System),
            _ => None,
        }
    }
}

/// The way a request reached the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Via {
    /// The main socket.
    Main,
    /// The agent socket.
    Agent,
    /// An adapter's own protocol channel.
    AdapterChannel,
    /// The protocol channel of an adapter that was started for a session (it owns a terminal).
    AdapterSession,
    /// The daemon itself.
    Internal,
    /// A connection on the main socket whose peer could not be attributed while an adapter runs
    /// (the process was gone when the daemon looked). It is class `ai`, never `human`.
    Unattributed,
}

impl Via {
    pub fn as_str(&self) -> &'static str {
        match self {
            Via::Main => "main",
            Via::Agent => "agent",
            Via::AdapterChannel => "adapter",
            Via::AdapterSession => "adapter_session",
            Via::Internal => "internal",
            Via::Unattributed => "unattributed",
        }
    }
}

/// Who is asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorId {
    pub class: ActorClass,
    /// `cli`, `example-agent`, `web`, `cron`: `[A-Za-z0-9._-]{1,32}`.
    pub label: Arc<str>,
    pub via: Via,
}

/// What a client wrote in the envelope's `actor` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// `human:<label>`
    Human(Arc<str>),
    /// `ai:<label>`
    Ai(Arc<str>),
    /// Anything else: another prefix (`adapter:` and `system:` may never be claimed), a label
    /// with other characters than `[A-Za-z0-9._-]`, an empty or a too long label, no colon.
    Invalid,
}

/// Is `label` a usable label?
pub fn is_valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL_LEN
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

/// Parse a claim. Only `human:<label>` and `ai:<label>` are accepted.
pub fn parse_claim(claim: &str) -> Claim {
    let Some((prefix, label)) = claim.split_once(':') else {
        return Claim::Invalid;
    };
    if !is_valid_label(label) {
        return Claim::Invalid;
    }
    match prefix {
        "human" => Claim::Human(Arc::from(label)),
        "ai" => Claim::Ai(Arc::from(label)),
        _ => Claim::Invalid,
    }
}

impl ActorId {
    pub fn new(class: ActorClass, label: &str, via: Via) -> Self {
        Self {
            class,
            label: Arc::from(label),
            via,
        }
    }

    /// A named adapter acting under its grant, reached `via` its protocol channel or (when the
    /// daemon found the peer of a socket in its process tree) a socket.
    pub fn adapter(name: &str, via: Via) -> Self {
        Self::new(ActorClass::Adapter, name, via)
    }

    /// A peer of the main socket that could not be attributed while an adapter process is alive:
    /// stricter than a person, never one.
    pub fn unresolved() -> Self {
        Self::new(ActorClass::Ai, UNRESOLVED_LABEL, Via::Unattributed)
    }

    /// The daemon's own work (`cron`, `crash`, `resurrect`).
    pub fn system(label: &str) -> Self {
        Self::new(ActorClass::System, label, Via::Internal)
    }

    /// Decide who is asking from the kind of connection and the claim in the envelope.
    ///
    /// - The agent socket (`Via::Agent`): always `Ai`. The label is the one of an `ai:<label>`
    ///   claim, else `agent`. A `human:` claim is ignored.
    /// - The main socket: no claim is `human:cli`; `human:<label>` stays human; `ai:<label>` is
    ///   `Ai`; anything else (including `adapter:*` and `system:*`) is `ai:invalid-claim`. Doubt
    ///   makes the request stricter, never looser.
    ///
    /// Any other `via` is not a socket a client connects to and is treated like the main one.
    pub fn from_connection(via: Via, claim: Option<&str>) -> Self {
        let parsed = claim.map(parse_claim);
        if via == Via::Agent {
            let label = match parsed {
                Some(Claim::Ai(label)) => label,
                _ => Arc::from(AGENT_LABEL),
            };
            return Self {
                class: ActorClass::Ai,
                label,
                via,
            };
        }
        let (class, label) = match parsed {
            None => (ActorClass::Human, Arc::from(DEFAULT_HUMAN_LABEL)),
            Some(Claim::Human(label)) => (ActorClass::Human, label),
            Some(Claim::Ai(label)) => (ActorClass::Ai, label),
            Some(Claim::Invalid) => (ActorClass::Ai, Arc::from(INVALID_CLAIM_LABEL)),
        };
        Self { class, label, via }
    }

    /// `class:label`, as written in logs, events and the audit log.
    pub fn wire(&self) -> String {
        format!("{}:{}", self.class.as_str(), self.label)
    }

    /// The inverse of [`wire`](Self::wire) (the `via` is not part of the wire form).
    pub fn from_wire(wire: &str, via: Via) -> Option<Self> {
        let (prefix, label) = wire.split_once(':')?;
        if !is_valid_label(label) {
            return None;
        }
        Some(Self {
            class: ActorClass::from_prefix(prefix)?,
            label: Arc::from(label),
            via,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_parse_human_label() {
        assert_eq!(parse_claim("human:alice"), Claim::Human(Arc::from("alice")));
        assert_eq!(
            parse_claim("human:a.b_c-9"),
            Claim::Human(Arc::from("a.b_c-9"))
        );
    }

    #[test]
    fn claim_parse_ai_label() {
        assert_eq!(
            parse_claim("ai:example-agent"),
            Claim::Ai(Arc::from("example-agent"))
        );
        assert_eq!(parse_claim("ai:X"), Claim::Ai(Arc::from("X")));
    }

    #[test]
    fn claim_with_bad_charset_is_invalid_claim() {
        for bad in [
            "ai:cl aude",
            "ai:clau/de",
            "human:a:b",
            "ai:é",
            "ai:",
            "ai",
            "",
            ":x",
            "AI:x",
            "ai:a\nb",
        ] {
            assert_eq!(parse_claim(bad), Claim::Invalid, "{bad:?}");
        }
        let id = ActorId::from_connection(Via::Main, Some("ai:cl aude"));
        assert_eq!(id.wire(), "ai:invalid-claim");
    }

    #[test]
    fn claim_adapter_prefix_is_not_trusted() {
        for claim in ["adapter:ui-web", "system:cron", "root:x"] {
            assert_eq!(parse_claim(claim), Claim::Invalid, "{claim}");
            let id = ActorId::from_connection(Via::Main, Some(claim));
            assert_eq!(
                (id.class, &*id.label),
                (ActorClass::Ai, INVALID_CLAIM_LABEL),
                "{claim}"
            );
        }
    }

    #[test]
    fn claim_longer_than_32_is_invalid() {
        let ok = "a".repeat(32);
        let long = "a".repeat(33);
        assert_eq!(
            parse_claim(&format!("ai:{ok}")),
            Claim::Ai(Arc::from(ok.as_str()))
        );
        assert_eq!(parse_claim(&format!("ai:{long}")), Claim::Invalid);
        assert_eq!(parse_claim(&format!("human:{long}")), Claim::Invalid);
    }

    #[test]
    fn actor_wire_roundtrip() {
        for (class, label) in [
            (ActorClass::Human, "cli"),
            (ActorClass::Ai, "example-agent"),
            (ActorClass::Adapter, "ui-web"),
            (ActorClass::System, "cron"),
        ] {
            let id = ActorId::new(class, label, Via::Main);
            let wire = id.wire();
            assert_eq!(wire, format!("{}:{}", class.as_str(), label));
            assert_eq!(ActorId::from_wire(&wire, Via::Main), Some(id));
        }
        assert_eq!(ActorId::from_wire("nobody:x", Via::Main), None);
        assert_eq!(ActorId::from_wire("ai:", Via::Main), None);
        assert_eq!(ActorId::from_wire("ai", Via::Main), None);
    }

    #[test]
    fn main_socket_without_claim_is_human_cli() {
        let id = ActorId::from_connection(Via::Main, None);
        assert_eq!(
            (id.class, id.wire().as_str(), id.via),
            (ActorClass::Human, "human:cli", Via::Main)
        );
        assert_eq!(
            ActorId::from_connection(Via::Main, Some("human:me")).wire(),
            "human:me"
        );
        assert_eq!(
            ActorId::from_connection(Via::Main, Some("ai:other-agent")).wire(),
            "ai:other-agent"
        );
    }

    #[test]
    fn agent_socket_is_always_ai() {
        for (claim, want) in [
            (None, "ai:agent"),
            (Some("human:me"), "ai:agent"),
            (Some("ai:other-agent"), "ai:other-agent"),
            (Some("system:cron"), "ai:agent"),
            (Some("ai:bad label"), "ai:agent"),
        ] {
            let id = ActorId::from_connection(Via::Agent, claim);
            assert_eq!(
                (id.class, id.wire().as_str(), id.via),
                (ActorClass::Ai, want, Via::Agent),
                "{claim:?}"
            );
        }
    }
}
