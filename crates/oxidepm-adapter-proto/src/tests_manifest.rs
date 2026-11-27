use serde_json::{json, Value};

use crate::manifest::{Manifest, Rule, FIXED_RESERVED_NAMES};
use crate::testutil::*;

#[test]
fn manifest_valid_examples_pass() {
    let m = validate(&base_manifest()).unwrap();
    assert_eq!(m.name, "runtime-demo");
    assert_eq!(m.permissions.memory_mb, 64);
    validate(&service_manifest()).unwrap();
}

#[test]
fn manifest_defaults_apply() {
    let m = parse(&json!({
        "schema": 1, "name": "tiny", "version": "1.0.0", "kind": "tool", "summary": "s",
        "protocol": [1], "min_daemon": "0.1.0", "exec": "tiny",
        "permissions": {"host": {"level": "read", "scope": "all"}}
    }));
    assert_eq!(m.permissions.memory_mb, 128);
    assert!(m.permissions.events.is_empty() && m.provides.verbs.is_empty() && !m.ai_driven);
}

#[test]
fn manifest_name_charset() {
    for name in [
        "Bad",
        "1abc",
        "a",
        "ab cd",
        "ab_cd",
        "-ab",
        "ab.cd",
        "ünï",
        &"a".repeat(33),
        "",
    ] {
        let mut m = base_manifest();
        m["name"] = json!(name);
        assert_eq!(rule_of(&m), Rule::NameCharset, "{name:?}");
    }
    for name in ["ab", "a-b-c", "runtime2", &"a".repeat(32)] {
        let mut m = base_manifest();
        m["name"] = json!(name);
        validate(&m).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn manifest_reserved_names() {
    // The fixed list is refused for everyone.
    for name in FIXED_RESERVED_NAMES {
        let mut m = base_manifest();
        m["name"] = json!(name);
        assert_eq!(rule_of(&m), Rule::FixedReservedName, "{name}");
    }
    // The core's hint tables come in through the context.
    let mut c = ctx();
    c.reserved_adapters = &["runtime-node"];
    c.reserved_runtimes = &["node"];
    c.reserved_verbs = &["check"];
    let mut m = base_manifest();
    m["name"] = json!("runtime-node");
    assert_eq!(validate_with(&m, &c).unwrap_err().rule, Rule::ReservedName);
    let mut m = base_manifest();
    m["provides"]["runtimes"] = json!(["node"]);
    assert_eq!(validate_with(&m, &c).unwrap_err().rule, Rule::ReservedName);
    // The base manifest has a verb called check.
    assert_eq!(
        validate_with(&base_manifest(), &c).unwrap_err().rule,
        Rule::ReservedName
    );
    // A name that is not on the lists is fine.
    let mut m = base_manifest();
    m["provides"]["verbs"] = json!([]);
    validate_with(&m, &c).unwrap();
}

#[test]
fn manifest_reserved_names_need_a_signed_archive() {
    let mut m = base_manifest();
    m["name"] = json!("runtime-node");
    let mut c = ctx();
    c.reserved_adapters = &["runtime-node"];
    assert_eq!(validate_with(&m, &c).unwrap_err().rule, Rule::ReservedName);
    c.signed_by_embedded_key = true;
    validate_with(&m, &c).unwrap();
    c.signed_by_embedded_key = false;
    c.allow_reserved_name = true;
    validate_with(&m, &c).unwrap();
    // Neither a signature nor the flag lifts the fixed list.
    let mut core = base_manifest();
    core["name"] = json!("cmd");
    c.signed_by_embedded_key = true;
    assert_eq!(
        validate_with(&core, &c).unwrap_err().rule,
        Rule::FixedReservedName
    );
}

#[test]
fn manifest_kind_values() {
    for kind in ["daemon", "Service", "TOOL", "", "ui", "runtime"] {
        let mut m = base_manifest();
        m["kind"] = json!(kind);
        let e = Manifest::parse(m.to_string().as_bytes()).unwrap_err();
        assert_eq!(e.rule, Rule::Parse, "{kind:?}");
    }
    let mut m = base_manifest();
    m["kind"] = json!(7);
    assert_eq!(
        Manifest::parse(m.to_string().as_bytes()).unwrap_err().rule,
        Rule::Parse
    );
    // Missing kind is a parse error too.
    let mut m = base_manifest();
    m.as_object_mut().unwrap().remove("kind");
    assert_eq!(
        Manifest::parse(m.to_string().as_bytes()).unwrap_err().rule,
        Rule::Parse
    );
}

#[test]
fn manifest_schema_version() {
    for s in [0, 2, 99] {
        let mut m = base_manifest();
        m["schema"] = json!(s);
        assert_eq!(rule_of(&m), Rule::Schema);
    }
}

#[test]
fn manifest_version_protocol_and_min_daemon() {
    for v in ["1", "1.0", "v1.0.0", "1.0.0.0", "latest", ""] {
        let mut m = base_manifest();
        m["version"] = json!(v);
        assert_eq!(rule_of(&m), Rule::Version, "{v:?}");
    }
    let mut m = base_manifest();
    m["version"] = json!("1.2.3-rc.1+build5");
    validate(&m).unwrap();

    let mut m = base_manifest();
    m["protocol"] = json!([]);
    assert_eq!(rule_of(&m), Rule::Protocol);
    m["protocol"] = json!([2, 3]);
    assert_eq!(rule_of(&m), Rule::Protocol);
    m["protocol"] = json!([2, 1]);
    validate(&m).unwrap();

    let mut m = base_manifest();
    m["min_daemon"] = json!("0.4.1");
    assert_eq!(rule_of(&m), Rule::MinDaemon);
    m["min_daemon"] = json!("nope");
    assert_eq!(rule_of(&m), Rule::MinDaemon);
    m["min_daemon"] = json!("0.3.0");
    validate(&m).unwrap();
}

#[test]
fn manifest_exec_must_be_relative() {
    for exec in [
        "/bin/sh", "../x", "a/../b", "./x", "", "a//b", "dir/", "a/./b", "x\u{1b}y", "a\0b",
    ] {
        let mut m = base_manifest();
        m["exec"] = json!(exec);
        assert_eq!(rule_of(&m), Rule::ExecRelative, "{exec:?}");
    }
    for exec in ["tool", "bin/tool", "a/b/c-d_e.bin"] {
        let mut m = base_manifest();
        m["exec"] = json!(exec);
        validate(&m).unwrap_or_else(|e| panic!("{exec}: {e}"));
    }
}

#[test]
fn manifest_exec_must_be_elf() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("bin")).unwrap();
    let mut elf = b"\x7fELF".to_vec();
    elf.extend_from_slice(&[2, 1, 1, 0]);
    std::fs::write(root.join("bin/good"), &elf).unwrap();
    std::fs::write(root.join("bin/script"), b"#!/bin/sh\necho hi\n").unwrap();
    std::fs::write(root.join("bin/short"), b"\x7fE").unwrap();
    std::fs::write(root.join("bin/empty"), b"").unwrap();
    symlink("good", root.join("bin/link")).unwrap();
    symlink("bin", root.join("linkdir")).unwrap();

    let with_exec = |exec: &str| {
        let mut v = base_manifest();
        v["exec"] = json!(exec);
        parse(&v)
    };
    with_exec("bin/good").check_exec_file(root).unwrap();
    for (exec, want) in [
        ("bin/script", Rule::ExecNotNative),
        ("bin/short", Rule::ExecNotNative),
        ("bin/empty", Rule::ExecNotNative),
        ("bin/missing", Rule::ExecFile),
        ("bin", Rule::ExecFile),
        ("bin/link", Rule::ExecFile),
        ("linkdir/good", Rule::ExecFile),
        ("../outside", Rule::ExecRelative),
        ("/bin/sh", Rule::ExecRelative),
    ] {
        assert_eq!(
            with_exec(exec).check_exec_file(root).unwrap_err().rule,
            want,
            "{exec}"
        );
    }
}

fn verb(name: &str) -> Value {
    json!({"name": name, "summary": "s", "tier": "caution", "writes": true, "dry_run": true, "max_runtime_secs": 60})
}

#[test]
fn manifest_verb_name_rules() {
    for name in [
        "Check",
        "c",
        "1check",
        "check_it",
        "ch eck",
        "-x",
        &"a".repeat(25),
        "",
    ] {
        let mut m = base_manifest();
        m["provides"]["verbs"] = json!([verb(name)]);
        assert_eq!(rule_of(&m), Rule::VerbName, "{name:?}");
    }
    for name in ["ab", "git-start", &"a".repeat(24)] {
        let mut m = base_manifest();
        m["provides"]["verbs"] = json!([verb(name)]);
        validate(&m).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    // Twice in one manifest.
    let mut m = base_manifest();
    m["provides"]["verbs"] = json!([verb("deploy"), verb("deploy")]);
    assert_eq!(rule_of(&m), Rule::VerbName);
    // A built-in subcommand, and a verb of another installed adapter, but not the adapter's own.
    let mut m = base_manifest();
    m["provides"]["verbs"] = json!([verb("status")]);
    let mut c = ctx();
    c.builtin_verbs = &["status", "start"];
    assert_eq!(validate_with(&m, &c).unwrap_err().rule, Rule::VerbCollision);
    let mut m = base_manifest();
    m["provides"]["verbs"] = json!([verb("deploy")]);
    let installed = [("other-adapter", "deploy")];
    let mut c = ctx();
    c.installed_verbs = &installed;
    let e = validate_with(&m, &c).unwrap_err();
    assert_eq!(e.rule, Rule::VerbCollision);
    assert!(
        e.detail.contains("other-adapter") && e.detail.contains("deploy"),
        "{e}"
    );
    let own = [("runtime-demo", "deploy")];
    c.installed_verbs = &own;
    validate_with(&m, &c).unwrap();
    // The runtime limit of a verb.
    for secs in [0, 3601] {
        let mut m = base_manifest();
        let mut v = verb("deploy");
        v["max_runtime_secs"] = json!(secs);
        m["provides"]["verbs"] = json!([v]);
        assert_eq!(rule_of(&m), Rule::VerbRuntime, "{secs}");
    }
}

#[test]
fn manifest_runtime_name_rules() {
    for name in [
        "cmd",
        "Node",
        "n",
        "no-de",
        "no_de",
        "1node",
        &"n".repeat(17),
        "",
    ] {
        let mut m = base_manifest();
        m["provides"]["runtimes"] = json!([name]);
        assert_eq!(rule_of(&m), Rule::RuntimeName, "{name:?}");
    }
    let mut m = base_manifest();
    m["provides"]["runtimes"] = json!(["node", "node"]);
    assert_eq!(rule_of(&m), Rule::RuntimeName);
    let mut m = base_manifest();
    m["provides"]["runtimes"] = json!(["node", "npm", "n2", "a".repeat(16)]);
    validate(&m).unwrap();
}

#[test]
fn manifest_writes_requires_caution_tier() {
    let mut m = base_manifest();
    let mut v = verb("fixit");
    v["tier"] = json!("safe");
    m["provides"]["verbs"] = json!([v.clone()]);
    assert_eq!(rule_of(&m), Rule::VerbWritesTier);
    for tier in ["caution", "dangerous"] {
        v["tier"] = json!(tier);
        m["provides"]["verbs"] = json!([v.clone()]);
        validate(&m).unwrap();
    }
    // A safe verb that does not write is fine.
    v["tier"] = json!("safe");
    v["writes"] = json!(false);
    m["provides"]["verbs"] = json!([v]);
    validate(&m).unwrap();
}

#[test]
fn manifest_spawning_adapter_may_not_declare_safe_verb() {
    let safe_verb = json!({"name": "look", "summary": "s", "tier": "safe"});
    for (key, value) in [
        ("spawn", json!({"mode": "named", "names": ["git"]})),
        ("spawn", json!({"mode": "any"})),
        ("network", json!({"mode": "outbound"})),
    ] {
        let mut m = base_manifest();
        m["provides"]["verbs"] = json!([safe_verb.clone()]);
        m["permissions"][key] = value;
        assert_eq!(rule_of(&m), Rule::VerbSafeButSpawning, "{key}");
        // The same verb at caution is allowed.
        let mut v = safe_verb.clone();
        v["tier"] = json!("caution");
        m["provides"]["verbs"] = json!([v]);
        validate(&m).unwrap();
    }
    let mut m = base_manifest();
    m["provides"]["verbs"] = json!([safe_verb]);
    validate(&m).unwrap();
}

#[test]
fn manifest_runtime_must_list_programs() {
    let mut m = base_manifest();
    m["provides"]["programs"] = json!([]);
    m["provides"]["capture_programs"] = json!([]);
    assert_eq!(rule_of(&m), Rule::RuntimeNeedsPrograms);
    // Bare file names only.
    for p in [
        "/usr/bin/node",
        "bin/node",
        "../node",
        "-n",
        ".hidden",
        "no de",
        "",
    ] {
        let mut m = base_manifest();
        m["provides"]["programs"] = json!([p]);
        m["provides"]["capture_programs"] = json!([]);
        assert_eq!(rule_of(&m), Rule::ProgramName, "{p:?}");
    }
    // capture_programs is a subset of programs.
    let mut m = base_manifest();
    m["provides"]["capture_programs"] = json!(["not-listed"]);
    assert_eq!(rule_of(&m), Rule::ProgramName);
    // An adapter with no runtime needs no programs.
    let mut m = base_manifest();
    m["provides"]["runtimes"] = json!([]);
    m["provides"]["programs"] = json!([]);
    m["provides"]["capture_programs"] = json!([]);
    validate(&m).unwrap();
}

#[test]
fn manifest_env_set_rejects_denied_keys() {
    for key in [
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "ld_preload",
        "DYLD_INSERT_LIBRARIES",
        "PATH",
        "Path",
        "NODE_OPTIONS",
        "BASH_ENV",
        "ENV",
        "PYTHONSTARTUP",
        "PYTHONHOME",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_EXTERNAL_DIFF",
        "npm_config_script_shell",
        "NPM_CONFIG_PREFIX",
    ] {
        let mut m = base_manifest();
        m["provides"]["env_set"] = json!([key]);
        assert_eq!(rule_of(&m), Rule::EnvSet, "{key}");
    }
    for key in ["1BAD", "has space", "A=B", "", "a-b"] {
        let mut m = base_manifest();
        m["provides"]["env_set"] = json!([key]);
        assert_eq!(rule_of(&m), Rule::EnvSet, "{key:?}");
    }
    let mut m = base_manifest();
    m["provides"]["env_set"] = json!(["DEMO_MODE", "NODE_ENV", "RUST_LOG"]);
    validate(&m).unwrap();
    assert!(crate::text::env_key_denied("LD_PRELOAD") && !crate::text::env_key_denied("NODE_ENV"));
}

#[test]
fn manifest_control_characters_are_refused() {
    let bad = [
        "bad\u{1b}[2Jtext",
        "tab\there",
        "nul\u{0}x",
        "c1\u{9b}x",
        "del\u{7f}",
        "bidi\u{202e}x",
        "cr\rx",
        "nl\nx",
    ];
    for s in bad {
        let mut m = base_manifest();
        m["summary"] = json!(s);
        assert_eq!(rule_of(&m), Rule::ControlCharacters, "summary {s:?}");
    }
    // A description may hold newlines but nothing else.
    let mut m = base_manifest();
    m["description"] = json!("two\nlines");
    validate(&m).unwrap();
    for s in &bad[..7] {
        let mut m = base_manifest();
        m["description"] = json!(s);
        assert_eq!(rule_of(&m), Rule::ControlCharacters, "description {s:?}");
    }
    let mut m = base_manifest();
    m["tags"] = json!(["ok", "e\u{1b}sc"]);
    assert_eq!(rule_of(&m), Rule::ControlCharacters);
    let mut m = base_manifest();
    let mut v = verb("deploy");
    v["summary"] = json!("go\u{1b}[31m");
    m["provides"]["verbs"] = json!([v]);
    assert_eq!(rule_of(&m), Rule::ControlCharacters);
    let mut m = base_manifest();
    let mut v = verb("deploy");
    v["args_summary"] = json!("<a>\u{1b}");
    m["provides"]["verbs"] = json!([v]);
    assert_eq!(rule_of(&m), Rule::ControlCharacters);
}

#[test]
fn manifest_text_lengths() {
    let mut m = base_manifest();
    m["summary"] = json!("s".repeat(121));
    assert_eq!(rule_of(&m), Rule::TextLength);
    m["summary"] = json!("s".repeat(120));
    validate(&m).unwrap();
    m["summary"] = json!("  ");
    assert_eq!(rule_of(&m), Rule::TextLength);
    let mut m = base_manifest();
    m["description"] = json!("d".repeat(4097));
    assert_eq!(rule_of(&m), Rule::TextLength);
    let mut m = base_manifest();
    m["tags"] = json!((0..11).map(|i| format!("t{i}")).collect::<Vec<_>>());
    assert_eq!(rule_of(&m), Rule::Limit);
}

#[test]
fn manifest_tool_with_events_invalid() {
    let mut m = base_manifest();
    m["permissions"]["events"] = json!(["lifecycle"]);
    assert_eq!(rule_of(&m), Rule::KindMismatch);
    // The same events on a service are fine, but only the known families.
    let mut s = service_manifest();
    validate(&s).unwrap();
    s["permissions"]["events"] = json!(["lifecycle", "logs"]);
    assert_eq!(rule_of(&s), Rule::Permissions);
}

#[test]
fn manifest_service_with_runtimes_invalid() {
    let mut m = service_manifest();
    m["provides"]["runtimes"] = json!(["demo"]);
    m["provides"]["programs"] = json!(["demo"]);
    assert_eq!(rule_of(&m), Rule::KindMismatch);
}

#[test]
fn manifest_size_cap() {
    // Padded with an unknown field to exactly the cap, then one byte more.
    let base = base_manifest().to_string();
    let pad_to = |total: usize| {
        let mut v = base_manifest();
        v["pad"] = json!("");
        let empty = v.to_string().len();
        v["pad"] = json!("p".repeat(total - empty));
        v.to_string().into_bytes()
    };
    assert!(base.len() < 16 * 1024);
    let at_cap = pad_to(16 * 1024);
    assert_eq!(at_cap.len(), 16 * 1024);
    Manifest::parse(&at_cap).unwrap();
    let over = pad_to(16 * 1024 + 1);
    assert_eq!(Manifest::parse(&over).unwrap_err().rule, Rule::Size);
    assert_eq!(
        Manifest::parse_validated(&over, &ctx()).unwrap_err().rule,
        Rule::Size
    );
}

#[test]
fn manifest_permission_rules() {
    let cases: Vec<(&str, Value)> = vec![
        ("memory_mb", json!(0)),
        ("memory_mb", json!(513)),
        ("env_pass", json!(["OK", "bad name"])),
        (
            "env_pass",
            json!((0..33).map(|i| format!("V{i}")).collect::<Vec<_>>()),
        ),
    ];
    for (key, value) in cases {
        let mut m = base_manifest();
        m["permissions"][key] = value.clone();
        assert_eq!(rule_of(&m), Rule::Permissions, "{key} {value}");
    }
    for (path, value) in [
        ("fs.read", json!(["relative/path"])),
        ("fs.read", json!(["/etc/../shadow"])),
        ("fs.write", json!(["$HOME"])),
        ("fs.write", json!(["/a\u{1b}b"])),
    ] {
        let mut m = base_manifest();
        let (a, b) = path.split_once('.').unwrap();
        m["permissions"][a][b] = value;
        assert_eq!(rule_of(&m), Rule::Permissions, "{path}");
    }
    let mut m = base_manifest();
    m["permissions"]["network"] = json!({"mode": "outbound", "listen": ["not-an-address"]});
    assert_eq!(rule_of(&m), Rule::Permissions);
    let mut m = base_manifest();
    m["permissions"]["host"]["max_in_flight"] = json!(129);
    assert_eq!(rule_of(&m), Rule::Permissions);
    let mut m = base_manifest();
    m["permissions"]["spawn"] = json!({"mode": "named", "names": ["git", "/bin/sh"]});
    assert_eq!(rule_of(&m), Rule::Permissions);
    // A well-formed wide set is accepted (consent is a different layer).
    let mut m = base_manifest();
    m["provides"]["verbs"] = json!([]);
    m["permissions"] = json!({
        "host": {"level": "admin", "scope": "opted-in", "max_in_flight": 128},
        "events": ["lifecycle", "adapter"], "env_pass": ["SSH_AUTH_SOCK"],
        "network": {"mode": "outbound", "hosts": ["api.telegram.org"], "listen": ["127.0.0.1:9615"]},
        "fs": {"read": ["any"], "write": ["$STATE", "$CWD", "/var/tmp/x"]},
        "spawn": {"mode": "named", "names": ["git", "ssh"]},
        "memory_mb": 512, "requires_enforcement": ["network", "fs_write", "fs_read", "spawn"]
    });
    m["kind"] = json!("service");
    m["provides"]["runtimes"] = json!([]);
    validate(&m).unwrap();
    // requires_enforcement is a subset of the four items.
    m["permissions"]["requires_enforcement"] = json!(["memory"]);
    assert_eq!(
        Manifest::parse(m.to_string().as_bytes()).unwrap_err().rule,
        Rule::Parse
    );
}

#[test]
fn manifest_socket_permissions_require_passed_environment_names() {
    let mut m = base_manifest();
    m["provides"]["verbs"] = json!([]);
    m["permissions"]["env_pass"] = json!(["SSH_AUTH_SOCK"]);
    for names in [
        json!(["SSH_AGENT"]),
        json!(["bad name"]),
        json!(vec!["SSH_AUTH_SOCK"; 33]),
    ] {
        m["permissions"]["connect_env_socket"] = names;
        assert_eq!(rule_of(&m), Rule::Permissions);
    }
    m["permissions"]["connect_env_socket"] = json!(["SSH_AUTH_SOCK"]);
    validate(&m).unwrap();

    m["provides"]["verbs"] = json!([{ "name": "look", "summary": "Look", "tier": "safe" }]);
    assert_eq!(rule_of(&m), Rule::VerbSafeButSpawning);
}

#[test]
fn rule_error_codes() {
    assert_eq!(Rule::NameCharset.error_code(), "E_ADAPTER_INVALID");
    assert_eq!(Rule::IndexStale.error_code(), "E_INDEX_STALE");
}
