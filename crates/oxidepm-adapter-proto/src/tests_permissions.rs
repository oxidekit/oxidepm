//! Permissions growth: the field is flagged when it grows, is not flagged when it shrinks
//! or stays, and the fingerprint covers the release.

use serde_json::{json, Value};

use crate::manifest::Manifest;
use crate::testutil::*;

/// Fields flagged when going from the base manifest with `old_edit` to the base with `new_edit`.
fn grown(
    old_edit: impl FnOnce(&mut Value),
    new_edit: impl FnOnce(&mut Value),
) -> Vec<&'static str> {
    let (mut old, mut new) = (base_manifest(), base_manifest());
    old_edit(&mut old);
    new_edit(&mut new);
    parse(&new)
        .grows_over(&parse(&old))
        .into_iter()
        .map(|g| g.field)
        .collect()
}

/// `field` grows from `a` to `b` and, reversed, nothing grows.
fn only(field: &'static str, path: &[&str], a: Value, b: Value) {
    let set = |v: &mut Value, x: &Value| {
        let mut cur = v;
        for k in &path[..path.len() - 1] {
            cur = &mut cur[*k];
        }
        cur[path[path.len() - 1]] = x.clone();
    };
    assert_eq!(
        grown(|v| set(v, &a), |v| set(v, &b)),
        vec![field],
        "{field}: {a} to {b}"
    );
    assert_eq!(
        grown(|v| set(v, &b), |v| set(v, &a)),
        Vec::<&str>::new(),
        "{field}: reverse {b} to {a}"
    );
}

#[test]
fn permissions_grow_nothing_when_equal() {
    assert!(grown(|_| {}, |_| {}).is_empty());
    let m = parse(&base_manifest());
    assert!(m.grows_over(&m).is_empty());
}

#[test]
fn permissions_grow_level() {
    only(
        "host.level",
        &["permissions", "host", "level"],
        json!("read"),
        json!("lifecycle"),
    );
    only(
        "host.level",
        &["permissions", "host", "level"],
        json!("lifecycle"),
        json!("admin"),
    );
    only(
        "host.level",
        &["permissions", "host", "level"],
        json!("read"),
        json!("admin"),
    );
}

#[test]
fn permissions_grow_scope() {
    only(
        "host.scope",
        &["permissions", "host", "scope"],
        json!("opted-in"),
        json!("all"),
    );
}

#[test]
fn permissions_grow_max_in_flight() {
    only(
        "host.max_in_flight",
        &["permissions", "host", "max_in_flight"],
        json!(32),
        json!(128),
    );
    // Not saying anything means the default of 32.
    assert_eq!(
        grown(
            |_| {},
            |v| v["permissions"]["host"]["max_in_flight"] = json!(64)
        ),
        vec!["host.max_in_flight"]
    );
    assert!(grown(
        |v| v["permissions"]["host"]["max_in_flight"] = json!(32),
        |_| {}
    )
    .is_empty());
}

#[test]
fn permissions_grow_events() {
    only(
        "events",
        &["permissions", "events"],
        json!(["lifecycle"]),
        json!(["lifecycle", "adapter"]),
    );
    only(
        "events",
        &["permissions", "events"],
        json!([]),
        json!(["lifecycle"]),
    );
    // The order of a list is not growth.
    assert!(grown(
        |v| v["permissions"]["events"] = json!(["lifecycle", "adapter"]),
        |v| v["permissions"]["events"] = json!(["adapter", "lifecycle"])
    )
    .is_empty());
}

#[test]
fn permissions_grow_env_pass() {
    only(
        "env_pass",
        &["permissions", "env_pass"],
        json!([]),
        json!(["SSH_AUTH_SOCK"]),
    );
    only(
        "env_pass",
        &["permissions", "env_pass"],
        json!(["HOME"]),
        json!(["HOME", "AWS_PROFILE"]),
    );
}

#[test]
fn environment_socket_grants_change_consent() {
    let mut old = base_manifest();
    old["permissions"]["env_pass"] = json!(["SSH_AUTH_SOCK", "OTHER_SOCKET"]);
    let mut new = old.clone();
    new["permissions"]["connect_env_socket"] = json!(["SSH_AUTH_SOCK", "OTHER_SOCKET"]);
    let hash = "a".repeat(64);
    let old = parse(&old);
    let granted = parse(&new);
    assert_eq!(
        granted
            .grows_over(&old)
            .iter()
            .map(|g| g.field)
            .collect::<Vec<_>>(),
        ["connect_env_socket"]
    );
    assert!(old.grows_over(&granted).is_empty());
    assert_ne!(granted.fingerprint(&hash), old.fingerprint(&hash));

    new["permissions"]["connect_env_socket"] = json!(["OTHER_SOCKET", "SSH_AUTH_SOCK"]);
    assert_eq!(granted.fingerprint(&hash), parse(&new).fingerprint(&hash));
    new["permissions"]["connect_env_socket"] = json!([]);
    assert_eq!(old.fingerprint(&hash), parse(&new).fingerprint(&hash));
}

#[test]
fn permissions_grow_network_mode() {
    only(
        "network.mode",
        &["permissions", "network", "mode"],
        json!("none"),
        json!("outbound"),
    );
}

#[test]
fn permissions_grow_hosts() {
    only(
        "network.hosts",
        &["permissions", "network", "hosts"],
        json!([]),
        json!(["api.telegram.org"]),
    );
    only(
        "network.hosts",
        &["permissions", "network", "hosts"],
        json!(["a.example"]),
        json!(["a.example", "b.example"]),
    );
}

#[test]
fn permissions_grow_listen() {
    only(
        "network.listen",
        &["permissions", "network", "listen"],
        json!([]),
        json!(["127.0.0.1:9615"]),
    );
    only(
        "network.listen",
        &["permissions", "network", "listen"],
        json!(["127.0.0.1:9615"]),
        json!(["127.0.0.1:9615", "0.0.0.0:80"]),
    );
}

#[test]
fn permissions_grow_fs_read() {
    only(
        "fs.read",
        &["permissions", "fs", "read"],
        json!(["$STATE"]),
        json!(["$STATE", "/etc"]),
    );
    // Replacing a path is a new path, in either direction.
    assert_eq!(
        grown(
            |v| v["permissions"]["fs"]["read"] = json!(["/a"]),
            |v| v["permissions"]["fs"]["read"] = json!(["/b"])
        ),
        vec!["fs.read"]
    );
}

#[test]
fn permissions_grow_fs_write() {
    only(
        "fs.write",
        &["permissions", "fs", "write"],
        json!([]),
        json!(["$STATE"]),
    );
    only(
        "fs.write",
        &["permissions", "fs", "write"],
        json!(["$STATE"]),
        json!(["$STATE", "$CWD"]),
    );
}

#[test]
fn permissions_grow_any() {
    // `any` is above every list, and nothing is above `any`.
    only(
        "fs.read",
        &["permissions", "fs", "read"],
        json!(["$STATE", "/etc"]),
        json!(["any"]),
    );
    // Once it has `any`, listing more paths is nothing new; and `any` to `any` is the same.
    assert!(grown(
        |v| v["permissions"]["fs"]["read"] = json!(["any"]),
        |v| v["permissions"]["fs"]["read"] = json!(["any", "/etc", "$CWD"])
    )
    .is_empty());
    only(
        "fs.write",
        &["permissions", "fs", "write"],
        json!([]),
        json!(["any"]),
    );
}

#[test]
fn permissions_grow_spawn() {
    only(
        "spawn.mode",
        &["permissions", "spawn", "mode"],
        json!("none"),
        json!("named"),
    );
    only(
        "spawn.mode",
        &["permissions", "spawn", "mode"],
        json!("named"),
        json!("any"),
    );
    only(
        "spawn.mode",
        &["permissions", "spawn", "mode"],
        json!("none"),
        json!("any"),
    );
}

#[test]
fn permissions_grow_names() {
    let named = |names: Value| json!({"mode": "named", "names": names});
    only(
        "spawn.names",
        &["permissions", "spawn"],
        named(json!(["git"])),
        named(json!(["git", "ssh"])),
    );
}

#[test]
fn permissions_grow_memory() {
    only(
        "memory_mb",
        &["permissions", "memory_mb"],
        json!(64),
        json!(65),
    );
    only(
        "memory_mb",
        &["permissions", "memory_mb"],
        json!(128),
        json!(512),
    );
}

#[test]
fn permissions_grow_runtimes() {
    only(
        "provides.runtimes",
        &["provides", "runtimes"],
        json!(["demo"]),
        json!(["demo", "other"]),
    );
}

#[test]
fn permissions_grow_verbs() {
    let v = |name: &str| json!({"name": name, "summary": "s", "tier": "caution"});
    only(
        "provides.verbs",
        &["provides", "verbs"],
        json!([v("one")]),
        json!([v("one"), v("two")]),
    );
    only(
        "provides.verbs",
        &["provides", "verbs"],
        json!([]),
        json!([v("one")]),
    );
}

#[test]
fn permissions_grow_serves() {
    only(
        "provides.serves",
        &["provides", "serves"],
        json!([]),
        json!(["provider.complete"]),
    );
}

#[test]
fn permissions_grow_programs() {
    only(
        "provides.programs",
        &["provides", "programs"],
        json!(["demo", "demo-build"]),
        json!(["demo", "demo-build", "sh"]),
    );
}

#[test]
fn permissions_grow_capture_programs() {
    only(
        "provides.capture_programs",
        &["provides", "capture_programs"],
        json!([]),
        json!(["demo"]),
    );
}

#[test]
fn permissions_grow_env_set() {
    only(
        "provides.env_set",
        &["provides", "env_set"],
        json!(["DEMO_MODE"]),
        json!(["DEMO_MODE", "OTHER"]),
    );
}

#[test]
fn permissions_grow_ai_driven_off() {
    // Going from ai_driven to not ai_driven drops the ai_control requirement; the other way is a
    // restriction.
    only("ai_driven", &["ai_driven"], json!(true), json!(false));
}

#[test]
fn permissions_grow_verb_tier_lower() {
    let v = |tier: &str| json!([{"name": "fixit", "summary": "s", "tier": tier}]);
    // A lower tier is guarded less: dangerous to caution to safe is growth, the other way is not.
    only(
        "verb.tier",
        &["provides", "verbs"],
        v("dangerous"),
        v("caution"),
    );
    only("verb.tier", &["provides", "verbs"], v("caution"), v("safe"));
}

#[test]
fn permissions_grow_verb_writes_on() {
    let v = |w: bool| json!([{"name": "fixit", "summary": "s", "tier": "caution", "writes": w}]);
    only("verb.writes", &["provides", "verbs"], v(false), v(true));
}

#[test]
fn permissions_grow_requires_enforcement_removed() {
    only(
        "requires_enforcement",
        &["permissions", "requires_enforcement"],
        json!(["network", "spawn"]),
        json!(["network"]),
    );
    only(
        "requires_enforcement",
        &["permissions", "requires_enforcement"],
        json!(["network"]),
        json!([]),
    );
}

#[test]
fn permissions_grow_kind() {
    // Validation is not run here, only the comparison.
    // A different kind is a different process model, so it is flagged both ways.
    assert_eq!(
        grown(|_| {}, |v| v["kind"] = json!("service")),
        vec!["kind"]
    );
    assert_eq!(
        grown(|v| v["kind"] = json!("service"), |_| {}),
        vec!["kind"]
    );
}

#[test]
fn permissions_grow_exec() {
    assert_eq!(
        grown(|_| {}, |v| v["exec"] = json!("bin/other")),
        vec!["exec"]
    );
}

#[test]
fn permissions_grow_protocol() {
    only("protocol", &["protocol"], json!([1]), json!([1, 2]));
}

#[test]
fn permissions_grow_reports_every_field_and_a_sentence() {
    let old = parse(&base_manifest());
    let mut v = base_manifest();
    v["permissions"]["network"]["mode"] = json!("outbound");
    v["permissions"]["spawn"]["mode"] = json!("any");
    v["permissions"]["fs"]["read"] = json!(["any"]);
    let g = parse(&v).grows_over(&old);
    let fields: Vec<_> = g.iter().map(|g| g.field).collect();
    assert_eq!(fields, vec!["network.mode", "fs.read", "spawn.mode"]);
    assert!(g.iter().all(|g| !g.detail.is_empty()));
}

// The fingerprint.

fn fp(v: &Value) -> String {
    parse(v).fingerprint(SHA_A)
}

#[test]
fn fingerprint_is_order_independent_and_covers_the_release() {
    let base = fp(&base_manifest());
    assert_eq!(base.len(), 16);
    assert!(base
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));

    // The same content in another order is the same fingerprint.
    let mut v = base_manifest();
    v["provides"]["programs"] = json!(["demo-build", "demo", "demo"]);
    v["provides"]["verbs"] = json!([
        {"name": "zeta", "summary": "z", "tier": "safe"},
        {"name": "alpha", "summary": "a", "tier": "safe"}
    ]);
    let mut w = v.clone();
    w["provides"]["programs"] = json!(["demo", "demo-build"]);
    w["provides"]["verbs"] = json!([
        {"name": "alpha", "summary": "a", "tier": "safe"},
        {"name": "zeta", "summary": "z", "tier": "safe"}
    ]);
    assert_eq!(fp(&v), fp(&w));
    let mut a = base_manifest();
    a["permissions"]["env_pass"] = json!(["B", "A"]);
    a["permissions"]["network"]["hosts"] = json!(["y.example", "x.example"]);
    a["protocol"] = json!([2, 1]);
    let mut b = a.clone();
    b["permissions"]["env_pass"] = json!(["A", "B"]);
    b["permissions"]["network"]["hosts"] = json!(["x.example", "y.example"]);
    b["protocol"] = json!([1, 2]);
    assert_eq!(fp(&a), fp(&b));
    // And the hash of the executable is compared in either case.
    assert_eq!(
        parse(&a).fingerprint(&SHA_A.to_uppercase()),
        parse(&a).fingerprint(SHA_A)
    );

    // Anything that is part of the release changes it.
    let m = parse(&base_manifest());
    assert_ne!(m.fingerprint(SHA_A), m.fingerprint(SHA_B), "the executable");
    type Edit = Box<dyn Fn(&mut Value)>;
    let edits: Vec<(&str, Edit)> = vec![
        ("name", Box::new(|v| v["name"] = json!("runtime-other"))),
        ("version", Box::new(|v| v["version"] = json!("0.1.1"))),
        ("kind", Box::new(|v| v["kind"] = json!("service"))),
        ("exec", Box::new(|v| v["exec"] = json!("bin/other"))),
        ("protocol", Box::new(|v| v["protocol"] = json!([1, 2]))),
        ("ai_driven", Box::new(|v| v["ai_driven"] = json!(true))),
        (
            "runtimes",
            Box::new(|v| v["provides"]["runtimes"] = json!(["demo", "x"])),
        ),
        (
            "programs",
            Box::new(|v| v["provides"]["programs"] = json!(["demo", "demo-build", "x"])),
        ),
        (
            "env_set",
            Box::new(|v| v["provides"]["env_set"] = json!([])),
        ),
        (
            "verb",
            Box::new(|v| v["provides"]["verbs"][0]["tier"] = json!("caution")),
        ),
        (
            "level",
            Box::new(|v| v["permissions"]["host"]["level"] = json!("admin")),
        ),
        (
            "network",
            Box::new(|v| v["permissions"]["network"]["mode"] = json!("outbound")),
        ),
        (
            "fs",
            Box::new(|v| v["permissions"]["fs"]["write"] = json!(["$STATE"])),
        ),
        (
            "spawn",
            Box::new(|v| v["permissions"]["spawn"]["mode"] = json!("any")),
        ),
        (
            "memory",
            Box::new(|v| v["permissions"]["memory_mb"] = json!(65)),
        ),
        (
            "enforcement",
            Box::new(|v| v["permissions"]["requires_enforcement"] = json!([])),
        ),
    ];
    let mut seen = vec![base.clone()];
    for (what, edit) in edits {
        let mut v = base_manifest();
        edit(&mut v);
        let f = fp(&v);
        assert!(!seen.contains(&f), "{what} did not change the fingerprint");
        seen.push(f);
    }
    // Text that is not part of what is granted does not matter.
    let mut v = base_manifest();
    v["summary"] = json!("A different sentence.");
    v["description"] = json!("Other words.");
    v["tags"] = json!(["other"]);
    assert_eq!(fp(&v), base);
}

#[test]
fn fingerprint_is_stable() {
    // Pins the serialisation: a change here means every consent ever given no longer matches.
    let m: Manifest = parse(&base_manifest());
    assert_eq!(m.fingerprint(SHA_A), "57411192fb4a8f31");
}

/// The permissions of the base manifest with `changes` (top-level keys) replaced.
fn perms(changes: Value) -> crate::Permissions {
    let mut base = serde_json::to_value(parse(&base_manifest()).permissions).unwrap();
    // the base manifest requires an enforced network and reads only its state: start from pure
    base["network"] = json!({"mode": "none"});
    base["spawn"] = json!({"mode": "none"});
    base["fs"] = json!({"read": ["$STATE"], "write": []});
    base["env_pass"] = json!([]);
    base["host"] = json!({"level": "read", "scope": "all"});
    for (k, v) in changes.as_object().unwrap() {
        base[k.as_str()] = v.clone();
    }
    serde_json::from_value(base).unwrap()
}

#[test]
fn pure_class_is_read_only_no_network_no_spawn_no_env_and_narrow_files() {
    let pure = perms(serde_json::json!({}));
    assert!(pure.is_pure_class());
    assert!(pure.broad_items().is_empty());

    for (change, why) in [
        (
            serde_json::json!({"host": {"level": "lifecycle", "scope": "all"}}),
            "level",
        ),
        (
            serde_json::json!({"network": {"mode": "outbound"}}),
            "network",
        ),
        (
            serde_json::json!({"spawn": {"mode": "named", "names": ["node"]}}),
            "spawn",
        ),
        (serde_json::json!({"env_pass": ["HOME"]}), "env_pass"),
        (
            serde_json::json!({"fs": {"read": ["any"], "write": ["$STATE"]}}),
            "fs.read any",
        ),
        (
            serde_json::json!({"fs": {"read": ["$STATE"], "write": ["/tmp/x"]}}),
            "fs.write outside the state",
        ),
    ] {
        assert!(
            !perms(change).is_pure_class(),
            "{why} leaves the pure class"
        );
    }
    // an explicit read list is still pure
    assert!(
        perms(serde_json::json!({"fs": {"read": ["$STATE", "/etc/hosts"], "write": []}}))
            .is_pure_class()
    );
}

#[test]
fn automatic_consent_requires_known_read_locations() {
    for path in ["/", "/home", "$CWD", "/etc"] {
        let p = perms(json!({"fs": {"read": [path], "write": ["$STATE"]}}));
        assert!(!p.is_pure_class(), "{path}");
    }
}

#[test]
fn unrestricted_writes_are_rejected() {
    let p = perms(json!({"fs": {"write": ["any"]}}));
    let error = p.validate().unwrap_err();
    assert!(error.detail.contains("fs.write"));
}

#[test]
fn broad_items_name_what_the_preview_marks() {
    let p = perms(serde_json::json!({
        "host": {"level": "admin", "scope": "all"},
        "network": {"mode": "outbound"},
        "spawn": {"mode": "any"},
        "fs": {"read": ["any"], "write": ["any"]},
        "env_pass": ["SSH_AUTH_SOCK", "GITHUB_TOKEN"]
    }));
    let broad = p.broad_items();
    for want in [
        "fs.read = any",
        "fs.write = any",
        "spawn = any",
        "network = outbound",
        "host.level = admin",
        "env_pass SSH_AUTH_SOCK",
        "env_pass GITHUB_TOKEN",
    ] {
        assert!(broad.iter().any(|b| b == want), "{want} in {broad:?}");
    }
}

#[test]
fn broad_items_include_nonautomatic_grants() {
    let p = perms(json!({
        "host": {"level": "lifecycle", "scope": "opted-in"},
        "spawn": {"mode": "named", "names": ["git", "npm"]},
        "fs": {"write": ["$STATE", "$CWD", "/var/cache/example"]}
    }));
    let broad = p.broad_items();
    for want in [
        "fs.write = $CWD",
        "fs.write = /var/cache/example",
        "spawn = allowlist",
        "host.level = lifecycle",
    ] {
        assert!(
            broad.iter().any(|b| b.starts_with(want)),
            "{want} in {broad:?}"
        );
    }
    assert!(!broad.iter().any(|b| b.contains("$STATE")), "{broad:?}");
}
