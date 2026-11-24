use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::codes;
use crate::index::Index;
use crate::manifest::Rule;
use crate::testutil::*;

const TARGET: &str = "x86_64-unknown-linux-gnu";

fn artifact(sha: &str, exe: &str) -> Value {
    json!({"url": "https://example.invalid/notifier.tar.gz", "size": 2200000, "sha256": sha, "exe_sha256": exe})
}

fn release(version: &str) -> Value {
    json!({
        "version": version, "released": "2026-11-20", "min_daemon": "0.4.0", "protocol": [1],
        "yanked": false, "yank_reason": null,
        "artifacts": {TARGET: artifact(SHA_A, SHA_B), "aarch64-apple-darwin": artifact(SHA_A, SHA_B)}
    })
}

fn base_index() -> Value {
    json!({
        "schema": 1, "sequence": 42,
        "generated": "2026-11-20T10:00:00Z", "expires": "2027-01-20T10:00:00Z",
        "revoked": [{"exe_sha256": SHA_C, "reason": "credentials leaked in version 0.1.0"}],
        "adapters": [{
            "name": "notifier", "kind": "service", "tags": ["notifier"],
            "summary": "Telegram, Discord, Slack and webhook notifications.",
            "description": "Sends a message when an app starts, stops, crashes or becomes unhealthy.",
            "releases": [release("0.1.0")]
        }]
    })
}

fn parse_index(v: &Value) -> Result<Index, crate::manifest::Invalid> {
    Index::parse(v.to_string().as_bytes())
}

fn daemon() -> semver::Version {
    semver::Version::parse(DAEMON).unwrap()
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn versions(i: &Index, target: &str) -> Vec<String> {
    i.offered("notifier", target, &daemon(), &[1])
        .iter()
        .map(|o| o.release.version.clone())
        .collect()
}

#[test]
fn index_sequence_and_expiry_parse() {
    let i = parse_index(&base_index()).unwrap();
    assert_eq!(i.sequence, 42);
    assert_eq!(i.generated, at("2026-11-20T10:00:00Z"));
    assert_eq!(i.expires, at("2027-01-20T10:00:00Z"));

    i.check_fresh(at("2026-12-01T00:00:00Z"), None).unwrap();
    i.check_fresh(at("2026-12-01T00:00:00Z"), Some(41)).unwrap();
    i.check_fresh(at("2026-12-01T00:00:00Z"), Some(42)).unwrap();
    // Expired: at the instant and after.
    for now in ["2027-01-20T10:00:00Z", "2027-06-01T00:00:00Z"] {
        let e = i.check_fresh(at(now), None).unwrap_err();
        assert_eq!(
            (e.rule, e.rule.error_code()),
            (Rule::IndexStale, codes::E_INDEX_STALE)
        );
    }
    // Older than the last one accepted under the same key.
    let e = i
        .check_fresh(at("2026-12-01T00:00:00Z"), Some(43))
        .unwrap_err();
    assert_eq!(e.rule, Rule::IndexStale);

    // Dates that are not dates, and an index that expires before it was made.
    for (key, val) in [("generated", "yesterday"), ("expires", "2027-01-20")] {
        let mut v = base_index();
        v[key] = json!(val);
        assert_eq!(parse_index(&v).unwrap_err().rule, Rule::Parse, "{key}");
    }
    let mut v = base_index();
    v["expires"] = json!("2026-11-20T10:00:00Z");
    assert_eq!(parse_index(&v).unwrap_err().rule, Rule::IndexDate);
    // Sequence is an unsigned integer.
    for s in [json!(-1), json!(1.5), json!("42")] {
        let mut v = base_index();
        v["sequence"] = s;
        assert_eq!(parse_index(&v).unwrap_err().rule, Rule::Parse);
    }
    let mut v = base_index();
    v["schema"] = json!(2);
    assert_eq!(parse_index(&v).unwrap_err().rule, Rule::IndexSchema);
}

#[test]
fn index_unknown_target_not_offered() {
    let i = parse_index(&base_index()).unwrap();
    assert_eq!(versions(&i, TARGET), ["0.1.0"]);
    assert_eq!(versions(&i, "aarch64-apple-darwin"), ["0.1.0"]);
    assert!(versions(&i, "riscv64gc-unknown-linux-gnu").is_empty());
    // Exact string match, nothing fuzzy.
    assert!(versions(&i, "x86_64-unknown-linux").is_empty());
    assert!(versions(&i, "X86_64-UNKNOWN-LINUX-GNU").is_empty());
    assert!(i
        .offered("no-such-adapter", TARGET, &daemon(), &[1])
        .is_empty());
    // A newer release without an artifact for the target falls back to an older one that has it.
    let mut v = base_index();
    let mut newer = release("0.2.0");
    newer["artifacts"] = json!({"aarch64-apple-darwin": artifact(SHA_A, SHA_B)});
    v["adapters"][0]["releases"] = json!([release("0.1.0"), newer]);
    let i = parse_index(&v).unwrap();
    assert_eq!(versions(&i, TARGET), ["0.1.0"]);
    assert_eq!(versions(&i, "aarch64-apple-darwin"), ["0.2.0", "0.1.0"]);
}

#[test]
fn index_limits_enforced() {
    let bad = |edit: &dyn Fn(&mut Value), want: Rule, what: &str| {
        let mut v = base_index();
        edit(&mut v);
        assert_eq!(parse_index(&v).unwrap_err().rule, want, "{what}");
    };
    // Size of the file.
    let mut v = base_index();
    v["pad"] = json!("p".repeat(2 * 1024 * 1024));
    assert_eq!(parse_index(&v).unwrap_err().rule, Rule::Size);
    // Counts.
    bad(
        &|v| {
            let a = v["adapters"][0].clone();
            let many: Vec<Value> = (0..501)
                .map(|i| {
                    let mut x = a.clone();
                    x["name"] = json!(format!("adapter-{}", i));
                    x
                })
                .collect();
            v["adapters"] = json!(many);
        },
        Rule::Limit,
        "501 adapters",
    );
    bad(
        &|v| {
            v["adapters"][0]["releases"] = json!((0..51)
                .map(|i| release(&format!("0.1.{i}")))
                .collect::<Vec<_>>())
        },
        Rule::Limit,
        "51 releases",
    );
    // 500 adapters and 50 releases are fine.
    let mut v = base_index();
    let a = v["adapters"][0].clone();
    v["adapters"] = json!((0..500)
        .map(|i| {
            let mut x = a.clone();
            x["name"] = json!(format!("adapter-{i}"));
            x
        })
        .collect::<Vec<_>>());
    parse_index(&v).unwrap();
    let mut v = base_index();
    v["adapters"][0]["releases"] = json!((0..50)
        .map(|j| release(&format!("0.1.{j}")))
        .collect::<Vec<_>>());
    parse_index(&v).unwrap();
    // Text.
    bad(
        &|v| v["adapters"][0]["summary"] = json!("s".repeat(121)),
        Rule::TextLength,
        "summary 121",
    );
    bad(
        &|v| v["adapters"][0]["description"] = json!("d".repeat(4097)),
        Rule::TextLength,
        "description 4097",
    );
    bad(
        &|v| v["adapters"][0]["summary"] = json!("a\u{1b}[2Kb"),
        Rule::ControlCharacters,
        "escape in summary",
    );
    bad(
        &|v| v["adapters"][0]["description"] = json!("a\u{9b}b"),
        Rule::ControlCharacters,
        "c1 in description",
    );
    bad(
        &|v| v["adapters"][0]["tags"] = json!(["x\ny"]),
        Rule::ControlCharacters,
        "newline in tag",
    );
    bad(
        &|v| v["revoked"][0]["reason"] = json!("r\u{1b}[0m"),
        Rule::ControlCharacters,
        "escape in revoked reason",
    );
    bad(
        &|v| v["revoked"][0]["exe_sha256"] = json!("nothex"),
        Rule::IndexHash,
        "revoked hash",
    );
    // Archives: https only, short, with real hashes and a size.
    let art = |v: &mut Value, key: &str, val: Value| {
        v["adapters"][0]["releases"][0]["artifacts"][TARGET][key] = val
    };
    for url in [
        "http://example.invalid/a.tar.gz",
        "ftp://x/a",
        "file:///etc/passwd",
        "https://",
        "https:///x",
        "//x/y",
        "https://a b/c",
        "https://a/\u{1b}x",
        &format!("https://example.invalid/{}", "a".repeat(500)),
    ] {
        bad(&|v| art(v, "url", json!(url)), Rule::IndexUrl, url);
    }
    let long_ok = format!(
        "https://example.invalid/{}",
        "a".repeat(512 - "https://example.invalid/".len())
    );
    assert_eq!(long_ok.len(), 512);
    let mut v = base_index();
    art(&mut v, "url", json!(long_ok));
    parse_index(&v).unwrap();
    bad(
        &|v| art(v, "url", json!(format!("{long_ok}a"))),
        Rule::IndexUrl,
        "url 513",
    );
    for h in ["", "AB12", &SHA_A.to_uppercase(), &SHA_A[..63]] {
        bad(&|v| art(v, "sha256", json!(h)), Rule::IndexHash, h);
        bad(&|v| art(v, "exe_sha256", json!(h)), Rule::IndexHash, h);
    }
    bad(&|v| art(v, "size", json!(0)), Rule::Limit, "size 0");
    bad(
        &|v| art(v, "size", json!(300 * 1024 * 1024)),
        Rule::Limit,
        "size over the archive cap",
    );
    // Names and versions.
    bad(
        &|v| v["adapters"][0]["name"] = json!("Bad_Name"),
        Rule::NameCharset,
        "name",
    );
    bad(
        &|v| v["adapters"][0]["releases"][0]["version"] = json!("one"),
        Rule::Version,
        "version",
    );
    bad(
        &|v| v["adapters"][0]["releases"][0]["min_daemon"] = json!("x"),
        Rule::MinDaemon,
        "min_daemon",
    );
    bad(
        &|v| v["adapters"][0]["releases"][0]["protocol"] = json!([]),
        Rule::Protocol,
        "protocol",
    );
    bad(
        &|v| v["adapters"][0]["kind"] = json!("daemon"),
        Rule::Parse,
        "kind",
    );
    bad(
        &|v| v["adapters"][0]["releases"] = json!([release("0.1.0"), release("0.1.0")]),
        Rule::IndexSchema,
        "same version twice",
    );
    bad(
        &|v| {
            let a = v["adapters"][0].clone();
            v["adapters"] = json!([a.clone(), a]);
        },
        Rule::IndexSchema,
        "same adapter twice",
    );
    bad(
        &|v| {
            v["adapters"][0]["releases"][0]["artifacts"] =
                json!({"bad target!": artifact(SHA_A, SHA_B)})
        },
        Rule::IndexSchema,
        "target",
    );
}

#[test]
fn index_yanked_release_not_offered() {
    let mut v = base_index();
    let mut yanked = release("0.2.0");
    yanked["yanked"] = json!(true);
    yanked["yank_reason"] = json!("crashes on start");
    v["adapters"][0]["releases"] = json!([release("0.1.0"), yanked]);
    let i = parse_index(&v).unwrap();
    // The yanked one is skipped for new installs; the older good one is offered.
    assert_eq!(versions(&i, TARGET), ["0.1.0"]);
    assert_eq!(
        i.latest_offer("notifier", TARGET, &daemon(), &[1])
            .unwrap()
            .release
            .version,
        "0.1.0"
    );
    // The reason is still readable for `list` and `info`.
    let r = i
        .adapter("notifier")
        .unwrap()
        .releases
        .iter()
        .find(|r| r.version == "0.2.0")
        .unwrap();
    assert_eq!(r.yank_reason.as_deref(), Some("crashes on start"));
    // All yanked: nothing is offered.
    v["adapters"][0]["releases"][0]["yanked"] = json!(true);
    let i = parse_index(&v).unwrap();
    assert!(versions(&i, TARGET).is_empty());
    assert!(i
        .latest_offer("notifier", TARGET, &daemon(), &[1])
        .is_none());
}

#[test]
fn index_revoked_list_parses() {
    let i = parse_index(&base_index()).unwrap();
    assert_eq!(i.revoked.len(), 1);
    assert!(i.is_revoked(SHA_C) && i.is_revoked(&SHA_C.to_uppercase()));
    assert!(!i.is_revoked(SHA_A));
    // No list at all is an empty list.
    let mut v = base_index();
    v.as_object_mut().unwrap().remove("revoked");
    assert!(parse_index(&v).unwrap().revoked.is_empty());
    // A release whose executable is revoked is not offered, even if it is the newest.
    let mut v = base_index();
    let mut bad = release("0.2.0");
    bad["artifacts"][TARGET] = artifact(SHA_A, SHA_C);
    v["adapters"][0]["releases"] = json!([release("0.1.0"), bad]);
    let i = parse_index(&v).unwrap();
    assert_eq!(versions(&i, TARGET), ["0.1.0"]);
}

#[test]
fn index_latest_is_computed_not_read() {
    // Listed out of order, with a lying `latest` field, and 0.10.0 above 0.9.0 (not as text).
    let mut v = base_index();
    v["adapters"][0]["latest"] = json!("0.0.1");
    v["adapters"][0]["releases"] = json!([
        release("0.9.0"),
        release("0.10.0"),
        release("0.2.0"),
        release("1.0.0-rc.1"),
        release("0.10.0-beta.2")
    ]);
    v["latest"] = json!("0.0.1");
    let i = parse_index(&v).unwrap();
    assert_eq!(
        versions(&i, TARGET),
        ["1.0.0-rc.1", "0.10.0", "0.10.0-beta.2", "0.9.0", "0.2.0"]
    );
    assert_eq!(
        i.latest_offer("notifier", TARGET, &daemon(), &[1])
            .unwrap()
            .release
            .version,
        "1.0.0-rc.1"
    );
}

#[test]
fn index_offer_respects_daemon_and_protocol() {
    let mut v = base_index();
    let mut newer = release("0.2.0");
    newer["min_daemon"] = json!("0.5.0");
    let mut proto2 = release("0.3.0");
    proto2["protocol"] = json!([2]);
    v["adapters"][0]["releases"] = json!([release("0.1.0"), newer, proto2]);
    let i = parse_index(&v).unwrap();
    assert_eq!(versions(&i, TARGET), ["0.1.0"]);
    assert_eq!(
        i.offered(
            "notifier",
            TARGET,
            &semver::Version::parse("0.5.0").unwrap(),
            &[1]
        )
        .iter()
        .map(|o| o.release.version.as_str())
        .collect::<Vec<_>>(),
        ["0.2.0", "0.1.0"]
    );
    assert_eq!(i.offered("notifier", TARGET, &daemon(), &[1, 2]).len(), 2);
}

#[test]
fn index_reads_only_the_newest_ten_releases() {
    let mut v = base_index();
    let mut releases: Vec<Value> = (1..=11).map(|n| release(&format!("0.{n}.0"))).collect();
    for r in releases.iter_mut().skip(1) {
        r["yanked"] = json!(true);
    }
    v["adapters"][0]["releases"] = json!(releases);
    let i = parse_index(&v).unwrap();
    // 0.1.0 is the only good one but it is the eleventh newest, so it is not looked at.
    assert!(versions(&i, TARGET).is_empty());
}
