//! Fixtures shared by the unit tests.

use serde_json::{json, Value};

use crate::manifest::{Invalid, Manifest, Rule, ValidateCtx};

pub const DAEMON: &str = "0.4.0";

/// A valid manifest for a tool that provides a runtime and one safe verb.
pub fn base_manifest() -> Value {
    json!({
        "schema": 1,
        "name": "runtime-demo",
        "version": "0.1.0",
        "kind": "tool",
        "summary": "Run demo apps.",
        "description": "Provides the demo runtime.\nIt returns steps and a plan.",
        "tags": ["runtime"],
        "protocol": [1],
        "min_daemon": "0.4.0",
        "exec": "bin/runtime-demo",
        "provides": {
            "runtimes": ["demo"],
            "verbs": [{"name": "check", "summary": "Check an app.", "tier": "safe", "mode": "relayed",
                       "writes": false, "dry_run": true, "max_runtime_secs": 600, "args_summary": "<dir>"}],
            "serves": [],
            "programs": ["demo", "demo-build"],
            "capture_programs": ["demo-build"],
            "env_set": ["DEMO_MODE"]
        },
        "permissions": {
            "host": {"level": "read", "scope": "all"},
            "events": [], "env_pass": [],
            "network": {"mode": "none"},
            "fs": {"read": ["$STATE"], "write": []},
            "spawn": {"mode": "none"},
            "memory_mb": 64,
            "requires_enforcement": ["network"]
        }
    })
}

/// A valid service manifest.
pub fn service_manifest() -> Value {
    let mut m = base_manifest();
    m["name"] = json!("notifier");
    m["kind"] = json!("service");
    m["provides"] = json!({"verbs": [], "serves": []});
    m["permissions"]["events"] = json!(["lifecycle"]);
    m
}

pub fn ctx() -> ValidateCtx<'static> {
    ValidateCtx::new(semver::Version::parse(DAEMON).unwrap())
}

pub fn validate(v: &Value) -> Result<Manifest, Invalid> {
    Manifest::parse_validated(v.to_string().as_bytes(), &ctx())
}

pub fn validate_with(v: &Value, ctx: &ValidateCtx) -> Result<Manifest, Invalid> {
    Manifest::parse_validated(v.to_string().as_bytes(), ctx)
}

/// The rule a manifest breaks; panics when it is valid.
pub fn rule_of(v: &Value) -> Rule {
    match validate(v) {
        Ok(_) => panic!("expected the manifest to be refused: {v}"),
        Err(e) => e.rule,
    }
}

pub fn parse(v: &Value) -> Manifest {
    Manifest::parse(v.to_string().as_bytes()).expect("parse")
}

pub const SHA_A: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";
pub const SHA_B: &str = "cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34cd34";
pub const SHA_C: &str = "ee56ee56ee56ee56ee56ee56ee56ee56ee56ee56ee56ee56ee56ee56ee56ee56";
