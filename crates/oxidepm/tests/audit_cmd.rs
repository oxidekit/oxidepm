//! `oxidepm audit`: reads the audit files directly (no daemon), filters, and prints nothing an
//! actor wrote to the terminal unsanitized.

mod common;

use common::TestHome;
use serde_json::{json, Value};

fn decision(seq: u64, ts: &str, req: &str, actor: (&str, &str), op: &str, verdict: &str) -> Value {
    let mut line = json!({
        "v": 1, "seq": seq, "ts": ts, "phase": "decision", "req": req,
        "actor": { "class": actor.0, "label": actor.1, "via": "main" },
        "op": op, "tier": "caution", "selector": "web", "decision": verdict, "dry_run": false,
    });
    if verdict == "deny" {
        line["deny"] = json!({ "code": "E_POLICY_DENIED", "why": "nope" });
    }
    line
}

fn result(seq: u64, ts: &str, req: &str) -> Value {
    json!({ "v": 1, "seq": seq, "ts": ts, "phase": "result", "req": req, "ok": true, "changed": 1, "failed": 0, "ms": 7 })
}

fn write_lines(home: &TestHome, file: &str, lines: &[Value]) {
    let dir = home.oxidepm_dir().join("audit");
    std::fs::create_dir_all(&dir).unwrap();
    let mut text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    text.push_str("{\"v\":1,\"seq\":99,\"ts\":\"torn"); // a torn last line
    std::fs::write(dir.join(file), text).unwrap();
}

fn sample(home: &TestHome) {
    write_lines(
        home,
        "audit.jsonl",
        &[
            decision(
                1,
                "2026-11-02T09:00:00.000Z",
                "r1",
                ("ai", "example-agent"),
                "restart",
                "allow",
            ),
            result(2, "2026-11-02T09:00:00.100Z", "r1"),
            decision(
                3,
                "2026-11-02T09:01:00.000Z",
                "r2",
                ("ai", "other-agent"),
                "stop",
                "deny",
            ),
            decision(
                4,
                "2026-11-02T09:02:00.000Z",
                "r3",
                ("human", "cli"),
                "delete",
                "allow",
            ),
            result(5, "2026-11-02T09:02:00.100Z", "r3"),
            decision(
                6,
                "2026-11-02T09:03:00.000Z",
                "r4",
                ("ai", "example-agent"),
                "stop",
                "allow",
            ),
        ],
    );
}

fn run(home: &TestHome, args: &[&str]) -> String {
    let out = home.cmd().args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn audit_tail_filters_by_actor_and_op() {
    let home = TestHome::new();
    sample(&home);

    let all = run(&home, &["audit"]);
    assert_eq!(
        all.lines().count(),
        6,
        "every whole line, the torn one skipped:\n{all}"
    );

    let example_agent = run(&home, &["audit", "--actor", "ai:example-agent"]);
    assert!(
        example_agent.contains("ai:example-agent restart")
            && example_agent.contains("ai:example-agent stop"),
        "{example_agent}"
    );
    assert!(
        !example_agent.contains("other-agent") && !example_agent.contains("human:cli"),
        "{example_agent}"
    );
    assert!(
        example_agent.contains("result r1 ok"),
        "the result of a request that matched is shown: {example_agent}"
    );

    // a class, and a bare label
    assert_eq!(
        run(&home, &["audit", "--actor", "ai"])
            .lines()
            .filter(|l| l.contains("ai:"))
            .count(),
        3
    );
    assert!(run(&home, &["audit", "--actor", "cli"]).contains("delete"));

    let stops = run(&home, &["audit", "--op", "stop"]);
    assert_eq!(stops.lines().count(), 2, "{stops}");
    assert!(stops.contains("DENIED E_POLICY_DENIED: nope"), "{stops}");
    let both = run(
        &home,
        &["audit", "--op", "stop", "--actor", "ai:example-agent"],
    );
    assert_eq!(both.lines().count(), 1, "{both}");

    // the newest N
    let tail = run(&home, &["audit", "--tail", "2"]);
    assert_eq!(tail.lines().count(), 2);
    assert!(tail.contains("#5") && tail.contains("#6"), "{tail}");

    // a time
    let since = run(&home, &["audit", "--since", "2026-11-02T09:02:30Z"]);
    assert_eq!(since.lines().count(), 1, "{since}");
    let out = home
        .cmd()
        .args(["audit", "--since", "yesterday-ish"])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "a --since that means nothing is refused"
    );
}

#[test]
fn audit_security_flag_reads_the_second_file() {
    let home = TestHome::new();
    sample(&home);
    write_lines(
        &home,
        "audit-security.jsonl",
        &[decision(
            1,
            "2026-11-02T09:01:00.000Z",
            "r2",
            ("ai", "other-agent"),
            "stop",
            "deny",
        )],
    );
    let normal = run(&home, &["audit", "--op", "stop"]);
    let security = run(&home, &["audit", "--security"]);
    assert_eq!(security.lines().count(), 1, "{security}");
    assert!(
        security.contains("ai:other-agent") && security.contains("DENIED"),
        "{security}"
    );
    assert!(normal.lines().count() > security.lines().count());
    // rotated files of the series are read too, oldest first
    let dir = home.oxidepm_dir().join("audit");
    std::fs::write(
        dir.join("audit-security.1.jsonl"),
        format!(
            "{}\n",
            decision(
                0,
                "2026-11-01T09:00:00.000Z",
                "r0",
                ("ai", "old"),
                "delete",
                "deny"
            )
        ),
    )
    .unwrap();
    let security = run(&home, &["audit", "--security"]);
    let old = security.find("ai:old").expect("the older file is read");
    assert!(
        old < security.find("ai:other-agent").unwrap(),
        "oldest first:\n{security}"
    );
}

#[test]
fn audit_works_with_daemon_down() {
    let home = TestHome::new();
    sample(&home);
    let out = run(&home, &["audit", "--tail", "1"]);
    assert!(out.contains("ai:example-agent stop"), "{out}");
    assert!(!home.socket().exists(), "no daemon was started for it");
    // no log yet: a sentence, not an error
    let empty = TestHome::new();
    let out = empty.cmd().arg("audit").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("No audit records"));
    assert!(!empty.socket().exists());
}

#[test]
fn audit_json_output_is_one_object_per_line() {
    let home = TestHome::new();
    sample(&home);
    let out = run(&home, &["--json", "audit", "--actor", "ai:example-agent"]);
    let parsed: Vec<Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON object per line"))
        .collect();
    assert_eq!(parsed.len(), 3, "{out}");
    assert!(parsed.iter().all(Value::is_object));
    assert_eq!(parsed[0]["seq"], 1);
    assert_eq!(parsed[0]["actor"]["label"], "example-agent");
    // nothing but the objects on stdout
    assert!(!out.contains("No audit") && !out.contains('\u{1b}'));
}

#[test]
fn audit_tail_strips_escape_sequences() {
    let home = TestHome::new();
    let mut line = decision(
        1,
        "2026-11-02T09:00:00.000Z",
        "r1",
        ("ai", "evil"),
        "stop",
        "deny",
    );
    // cursor movement and erase-line in every string an actor can influence
    line["actor"]["label"] = json!("ev\u{1b}[2Kil\u{9b}31m");
    line["selector"] = json!("web\u{1b}[1A\u{1b}[2K");
    line["deny"]["why"] = json!("nope\rALLOWED\u{7}\u{8}\u{7f}");
    line["targets"] = json!([{ "id": 1, "name": "x\u{1b}]0;title\u{7}" }]);
    write_lines(&home, "audit.jsonl", &[line]);
    let out = run(&home, &["audit"]);
    for bad in ['\u{1b}', '\u{9b}', '\r', '\u{7}', '\u{8}', '\u{7f}'] {
        assert!(!out.contains(bad), "{bad:?} reached the terminal: {out:?}");
    }
    assert!(
        out.contains("ev\u{FFFD}[2Kil\u{FFFD}31m"),
        "replaced, not removed: {out:?}"
    );
    assert_eq!(out.lines().count(), 1, "one line: {out:?}");
}
