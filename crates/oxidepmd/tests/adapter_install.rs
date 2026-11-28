//! Test adapter catalog verification, installation, consent, updates and removal.
//! A local server serves archives and a catalog signed with throwaway keys.
//! The tests use the real CLI and daemon in a temporary home.

mod common;

use common::adapter_kit::*;
use serde_json::json;

fn alpha() -> Packaged {
    Pkg::tool("alpha", "0.1.0").package()
}

fn kit_with(pkgs: &[&Packaged]) -> Kit {
    let kit = Kit::new();
    kit.publish(pkgs, &IndexOpts::default());
    kit
}

fn sign_bundle(kit: &Kit) {
    let checksums = std::fs::read(kit.bundle.join("SHA256SUMS")).unwrap();
    let comment = oxidepm_adapter_proto::signing::bundle_trusted_comment(
        concat!("v", env!("CARGO_PKG_VERSION")),
        &target(),
    );
    std::fs::write(
        kit.bundle.join("SHA256SUMS.minisig"),
        kit.key.sign(&checksums, &comment),
    )
    .unwrap();
}

fn enable_with_fingerprint(kit: &Kit, pkg: &Packaged) -> Run {
    kit.run(&[
        "adapter",
        "enable",
        &pkg.name,
        "--accept",
        &pkg.fingerprint(),
    ])
}

fn no_adapter_files(kit: &Kit) {
    assert!(
        kit.adapters_tree().is_empty(),
        "adapter files were left behind: {:?}",
        kit.adapters_tree()
    );
    assert!(!kit.home.join("adapters.db").exists());
}

// ---- the index -------------------------------------------------------------------------------

#[test]
fn available_listing_search_and_info_read_the_signed_index() {
    let a = alpha();
    let b = Pkg::service("beta", "2.0.0").package();
    let kit = kit_with(&[&a, &b]);

    let list = kit.run(&["adapter", "list", "--available"]);
    assert_eq!(list.code, 0, "{}", list.all());
    assert!(
        list.stdout.contains("alpha") && list.stdout.contains("beta"),
        "{}",
        list.stdout
    );
    assert!(
        list.stdout.contains("0.1.0") && list.stdout.contains("2.0.0"),
        "{}",
        list.stdout
    );
    assert!(
        list.stdout.contains(&kit.key.key_id()),
        "the key that signed is named: {}",
        list.stdout
    );
    assert!(list.stdout.contains("sequence 42"), "{}", list.stdout);

    let found = kit.run(&["adapter", "search", "BET"]);
    assert_eq!(found.code, 0, "{}", found.all());
    assert!(
        found.stdout.contains("beta") && !found.stdout.contains("alpha"),
        "{}",
        found.stdout
    );
    let none = kit.run(&["adapter", "search", "zzz"]);
    assert!(none.stdout.contains("No adapters match"), "{}", none.stdout);

    let info = kit.run(&["adapter", "info", "alpha"]);
    assert_eq!(info.code, 0, "{}", info.all());
    assert!(info.stdout.contains("alpha (tool)"), "{}", info.stdout);
    assert!(
        info.stdout.contains(&kit.key.key_id()) && info.stdout.contains("official"),
        "trust path and key id: {}",
        info.stdout
    );
    assert!(
        info.stdout.contains("can be installed here"),
        "{}",
        info.stdout
    );
    let missing = kit.run(&["adapter", "info", "nope"]);
    assert_eq!(missing.code, 1);
    assert!(
        missing.stderr.contains("E_ADAPTER_MISSING"),
        "{}",
        missing.stderr
    );

    let json = kit.run(&["--json", "adapter", "list", "--available"]);
    let value: serde_json::Value =
        serde_json::from_str(&json.stdout).unwrap_or_else(|e| panic!("{e}: {}", json.stdout));
    assert_eq!(value["sequence"], 42);
    assert_eq!(value["adapters"].as_array().unwrap().len(), 2);
    no_adapter_files(&kit);
}

#[test]
fn search_requires_the_daemon_when_index_history_exists() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    let listing = kit.run(&["adapter", "list", "--available"]);
    assert_eq!(listing.code, 0, "{}", listing.all());
    stop(daemon);

    for args in [
        &["adapter", "search", "alpha"][..],
        &["--dry-run", "adapter", "import", "alpha"][..],
    ] {
        let hits = kit.http.hits();
        let run = kit.run(args);
        assert_eq!(run.code, 1, "{}", run.all());
        assert!(run.stdout.is_empty(), "{}", run.all());
        assert!(run.stderr.contains("oxidepm resurrect"), "{}", run.all());
        assert_eq!(kit.http.hits() - hits, 2, "only the index and signature");
    }

    let daemon = kit.daemon();
    kit.publish(&[&a], &seq(41));
    let hits = kit.http.hits();
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stdout.is_empty(), "{}", run.all());
    assert!(run.stderr.contains("E_INDEX_STALE"), "{}", run.all());
    assert_eq!(kit.http.hits() - hits, 2, "only the index and signature");

    kit.publish(&[&a], &IndexOpts::default());
    let hits = kit.http.hits();
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("adapter list --available"),
        "{}",
        run.all()
    );
    assert_eq!(kit.http.hits() - hits, 2, "only the index and signature");

    let listing = kit.run(&["adapter", "list", "--available"]);
    assert_eq!(listing.code, 0, "{}", listing.all());
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 0, "{}", run.all());
    stop(daemon);
}

#[test]
fn without_an_index_the_commands_say_so_and_pinned_import_still_works() {
    let a = alpha();
    let kit = Kit::new();
    let mut cmd = kit.cli();
    cmd.env("OXIDEPM_TEST_INDEX_URL", "")
        .args(["adapter", "list", "--available"]);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(text.contains("no index is configured"), "{text}");

    let file = kit.dir.path().join("alpha.tar.gz");
    std::fs::write(&file, &a.archive).unwrap();
    let ok = kit.run(&[
        "--dry-run",
        "adapter",
        "import",
        file.to_str().unwrap(),
        "--sha256",
        &a.archive_sha,
    ]);
    assert_eq!(ok.code, 0, "{}", ok.all());
}

#[test]
fn import_with_tampered_index_is_refused() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let mut body = kit.index_json(&[&a], &IndexOpts::default()).to_string();
    body = body.replace("Made for the install tests.", "Made for the install tests!");
    kit.http.route("/index.json", body.into_bytes());
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_SIGNATURE_INVALID"), "{}", run.stderr);
    no_adapter_files(&kit);
}

#[test]
fn import_with_mixed_index_generations_is_refused() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let old_body = kit.http_index_body();
    let old_sig = kit.http_index_sig();
    kit.publish(&[&a], &seq(43));
    let new_body = kit.http_index_body();
    let new_sig = kit.http_index_sig();

    for (body, sig) in [(new_body, old_sig), (old_body, new_sig)] {
        kit.http.route("/index.json", body);
        kit.http.route("/index.json.minisig", sig.into_bytes());
        let hits = kit.http.hits();
        let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
        assert_eq!(run.code, 1, "{}", run.all());
        assert!(run.stdout.is_empty(), "{}", run.all());
        assert!(run.stderr.contains("E_SIGNATURE_INVALID"), "{}", run.all());
        assert!(run.stderr.contains("retry shortly"), "{}", run.all());
        assert_eq!(kit.http.hits() - hits, 2, "only the index and signature");
        no_adapter_files(&kit);
    }
}

#[test]
fn import_with_unknown_signing_key_is_refused() {
    let a = alpha();
    let kit = Kit::new();
    let stranger = oxidepm_adapter_proto::signing::testkeys::TestKey::generate();
    kit.publish_signed(&[&a], &IndexOpts::default(), &stranger, false);
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_SIGNATURE_INVALID"), "{}", run.stderr);
    assert!(
        run.stderr.contains("not one of the keys"),
        "there is no trust on first use: {}",
        run.stderr
    );
    no_adapter_files(&kit);
}

#[test]
fn legacy_minisign_signature_is_refused() {
    let a = alpha();
    let kit = Kit::new();
    kit.publish_signed(&[&a], &IndexOpts::default(), &kit.key, true);
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("E_SIGNATURE_INVALID") && run.stderr.contains("legacy"),
        "{}",
        run.stderr
    );
}

#[test]
fn import_with_expired_index_is_refused() {
    let a = alpha();
    let kit = Kit::new();
    kit.publish(
        &[&a],
        &IndexOpts {
            generated: "2020-01-01T00:00:00Z",
            expires: "2020-03-01T00:00:00Z",
            ..IndexOpts::default()
        },
    );
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_INDEX_STALE"), "{}", run.stderr);
    no_adapter_files(&kit);
}

// ---- the archive -----------------------------------------------------------------------------

#[test]
fn import_with_wrong_archive_hash_is_refused() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    // the server hands out another archive than the one the signed index describes
    // a small executable keeps this archive under the size the signed index announces,
    // whatever the fixture binary compresses to
    let other = Pkg::tool("alpha", "0.1.0")
        .with("summary", json!("Not the one."))
        .exec(b"not the fixture".to_vec())
        .package();
    kit.http.route("/alpha-0.1.0.tar.gz", other.archive);
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_CHECKSUM_MISMATCH"), "{}", run.stderr);
    no_adapter_files(&kit);
}

#[test]
fn an_executable_that_differs_from_the_signed_hash_is_refused() {
    let a = alpha();
    let kit = Kit::new();
    // an index whose archive hash is right but whose executable hash is not
    let mut doc = kit.index_json(&[&a], &IndexOpts::default());
    doc["adapters"][0]["releases"][0]["artifacts"][target()]["exe_sha256"] = json!("0".repeat(64));
    let body = doc.to_string().into_bytes();
    let sig = kit.key.sign_index(&body);
    kit.http.route("/index.json", body);
    kit.http.route("/index.json.minisig", sig.into_bytes());
    kit.http.route("/alpha-0.1.0.tar.gz", a.archive.clone());
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("E_CHECKSUM_MISMATCH") && run.stderr.contains("executable"),
        "{}",
        run.stderr
    );
}

#[test]
fn script_exec_is_refused_at_import() {
    let script = Pkg::tool("scripted", "0.1.0")
        .exec(b"#!/bin/sh\necho hi\n".to_vec())
        .package();
    let kit = kit_with(&[&script]);
    let run = kit.run(&["--dry-run", "adapter", "import", "scripted"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("E_ADAPTER_INVALID") && run.stderr.contains("native executable"),
        "{}",
        run.stderr
    );
}

#[test]
fn an_archive_with_a_path_escape_is_refused() {
    let a = Pkg::tool("alpha", "0.1.0");
    let bad = a.package_with(&[("../escape", b"x", 0o644)]);
    let kit = kit_with(&[&bad]);
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_ADAPTER_INVALID"), "{}", run.stderr);
    assert!(!kit.dir.path().join("escape").exists());
}

// ---- pinned ----------------------------------------------------------------------------------

#[test]
fn import_pinned_hash_requires_the_matching_hash() {
    let a = alpha();
    let kit = Kit::new();
    let file = kit.dir.path().join("alpha.tar.gz");
    std::fs::write(&file, &a.archive).unwrap();
    let path = file.to_str().unwrap();

    // the first run prints the hash and exits 2, installing nothing
    let first = kit.run(&["--dry-run", "adapter", "import", path]);
    assert_eq!(first.code, 2, "{}", first.all());
    assert!(
        first.stderr.contains(&a.archive_sha),
        "the computed hash is printed: {}",
        first.stderr
    );
    assert!(
        !first.stdout.contains("Adapter "),
        "no preview before the hash is pinned: {}",
        first.stdout
    );

    // a hash that is not the archive's
    let wrong = kit.run(&[
        "--dry-run",
        "adapter",
        "import",
        path,
        "--sha256",
        &"0".repeat(64),
    ]);
    assert_eq!(wrong.code, 1, "{}", wrong.all());
    assert!(
        wrong.stderr.contains("E_CHECKSUM_MISMATCH"),
        "{}",
        wrong.stderr
    );

    // the right one shows the preview, labelled as pinned by the user, with no key id
    let right = kit.run(&[
        "--dry-run",
        "adapter",
        "import",
        path,
        "--sha256",
        &a.archive_sha,
    ]);
    assert_eq!(right.code, 0, "{}", right.all());
    assert!(
        right.stdout.contains("pinned by you: sha256"),
        "{}",
        right.stdout
    );
    assert!(right.stdout.contains(&a.archive_sha), "{}", right.stdout);
    assert!(!right.stdout.contains("official"), "{}", right.stdout);
    no_adapter_files(&kit);
}

#[test]
fn import_pinned_url_downloads_and_needs_the_hash_too() {
    let a = alpha();
    let kit = Kit::new();
    kit.http.route("/alpha.tar.gz", a.archive.clone());
    let url = kit.http.url("/alpha.tar.gz");
    let first = kit.run(&["--dry-run", "adapter", "import", &url]);
    assert_eq!(first.code, 2, "{}", first.all());
    assert!(first.stderr.contains(&a.archive_sha), "{}", first.stderr);
    let right = kit.run(&[
        "--dry-run",
        "adapter",
        "import",
        &url,
        "--sha256",
        &a.archive_sha,
    ]);
    assert_eq!(right.code, 0, "{}", right.all());
}

// ---- the preview -----------------------------------------------------------------------------

#[test]
fn import_preview_names_broad_permissions() {
    let wide = Pkg::service("wide", "0.3.0")
        .perms(json!({
            "host": {"level": "admin", "scope": "all"},
            "events": ["lifecycle"],
            "env_pass": ["SSH_AUTH_SOCK", "AWS_SECRET_ACCESS_KEY"],
            "network": {"mode": "outbound", "hosts": ["api.example.org"]},
            "fs": {"read": ["any"], "write": ["$STATE"]},
            "spawn": {"mode": "any"}
        }))
        .package();
    let kit = kit_with(&[&wide]);
    let run = kit.run(&["--dry-run", "adapter", "import", "wide"]);
    assert_eq!(run.code, 0, "{}", run.all());
    let out = &run.stdout;
    assert!(out.contains("Adapter   wide 0.3.0 (service)"), "{out}");
    assert!(
        out.contains(&format!("signed by key {} (official)", kit.key.key_id())),
        "{out}"
    );
    assert!(
        out.contains(&format!("Archive sha256 {} verified", wide.archive_sha)),
        "{out}"
    );
    assert!(
        out.contains(&format!("Program sha256 {}", wide.exe_sha)),
        "{out}"
    );
    for needle in [
        "ADMIN",
        "outbound",
        "api.example.org",
        "Not enforceable per host",
        "reads any file you can read",
        "ANY program",
        "SSH_AUTH_SOCK",
        "AWS_SECRET_ACCESS_KEY",
    ] {
        assert!(out.contains(needle), "the preview names {needle}: {out}");
    }
    assert!(out.matches("[BROAD]").count() >= 5, "{out}");
    assert!(out.contains("Enforced on this machine"), "{out}");
    // the fingerprint to pass at enable, and the permission block again at the end
    let manifest =
        oxidepm_adapter_proto::Manifest::parse(wide.manifest.to_string().as_bytes()).unwrap();
    let fp = manifest.fingerprint(&wide.exe_sha);
    assert!(
        out.contains(&format!("oxidepm adapter enable wide --accept {fp}")),
        "{out}"
    );
    let next_at = out.find("Next      ").expect("a Next line");
    let again_at = out.find("Again, what it needs:").expect("the block again");
    assert!(
        again_at > next_at,
        "the permission block is repeated at the end: {out}"
    );
    let end = out.split("Again, what it needs:").nth(1).unwrap();
    assert!(
        end.contains("ADMIN") && end.contains("ANY program") && end.contains("SSH_AUTH_SOCK"),
        "{out}"
    );
}

#[test]
fn a_pure_adapter_is_not_marked_broad() {
    let kit = kit_with(&[&alpha()]);
    let run = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(!run.stdout.contains("[BROAD]"), "{}", run.stdout);
    assert!(
        run.stdout.contains("writes only its own directory"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("read only (status)"), "{}", run.stdout);
}

#[test]
fn preview_strips_escape_sequences() {
    // control characters are refused when the manifest is validated, so none can reach the
    // preview; the refusal does not print them either
    let evil = Pkg::tool("evil", "0.1.0")
        .with(
            "description",
            json!("Harmless.\u{1b}[2K\u{1b}[1A\u{1b}[2Kneeds nothing\u{9b}2J"),
        )
        .package();
    let kit = kit_with(&[&evil]);
    let run = kit.run(&["--dry-run", "adapter", "import", "evil"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_ADAPTER_INVALID"), "{}", run.stderr);
    assert!(
        !run.all().contains('\u{1b}') && !run.all().contains('\u{9b}'),
        "no escape reaches the terminal: {:?}",
        run.all()
    );

    // the same for text that comes out of the index
    let kit = Kit::new();
    let ok = alpha();
    let mut doc = kit.index_json(&[&ok], &IndexOpts::default());
    doc["adapters"][0]["summary"] = json!("fine\u{1b}[31m red");
    let body = doc.to_string().into_bytes();
    kit.http.route("/index.json", body.clone());
    let comment = "index sequence=42 generated=2026-11-20T10:00:00Z";
    kit.http.route(
        "/index.json.minisig",
        kit.key.sign(&body, comment).into_bytes(),
    );
    let run = kit.run(&["adapter", "list", "--available"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(!run.all().contains('\u{1b}'), "{:?}", run.all());
}

// ---- rehearsal -------------------------------------------------------------------------------

#[test]
fn dry_run_import_downloads_verifies_and_leaves_no_trace() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let tmp = kit.dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let mut cmd = kit.cli();
    cmd.env("TMPDIR", &tmp)
        .args(["--dry-run", "adapter", "import", "alpha"]);
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("Dry run: verified, nothing was installed"),
        "{text}"
    );
    // it did go to the network for the index, the signature and the archive
    assert_eq!(kit.http.hits(), 3, "index, signature and archive");
    // and left nothing: no adapter directory, no table, nothing in the temporary directory
    no_adapter_files(&kit);
    let left: Vec<_> = std::fs::read_dir(&tmp)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(
        left.is_empty(),
        "the rehearsal's files were deleted: {left:?}"
    );
}

// ---- the bundle ------------------------------------------------------------------------------

#[test]
fn bundled_reserved_names_require_a_release_signature() {
    use oxidepm_adapter_proto::signing::{bundle_trusted_comment, testkeys::TestKey};

    let node = Pkg::tool("runtime-node", "0.1.0").package();
    let kit = kit_with(&[&node]);
    let daemon = kit.daemon();
    let args = ["adapter", "import", "--bundled", "runtime-node"];
    let preview = ["--dry-run"].into_iter().chain(args).collect::<Vec<_>>();
    let signature = kit.bundle.join("SHA256SUMS.minisig");
    let checksums = std::fs::read(kit.bundle.join("SHA256SUMS")).unwrap();
    let tag = concat!("v", env!("CARGO_PKG_VERSION"));
    let target = target();
    let other_target = if target == "x86_64-unknown-linux-gnu" {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    let comment = bundle_trusted_comment(tag, &target);
    let valid = kit.key.sign(&checksums, &comment);

    for (contents, mode, reason) in [
        (None, 0o644, "SHA256SUMS.minisig"),
        (
            Some(kit.key.sign(b"different checksum list", &comment)),
            0o644,
            "does not match the signed data",
        ),
        (
            Some(
                kit.key
                    .sign(&checksums, &bundle_trusted_comment("v0.0.0", &target)),
            ),
            0o644,
            "trusted comment",
        ),
        (
            Some(
                kit.key
                    .sign(&checksums, &bundle_trusted_comment(tag, other_target)),
            ),
            0o644,
            "trusted comment",
        ),
        (
            Some(TestKey::generate().sign(&checksums, &comment)),
            0o644,
            "which is not one of the keys this build trusts",
        ),
        (Some(valid.clone()), 0o664, "group or other write access"),
    ] {
        if let Some(contents) = contents {
            std::fs::write(&signature, contents).unwrap();
            std::fs::set_permissions(&signature, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let run = kit.run(&preview);
        stage_by_hand(&kit, &node, "runtime-node");
        let reply = raw_install(&daemon, "runtime-node", "bundled", None);
        assert_eq!(run.code, 1, "{}", run.all());
        assert!(run.stderr.contains("E_ADAPTER_INVALID"), "{}", run.all());
        assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");
        for message in [run.stderr.as_str(), reply["message"].as_str().unwrap()] {
            assert!(message.contains(reason), "{message}");
            assert!(message.contains("--allow-reserved-name"), "{message}");
        }
    }
    assert!(names_listed(&kit).is_empty());

    let mut explicit_preview = preview.clone();
    explicit_preview.push("--allow-reserved-name");
    let run = kit.run(&explicit_preview);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("Bundle signature unverified"),
        "{}",
        run.all()
    );

    std::fs::write(&signature, valid).unwrap();
    std::fs::set_permissions(&signature, std::fs::Permissions::from_mode(0o644)).unwrap();
    let run = kit.run(&preview);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("Bundle signature verified with key"),
        "{}",
        run.all()
    );
    let run = kit.run(&args);
    assert_eq!(run.code, 0, "{}", run.all());
    assert_eq!(info_json(&kit, "runtime-node")["source"], "bundled");
    assert_eq!(kit.http.hits(), 0);
    stop(daemon);
}

#[test]
fn unsigned_release_bundle_installs_without_index_keys() {
    let adapter = alpha();
    let kit = kit_with(&[&adapter]);
    std::fs::remove_file(kit.bundle.join("index.json.minisig")).unwrap();
    let bundle = kit.bundle.to_str().unwrap();
    let daemon = DaemonProc::spawn_with_env(&kit.home, &[("OXIDEPM_TEST_BUNDLE_DIR", bundle)]);
    let out = kit
        .cli()
        .env("OXIDEPM_TEST_INDEX_URL", "")
        .env_remove("OXIDEPM_TEST_INDEX_KEYS")
        .args(["adapter", "import", "--bundled", "alpha"])
        .output()
        .unwrap();
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{output}");
    let enabled = enable_with_fingerprint(&kit, &adapter);
    assert_eq!(enabled.code, 0, "{}", enabled.all());
    let info = info_json(&kit, "alpha");
    assert_eq!(info["source"], "bundled");
    assert_eq!(info["trust"], "bundled");
    assert_eq!(info["enabled"], true);
    assert_eq!(kit.http.hits(), 0);
    stop(daemon);
}

#[test]
fn unverified_bundled_runtimes_need_explicit_consent() {
    let runtime = Pkg::tool("runtime-demo", "0.1.0")
        .with(
            "provides",
            json!({"runtimes": ["demo"], "programs": ["demo"]}),
        )
        .package();
    for mismatched_signature in [false, true] {
        let kit = kit_with(&[&runtime]);
        if mismatched_signature {
            let comment = oxidepm_adapter_proto::signing::bundle_trusted_comment(
                concat!("v", env!("CARGO_PKG_VERSION")),
                &target(),
            );
            std::fs::write(
                kit.bundle.join("SHA256SUMS.minisig"),
                kit.key.sign(b"other checksums", &comment),
            )
            .unwrap();
        }
        let daemon = kit.daemon();
        let import = kit.run(&["adapter", "import", "--bundled", "runtime-demo"]);
        assert_eq!(import.code, 0, "{}", import.all());
        let auto = kit.run(&["adapter", "enable", "runtime-demo", "--yes"]);
        assert_eq!(auto.code, 2, "{}", auto.all());
        assert!(
            auto.stderr.contains("needs explicit consent"),
            "{}",
            auto.all()
        );
        let reply = daemon.rpc(json!({
            "type": "adapter", "op": "enable", "name": "runtime-demo", "yes": true,
        }));
        assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");
        assert!(
            reply["message"]
                .as_str()
                .unwrap()
                .contains("requires explicit consent"),
            "{reply}"
        );
        assert_eq!(info_json(&kit, "runtime-demo")["enabled"], false);
        let info = kit.run(&["adapter", "info", "runtime-demo"]);
        assert!(
            info.stdout.contains("bundled, unverified"),
            "{}",
            info.all()
        );
        let list = kit.run(&["adapter", "list"]);
        assert!(
            list.stdout.contains("bundled, unverified"),
            "{}",
            list.all()
        );
        let explicit = enable_with_fingerprint(&kit, &runtime);
        assert_eq!(explicit.code, 0, "{}", explicit.all());
    }
}

#[test]
fn daemon_checks_the_archive_and_staged_files_of_a_release_bundle() {
    let adapter = alpha();
    let kit = kit_with(&[&adapter]);
    std::fs::remove_file(kit.bundle.join("index.json.minisig")).unwrap();
    let bundle = kit.bundle.to_str().unwrap();
    let daemon = DaemonProc::spawn_with_env(&kit.home, &[("OXIDEPM_TEST_BUNDLE_DIR", bundle)]);

    let tampered = stage_by_hand(&kit, &adapter, "tampered");
    let manifest = tampered.join("manifest.json");
    let mut body: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    body["summary"] = json!("Altered manifest");
    std::fs::write(manifest, body.to_string()).unwrap();
    let changed_tree = raw_install(&daemon, "tampered", "bundled", None);

    let archive = kit.bundle.join("alpha-0.1.0.tar.gz");
    let mut bytes = std::fs::read(&archive).unwrap();
    let last = bytes.last_mut().unwrap();
    *last ^= 1;
    std::fs::write(archive, bytes).unwrap();
    stage_by_hand(&kit, &adapter, "original");
    let changed_archive = raw_install(&daemon, "original", "bundled", None);

    assert_eq!(
        changed_tree["code"], "E_CHECKSUM_MISMATCH",
        "{changed_tree}"
    );
    assert_eq!(
        changed_archive["code"], "E_CHECKSUM_MISMATCH",
        "{changed_archive}"
    );
    assert!(names_listed(&kit).is_empty());
}

#[test]
fn bundled_import_refuses_changed_archives_and_catalogs() {
    let a = alpha();
    let b = Pkg::tool("beta", "0.2.0")
        .with("tags", json!(["runtime"]))
        .package();
    let kit = kit_with(&[&a, &b]);
    // the release catalog and archives are both checked offline
    let mut cmd = kit.cli();
    cmd.env("OXIDEPM_TEST_INDEX_URL", "").args([
        "--dry-run",
        "adapter",
        "import",
        "--bundled",
        "alpha",
    ]);
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("Adapter   alpha 0.1.0"), "{text}");
    assert!(text.contains("bundle "), "{text}");
    assert!(text.contains("bundled with this release"), "{text}");
    assert_eq!(
        kit.http.hits(),
        0,
        "a bundled import does not use the network"
    );

    // a tampered archive in the bundle
    std::fs::write(kit.bundle.join("alpha-0.1.0.tar.gz"), &b.archive).unwrap();
    let bad = kit.run(&["--dry-run", "adapter", "import", "--bundled", "alpha"]);
    assert_eq!(bad.code, 1, "{}", bad.all());
    assert!(bad.stderr.contains("E_CHECKSUM_MISMATCH"), "{}", bad.stderr);

    // a tampered index in the bundle
    std::fs::write(kit.bundle.join("alpha-0.1.0.tar.gz"), &a.archive).unwrap();
    let body = std::fs::read_to_string(kit.bundle.join("index.json"))
        .unwrap()
        .replace("0.1.0", "0.1.1");
    std::fs::write(kit.bundle.join("index.json"), body).unwrap();
    let bad = kit.run(&["--dry-run", "adapter", "import", "--bundled", "alpha"]);
    assert_eq!(bad.code, 1, "{}", bad.all());
    assert!(bad.stderr.contains("E_CHECKSUM_MISMATCH"), "{}", bad.stderr);
}

#[test]
fn a_bundle_import_by_tag_takes_only_the_adapters_with_the_tag() {
    let a = alpha();
    let b = Pkg::tool("beta", "0.2.0")
        .with("tags", json!(["runtime"]))
        .package();
    let kit = kit_with(&[&a, &b]);
    // the index lists tags of its own, apart from the manifests': both carry "test"
    let all = kit.run(&[
        "--dry-run",
        "adapter",
        "import",
        "--bundled",
        "--tag",
        "test",
    ]);
    assert_eq!(all.code, 0, "{}", all.all());
    assert!(
        all.stdout.contains("Adapter   alpha") && all.stdout.contains("Adapter   beta"),
        "{}",
        all.stdout
    );
    let none = kit.run(&[
        "--dry-run",
        "adapter",
        "import",
        "--bundled",
        "--tag",
        "nothing",
    ]);
    assert_eq!(none.code, 1, "{}", none.all());
    assert!(
        none.stderr.contains("no adapter that matches"),
        "{}",
        none.stderr
    );
}

// ======================================================================================
// 28b: install, enable, disable, remove, list, info, backup and restore
// ======================================================================================

use common::daemon::{DaemonProc, Signal};
use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

fn json_of(run: &Run) -> Value {
    serde_json::from_str(&run.stdout).unwrap_or_else(|e| panic!("{e}: {}", run.all()))
}

fn info_json(kit: &Kit, name: &str) -> Value {
    let run = kit.run(&["--json", "adapter", "info", name]);
    assert_eq!(run.code, 0, "{}", run.all());
    json_of(&run)
}

fn names_listed(kit: &Kit) -> Vec<String> {
    let run = kit.run(&["--json", "adapter", "list"]);
    assert_eq!(run.code, 0, "{}", run.all());
    json_of(&run)
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_string())
        .collect()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// Stop the daemon the polite way, so its database is closed.
fn stop(mut daemon: DaemonProc) {
    daemon.signal(Signal::SIGTERM);
    daemon.wait_exit_for(Duration::from_secs(20));
}

fn entries(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn import(kit: &Kit, name: &str) -> Run {
    let run = kit.run(&["adapter", "import", name]);
    assert_eq!(run.code, 0, "importing {name}: {}", run.all());
    run
}

// ---- install ---------------------------------------------------------------------------------

#[test]
fn import_from_signed_index_installs_disabled() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();

    let run = import(&kit, "alpha");
    assert!(
        run.stdout
            .contains("Installed alpha 0.1.0 (disabled, nothing runs)"),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains(&format!(
            "oxidepm adapter enable alpha --accept {}",
            a.fingerprint()
        )),
        "{}",
        run.stdout
    );

    let info = info_json(&kit, "alpha");
    assert_eq!(info["trust"], format!("signed:{}", kit.key.key_id()));
    assert_eq!(info["source"], "index");
    assert_eq!(info["exe_sha256"], a.exe_sha);
    assert_eq!(info["fingerprint"], a.fingerprint());
    assert_eq!(info["enabled"], false);
    assert_eq!(info["state"], "disabled");
    assert!(info["grant"].is_null(), "a disabled adapter holds no grant");

    // the files: directories 0700, the executable 0500, the manifest 0400, and the staging
    // directory is gone
    let dir = kit.home.join("adapters/installed/alpha/0.1.0");
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("manifest.json")), 0o400);
    assert_eq!(mode(&dir.join("oxidepm-fixture-adapter")), 0o500);
    assert!(
        entries(&kit.home.join("adapters/staging")).is_empty(),
        "{:?}",
        entries(&kit.home.join("adapters/staging"))
    );

    // and the recorded digest is what runs: the file on disk hashes to it
    assert_eq!(
        sha_hex(&std::fs::read(dir.join("oxidepm-fixture-adapter")).unwrap()),
        a.exe_sha
    );
    drop(daemon);
}

#[test]
fn import_installs_disabled_and_starts_nothing() {
    let io_kit = Kit::new();
    let io = io_kit.io_dir();
    let svc = Pkg::service("svc", "0.1.0").fixture_io(&io).package();
    io_kit.publish(&[&svc], &IndexOpts::default());
    let env = io_kit.script_env("svc");
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = io_kit.daemon_with(&env_refs);
    let run = io_kit.run(&["adapter", "import", "svc"]);
    assert_eq!(run.code, 0, "{}", run.all());
    std::thread::sleep(Duration::from_millis(1500));
    let status = daemon.rpc(json!({"type": "daemon_status"}));
    let adapter = status["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "svc")
        .cloned()
        .expect("listed");
    assert_eq!(adapter["state"], "disabled", "{status}");
    assert!(
        io_kit.report("svc").is_empty(),
        "the adapter never started: {:?}",
        io_kit.report("svc")
    );
}

#[test]
fn a_refused_import_leaves_nothing_installed() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    // the archive the server hands out is not the one the signed index describes
    // a small executable keeps this archive under the size the signed index announces,
    // whatever the fixture binary compresses to
    let other = Pkg::tool("alpha", "0.1.0")
        .with("summary", json!("Not the one."))
        .exec(b"not the fixture".to_vec())
        .package();
    kit.http.route("/alpha-0.1.0.tar.gz", other.archive);
    let run = kit.run(&["adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_CHECKSUM_MISMATCH"), "{}", run.stderr);
    assert!(names_listed(&kit).is_empty());
    assert!(entries(&kit.home.join("adapters/installed")).is_empty());
    assert!(
        entries(&kit.home.join("adapters/staging")).is_empty(),
        "the staging directory was removed"
    );
    drop(daemon);
}

#[test]
fn import_with_older_sequence_is_refused() {
    let a = alpha();
    let b = Pkg::tool("beta", "0.1.0").package();
    let kit = Kit::new();
    kit.publish(
        &[&a, &b],
        &IndexOpts {
            sequence: 50,
            ..IndexOpts::default()
        },
    );
    let daemon = kit.daemon();
    import(&kit, "alpha");
    // a replayed older index
    kit.publish(
        &[&a, &b],
        &IndexOpts {
            sequence: 49,
            ..IndexOpts::default()
        },
    );
    let run = kit.run(&["adapter", "import", "beta"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("E_INDEX_STALE") && run.stderr.contains("50"),
        "{}",
        run.stderr
    );
    assert_eq!(names_listed(&kit), vec!["alpha"]);
    // reading the index is refused the same way
    let listing = kit.run(&["adapter", "list", "--available"]);
    assert_eq!(listing.code, 1, "{}", listing.all());
    assert!(
        listing.stderr.contains("E_INDEX_STALE"),
        "{}",
        listing.stderr
    );
    // the same sequence again is fine, and a newer one moves the mark
    kit.publish(
        &[&a, &b],
        &IndexOpts {
            sequence: 50,
            ..IndexOpts::default()
        },
    );
    import(&kit, "beta");
    drop(daemon);
}

#[test]
fn last_sequence_is_kept_per_key() {
    let a = alpha();
    let b = Pkg::tool("beta", "0.1.0").package();
    let mut kit = Kit::new();
    let other_key = oxidepm_adapter_proto::signing::testkeys::TestKey::generate();
    kit.trust(&other_key);
    // one stray high sequence under the first key ...
    kit.publish(
        &[&a, &b],
        &IndexOpts {
            sequence: 1_000_000,
            ..IndexOpts::default()
        },
    );
    let daemon = kit.daemon();
    import(&kit, "alpha");
    // ... does not make an index signed by another key stale
    kit.publish_signed(
        &[&a, &b],
        &IndexOpts {
            sequence: 3,
            ..IndexOpts::default()
        },
        &other_key,
        false,
    );
    let run = kit.run(&["adapter", "import", "beta"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("Installed beta"), "{}", run.stdout);
    let info = info_json(&kit, "beta");
    assert_eq!(info["trust"], format!("signed:{}", other_key.key_id()));
    drop(daemon);
}

#[test]
fn import_of_installed_name_is_not_a_second_install() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let again = kit.run(&["adapter", "import", "alpha"]);
    assert_eq!(again.code, 1, "{}", again.all());
    assert!(
        again.stderr.contains("installed already"),
        "{}",
        again.stderr
    );
    assert_eq!(names_listed(&kit), vec!["alpha"]);
    drop(daemon);
}

// ---- the daemon checks again what the CLI checked --------------------------------------------

/// Put the archive's files in `adapters/staging/<name>` the way the CLI would have.
fn stage_by_hand(kit: &Kit, pkg: &Packaged, name: &str) -> std::path::PathBuf {
    let staging = kit.home.join("adapters/staging");
    std::fs::create_dir_all(&staging).unwrap();
    for dir in [kit.home.join("adapters"), staging.clone()] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let target = staging.join(name);
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    let decoder = flate2::read::GzDecoder::new(&pkg.archive[..]);
    tar::Archive::new(decoder).unpack(&target).unwrap();
    target
}

fn raw_install(
    daemon: &DaemonProc,
    staging: &str,
    source: &str,
    index: Option<(&str, &str)>,
) -> Value {
    let mut request =
        json!({"type": "adapter", "op": "install", "staging": staging, "source": source});
    if let Some((bytes, sig)) = index {
        request["index_bytes"] = json!(bytes);
        request["index_sig"] = json!(sig);
    }
    daemon.rpc(request)
}

#[test]
fn staged_files_changed_after_cli_verification_are_refused() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    let tree = stage_by_hand(&kit, &a, "swapped");
    // between the CLI's check and the daemon's, the executable is replaced by another ELF file
    let exe = tree.join("oxidepm-fixture-adapter");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut bytes = std::fs::read(&exe).unwrap();
    bytes.push(0);
    std::fs::write(&exe, bytes).unwrap();
    let body = String::from_utf8(kit.http_index_body()).unwrap();
    let sig = kit.http_index_sig();
    let reply = raw_install(&daemon, "swapped", "index", Some((&body, &sig)));
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], "E_CHECKSUM_MISMATCH", "{reply}");
    assert!(
        entries(&kit.home.join("adapters/installed"))
            .iter()
            .all(|e| !e.contains("alpha")),
        "{:?}",
        entries(&kit.home.join("adapters/installed"))
    );
    assert!(
        entries(&kit.home.join("adapters/installed")).is_empty(),
        "the moved directory was deleted again: {:?}",
        entries(&kit.home.join("adapters/installed"))
    );
}

#[test]
fn a_signed_artifact_requires_the_matching_manifest_digest() {
    let adapter = alpha();
    let kit = kit_with(&[&adapter]);
    let daemon = kit.daemon();
    stage_by_hand(&kit, &adapter, "release");
    let mut doc = kit.index_json(&[&adapter], &IndexOpts::default());
    doc["adapters"][0]["releases"][0]["artifacts"][target()]["manifest_sha256"] =
        json!("0".repeat(64));
    let body = doc.to_string();
    let sig = kit.key.sign_index(body.as_bytes());
    let result = raw_install(&daemon, "release", "index", Some((&body, &sig)));
    assert_eq!(result["code"], "E_CHECKSUM_MISMATCH", "{result}");
    assert!(
        result["message"].as_str().unwrap().contains("manifest"),
        "{result}"
    );
}

#[test]
fn daemon_re_verifies_the_index_signature() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    // an index the key never signed: the exe hash in it matches the staged file, so only the
    // signature stands in the way
    let mut doc = kit.index_json(&[&a], &IndexOpts::default());
    doc["sequence"] = json!(43);
    let forged = doc.to_string();
    let genuine_sig = kit.http_index_sig();
    stage_by_hand(&kit, &a, "forged");
    let reply = raw_install(&daemon, "forged", "index", Some((&forged, &genuine_sig)));
    assert_eq!(reply["type"], "error", "{reply}");
    assert_eq!(reply["code"], "E_SIGNATURE_INVALID", "{reply}");

    // and an unsigned source that claims the index
    stage_by_hand(&kit, &a, "unsigned");
    let reply = raw_install(&daemon, "unsigned", "index", None);
    assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");
    assert!(names_listed(&kit).is_empty());
}

#[test]
fn the_daemon_names_a_staging_directory_by_one_plain_component() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    for bad in ["../x", "a/b", "..", ".", "", "/etc"] {
        let reply = raw_install(&daemon, bad, "pinned:test", None);
        assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{bad:?}: {reply}");
    }
    let missing = raw_install(&daemon, "nothing-here", "pinned:test", None);
    assert_eq!(missing["code"], "E_ADAPTER_INVALID", "{missing}");
}

#[test]
fn third_party_cannot_provide_node_runtime() {
    let squatter = Pkg::tool("runtime-node", "0.1.0")
        .with(
            "provides",
            json!({"runtimes": ["node"], "programs": ["node"]}),
        )
        .package();
    let kit = Kit::new();
    let file = kit.dir.path().join("rt.tar.gz");
    std::fs::write(&file, &squatter.archive).unwrap();
    let daemon = kit.daemon();
    // pinned: refused by the CLI ...
    let run = kit.run(&[
        "adapter",
        "import",
        file.to_str().unwrap(),
        "--sha256",
        &squatter.archive_sha,
    ]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("E_ADAPTER_INVALID") && run.stderr.contains("runtime-node"),
        "{}",
        run.stderr
    );
    // ... and by the daemon, if a staged directory gets to it by another way
    stage_by_hand(&kit, &squatter, "squat");
    let reply = raw_install(&daemon, "squat", "pinned:by-hand", None);
    assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");
    assert!(
        reply["message"].as_str().unwrap().contains("runtime-node"),
        "{reply}"
    );
    assert!(names_listed(&kit).is_empty());
}

#[test]
fn a_signed_adapter_may_use_a_reserved_name() {
    let node = Pkg::tool("runtime-node", "0.1.0")
        .with(
            "provides",
            json!({"runtimes": ["node"], "programs": ["node"]}),
        )
        .package();
    let kit = kit_with(&[&node]);
    let daemon = kit.daemon();
    import(&kit, "runtime-node");
    assert_eq!(names_listed(&kit), vec!["runtime-node"]);
    drop(daemon);
}

#[test]
fn reserved_name_for_pinned_adapter_needs_the_flag() {
    let node = Pkg::tool("runtime-node", "0.1.0")
        .with(
            "provides",
            json!({"runtimes": ["node"], "programs": ["node"]}),
        )
        .package();
    let kit = Kit::new();
    let file = kit.dir.path().join("rt.tar.gz");
    std::fs::write(&file, &node.archive).unwrap();
    let daemon = kit.daemon();
    let args = [
        "adapter",
        "import",
        file.to_str().unwrap(),
        "--sha256",
        &node.archive_sha,
        "--allow-reserved-name",
    ];
    let run = kit.run(&args);
    assert_eq!(run.code, 0, "{}", run.all());
    let info = info_json(&kit, "runtime-node");
    assert_eq!(info["trust"], "pinned");
    assert!(
        info["source"].as_str().unwrap().starts_with("pinned:"),
        "{info}"
    );
    drop(daemon);
}

#[test]
fn verb_colliding_with_a_builtin_is_refused() {
    let verb = json!([{"name": "status", "summary": "x", "tier": "safe", "mode": "relayed", "writes": false, "dry_run": true, "max_runtime_secs": 60, "args_summary": ""}]);
    let clash = Pkg::tool("clash", "0.1.0")
        .with("provides", json!({"verbs": verb}))
        .package();
    let kit = kit_with(&[&clash]);
    let daemon = kit.daemon();
    let run = kit.run(&["adapter", "import", "clash"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("E_ADAPTER_INVALID") && run.stderr.contains("status"),
        "{}",
        run.stderr
    );
    // the daemon refuses it too, given the same list by a hand-made request
    stage_by_hand(&kit, &clash, "clash");
    let mut request = json!({"type": "adapter", "op": "install", "staging": "clash", "source": "pinned:by-hand", "allow_reserved_name": true});
    request["builtin_verbs"] = json!(["status"]);
    let reply = daemon.rpc(request);
    assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");
    assert!(names_listed(&kit).is_empty());
}

#[test]
fn a_verb_of_another_installed_adapter_is_refused() {
    let verb = |name: &str| json!([{"name": name, "summary": "x", "tier": "safe", "mode": "relayed", "writes": false, "dry_run": true, "max_runtime_secs": 60, "args_summary": ""}]);
    let first = Pkg::tool("first", "0.1.0")
        .with("provides", json!({"verbs": verb("lint")}))
        .package();
    let second = Pkg::tool("second", "0.1.0")
        .with("provides", json!({"verbs": verb("lint")}))
        .package();
    let kit = kit_with(&[&first, &second]);
    let daemon = kit.daemon();
    import(&kit, "first");
    let run = kit.run(&["adapter", "import", "second"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("E_ADAPTER_INVALID")
            && run.stderr.contains("first")
            && run.stderr.contains("lint"),
        "{}",
        run.stderr
    );
    drop(daemon);
}

// ---- enable ------------------------------------------------------------------------------------

#[test]
fn enable_requires_the_fingerprint_without_a_terminal() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let run = kit.run(&["adapter", "enable", "alpha"]);
    assert_eq!(run.code, 2, "{}", run.all());
    assert!(
        run.stderr
            .contains(&format!("--accept {}", a.fingerprint())),
        "{}",
        run.stderr
    );
    // nothing was enabled
    assert_eq!(info_json(&kit, "alpha")["enabled"], false);
    // a fingerprint that is not this release's is refused by the daemon
    let wrong = kit.run(&["adapter", "enable", "alpha", "--accept", "0123456789abcdef"]);
    assert_eq!(wrong.code, 1, "{}", wrong.all());
    assert!(
        wrong.stderr.contains("E_ADAPTER_INVALID") && wrong.stderr.contains(&a.fingerprint()),
        "{}",
        wrong.stderr
    );
    assert_eq!(info_json(&kit, "alpha")["enabled"], false);
    drop(daemon);
}

#[test]
fn yes_is_honoured_only_for_the_pure_class() {
    let pure = alpha();
    let wide = Pkg::tool("wide", "0.1.0")
        .perms(json!({"network": {"mode": "outbound", "hosts": ["example.org"]}}))
        .package();
    let kit = kit_with(&[&pure, &wide]);
    let daemon = kit.daemon();
    let bundled = kit.run(&["adapter", "import", "--bundled", "alpha"]);
    assert_eq!(bundled.code, 0, "{}", bundled.all());
    import(&kit, "wide");
    let ok = kit.run(&["adapter", "enable", "alpha", "--yes"]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(
        ok.stdout
            .contains("Enabled alpha 0.1.0 (level read, scope all)"),
        "{}",
        ok.stdout
    );
    let no = kit.run(&["adapter", "enable", "wide", "--yes"]);
    assert_eq!(no.code, 2, "{}", no.all());
    assert!(
        no.stderr.contains("--yes is honoured only") && no.stderr.contains("network = outbound"),
        "{}",
        no.stderr
    );
    assert!(no.stderr.contains(&wide.fingerprint()), "{}", no.stderr);
    assert_eq!(info_json(&kit, "wide")["enabled"], false);
    // with the fingerprint it works
    let accepted = kit.run(&["adapter", "enable", "wide", "--accept", &wide.fingerprint()]);
    assert_eq!(accepted.code, 0, "{}", accepted.all());
    drop(daemon);
}

#[test]
fn adapters_that_provide_programs_require_explicit_consent() {
    let runtime = Pkg::tool("runtime-demo", "0.1.0")
        .with(
            "provides",
            json!({"runtimes": ["demo"], "programs": ["demo"]}),
        )
        .package();
    let programs = Pkg::tool("programs", "0.1.0")
        .with("provides", json!({"programs": ["example"]}))
        .package();
    let kit = kit_with(&[&runtime, &programs]);
    let daemon = kit.daemon();

    for package in [&runtime, &programs] {
        let name = &package.name;
        import(&kit, name);
        let fingerprint = package.fingerprint();
        let reply =
            daemon.rpc(json!({"type": "adapter", "op": "enable", "name": name, "yes": true}));
        assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");
        assert!(
            reply["message"].as_str().unwrap().contains(&fingerprint),
            "{reply}"
        );

        let run = kit.run(&["adapter", "enable", name, "--yes"]);
        assert_eq!(run.code, 2, "{}", run.all());
        assert!(run.stderr.contains(&fingerprint), "{}", run.all());
        assert_eq!(info_json(&kit, name)["enabled"], false);

        let accepted = kit.run(&["adapter", "enable", name, "--accept", &fingerprint]);
        assert_eq!(accepted.code, 0, "{}", accepted.all());
    }
}

#[test]
fn a_self_built_adapter_providing_programs_needs_explicit_consent() {
    let package = Pkg::tool("programs", "0.1.0")
        .with("provides", json!({"programs": ["example"]}))
        .package();
    let kit = Kit::new();
    let archive = kit.dir.path().join("programs.tar.gz");
    std::fs::write(&archive, &package.archive).unwrap();
    let daemon = kit.daemon();
    let imported = kit.run(&[
        "adapter",
        "import",
        archive.to_str().unwrap(),
        "--sha256",
        &package.archive_sha,
    ]);
    assert_eq!(imported.code, 0, "{}", imported.all());
    assert_eq!(info_json(&kit, "programs")["trust"], "pinned");

    let fingerprint = package.fingerprint();
    let refused = kit.run(&["adapter", "enable", "programs", "--yes"]);
    assert_eq!(refused.code, 2, "{}", refused.all());
    assert!(refused.stderr.contains(&fingerprint), "{}", refused.all());
    assert_eq!(info_json(&kit, "programs")["enabled"], false);
    let reply = daemon.rpc(json!({
        "type": "adapter", "op": "enable", "name": "programs", "yes": true,
    }));
    assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");

    let accepted = kit.run(&["adapter", "enable", "programs", "--accept", &fingerprint]);
    assert_eq!(accepted.code, 0, "{}", accepted.all());
    assert_eq!(info_json(&kit, "programs")["enabled"], true);
    assert!(
        refused.stdout.contains("programs: example"),
        "{}",
        refused.all()
    );
}

#[test]
fn yes_is_not_honoured_with_env_pass_or_broad_fs_read() {
    let env = Pkg::tool("envy", "0.1.0")
        .perms(json!({"env_pass": ["SSH_AUTH_SOCK"]}))
        .package();
    let reader = Pkg::tool("reader", "0.1.0")
        .perms(json!({"fs": {"read": ["any"], "write": ["$STATE"]}}))
        .package();
    let spawner = Pkg::tool("spawner", "0.1.0")
        .perms(json!({"spawn": {"mode": "any"}}))
        .package();
    let kit = kit_with(&[&env, &reader, &spawner]);
    let daemon = kit.daemon();
    for name in ["envy", "reader", "spawner"] {
        import(&kit, name);
        let run = kit.run(&["adapter", "enable", name, "--yes"]);
        assert_eq!(run.code, 2, "{name}: {}", run.all());
        assert_eq!(info_json(&kit, name)["enabled"], false, "{name}");
    }
    // even if the CLI is bypassed, the daemon refuses
    let reply = daemon.rpc(json!({"type": "adapter", "op": "enable", "name": "envy", "yes": true}));
    assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");
    drop(daemon);
}

#[test]
fn yes_refusal_names_the_requested_read_paths() {
    let reader = Pkg::tool("reader", "0.1.0")
        .perms(json!({"fs": {"read": ["$STATE", "/etc/hosts", "$CWD", "/var/data"]}}))
        .package();
    let kit = kit_with(&[&reader]);
    let daemon = kit.daemon();
    import(&kit, "reader");

    let cli = kit.run(&["adapter", "enable", "reader", "--yes"]);
    let reply =
        daemon.rpc(json!({"type": "adapter", "op": "enable", "name": "reader", "yes": true}));
    assert_eq!(cli.code, 2, "{}", cli.all());
    assert_eq!(reply["code"], "E_ADAPTER_INVALID", "{reply}");
    for wanted in ["fs.read = $CWD", "fs.read = /var/data"] {
        assert!(
            cli.stderr.contains(wanted) && reply["message"].as_str().unwrap().contains(wanted),
            "{}; {reply}",
            cli.stderr
        );
    }
}

#[test]
fn accept_of_an_old_release_fingerprint_is_refused_for_a_new_release() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.1.1").package();
    assert_ne!(
        v1.fingerprint(),
        v2.fingerprint(),
        "same permissions, different release"
    );
    let kit = Kit::new();
    kit.publish(
        &[&v2],
        &IndexOpts {
            sequence: 43,
            ..IndexOpts::default()
        },
    );
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let stale = kit.run(&["adapter", "enable", "alpha", "--accept", &v1.fingerprint()]);
    assert_eq!(stale.code, 1, "{}", stale.all());
    assert!(stale.stderr.contains(&v2.fingerprint()), "{}", stale.stderr);
    let right = kit.run(&["adapter", "enable", "alpha", "--accept", &v2.fingerprint()]);
    assert_eq!(right.code, 0, "{}", right.all());
    drop(daemon);
}

#[test]
fn admin_grant_needs_an_interactive_terminal() {
    let boss = Pkg::tool("boss", "0.1.0")
        .perms(json!({"host": {"level": "admin", "scope": "all"}}))
        .package();
    let kit = kit_with(&[&boss]);
    let daemon = kit.daemon();
    import(&kit, "boss");
    let run = kit.run(&["adapter", "enable", "boss", "--accept", &boss.fingerprint()]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("interactive terminal"),
        "{}",
        run.stderr
    );
    assert_eq!(info_json(&kit, "boss")["enabled"], false);
    // --yes does not stand in for the terminal either
    let yes = kit.run(&["adapter", "enable", "boss", "--yes"]);
    assert_eq!(yes.code, 2, "{}", yes.all());
    // lowering the level is allowed, and does not need one
    let lowered = kit.run(&[
        "adapter",
        "enable",
        "boss",
        "--accept",
        &boss.fingerprint(),
        "--level",
        "lifecycle",
    ]);
    assert_eq!(lowered.code, 0, "{}", lowered.all());
    let grant = info_json(&kit, "boss")["grant"].clone();
    assert_eq!(grant["host"]["level"], "lifecycle", "{grant}");
    drop(daemon);
}

#[test]
fn a_level_can_be_lowered_and_never_raised() {
    let lc = Pkg::tool("lc", "0.1.0")
        .perms(json!({"host": {"level": "lifecycle", "scope": "all"}}))
        .package();
    let kit = kit_with(&[&lc]);
    let daemon = kit.daemon();
    import(&kit, "lc");
    let raise = kit.run(&[
        "adapter",
        "enable",
        "lc",
        "--accept",
        &lc.fingerprint(),
        "--level",
        "admin",
    ]);
    assert_eq!(raise.code, 1, "{}", raise.all());
    assert!(
        raise.stderr.contains("can only be lowered"),
        "{}",
        raise.stderr
    );
    let narrow = kit.run(&[
        "adapter",
        "enable",
        "lc",
        "--accept",
        &lc.fingerprint(),
        "--level",
        "read",
        "--scope",
        "opted-in",
    ]);
    assert_eq!(narrow.code, 0, "{}", narrow.all());
    let grant = info_json(&kit, "lc")["grant"].clone();
    assert_eq!(
        (
            grant["host"]["level"].as_str(),
            grant["host"]["scope"].as_str()
        ),
        (Some("read"), Some("opted-in")),
        "{grant}"
    );
    drop(daemon);
}

#[test]
fn enable_runs_the_tool_round_trip_and_fails_on_a_broken_adapter() {
    let kit = Kit::new();
    let io = kit.io_dir();
    let good = Pkg::tool("good", "0.1.0").package();
    let broken = Pkg::tool("broken", "0.1.0").fixture_io(&io).package();
    kit.publish(&[&good, &broken], &IndexOpts::default());
    // the broken one answers `initialize` with the wrong name
    kit.write_script(
        "broken",
        json!([{"on": "initialize", "do": {"lie_name": {}}}]),
    );
    let env = kit.script_env("broken");
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = kit.daemon_with(&env_refs);
    import(&kit, "good");
    import(&kit, "broken");

    let ok = enable_with_fingerprint(&kit, &good);
    assert_eq!(ok.code, 0, "{}", ok.all());
    let info = info_json(&kit, "good");
    assert_eq!(info["enabled"], true);
    // the tool did its round trip and was stopped again; it runs only when called
    assert!(
        ["stopped", "ready"].contains(&info["state"].as_str().unwrap()),
        "{info}"
    );

    let bad = kit.run(&[
        "adapter",
        "enable",
        "broken",
        "--accept",
        &broken.fingerprint(),
    ]);
    assert_eq!(bad.code, 1, "{}", bad.all());
    assert!(
        bad.stderr.contains("E_ADAPTER_TAMPERED"),
        "the error comes at enable: {}",
        bad.stderr
    );
    let info = info_json(&kit, "broken");
    assert_eq!(
        info["enabled"], false,
        "a broken adapter is not left enabled: {info}"
    );
    assert!(info["grant"].is_null());
    drop(daemon);
}

#[test]
fn a_service_is_started_by_enable_and_stopped_by_disable() {
    let kit = Kit::new();
    let io = kit.io_dir();
    let svc = Pkg::service("svc", "0.1.0").fixture_io(&io).package();
    kit.publish(&[&svc], &IndexOpts::default());
    kit.write_script("svc", json!([]));
    let env = kit.script_env("svc");
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = kit.daemon_with(&env_refs);
    import(&kit, "svc");
    assert!(kit.report("svc").is_empty());
    let run = kit.run(&["adapter", "enable", "svc", "--accept", &svc.fingerprint()]);
    assert_eq!(run.code, 0, "{}", run.all());
    let info = info_json(&kit, "svc");
    assert_eq!(info["state"], "ready", "{info}");
    assert_eq!(
        kit.report("svc")
            .iter()
            .filter(|r| r["what"] == "start")
            .count(),
        1
    );
    let enforced = info["enforced"].clone();
    assert!(
        enforced.is_object(),
        "the self-test result is stored: {info}"
    );

    let off = kit.run(&["adapter", "disable", "svc"]);
    assert_eq!(off.code, 0, "{}", off.all());
    let info = info_json(&kit, "svc");
    assert_eq!(
        (info["enabled"].as_bool(), info["state"].as_str()),
        (Some(false), Some("disabled")),
        "{info}"
    );
    assert!(
        info["grant"].is_null(),
        "a disabled adapter holds no rights"
    );
    drop(daemon);
}

#[test]
fn runtime_provided_twice_is_refused_at_enable() {
    let runtime = |name: &str| {
        Pkg::tool(name, "0.1.0")
            .with(
                "provides",
                json!({"runtimes": ["demo"], "programs": ["demo"]}),
            )
            .package()
    };
    let (one, two) = (runtime("rt-one"), runtime("rt-two"));
    let kit = kit_with(&[&one, &two]);
    let daemon = kit.daemon();
    import(&kit, "rt-one");
    import(&kit, "rt-two");
    assert_eq!(
        kit.run(&[
            "adapter",
            "enable",
            "rt-one",
            "--accept",
            &one.fingerprint()
        ])
        .code,
        0
    );
    let run = kit.run(&[
        "adapter",
        "enable",
        "rt-two",
        "--accept",
        &two.fingerprint(),
    ]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("rt-one") && run.stderr.contains("demo"),
        "the first provider is named: {}",
        run.stderr
    );
    assert_eq!(info_json(&kit, "rt-two")["enabled"], false);
    // after the first is disabled the second can be enabled
    assert_eq!(kit.run(&["adapter", "disable", "rt-one"]).code, 0);
    assert_eq!(
        kit.run(&[
            "adapter",
            "enable",
            "rt-two",
            "--accept",
            &two.fingerprint()
        ])
        .code,
        0
    );
    drop(daemon);
}

#[test]
fn enabled_adapters_may_not_ask_for_more_than_a_gibibyte_together() {
    let big = |name: &str| {
        Pkg::tool(name, "0.1.0")
            .perms(json!({"memory_mb": 400}))
            .package()
    };
    let (a, b, c) = (big("big-a"), big("big-b"), big("big-c"));
    let kit = kit_with(&[&a, &b, &c]);
    let daemon = kit.daemon();
    for (n, p) in [("big-a", &a), ("big-b", &b), ("big-c", &c)] {
        import(&kit, n);
        let run = enable_with_fingerprint(&kit, p);
        if n == "big-c" {
            assert_eq!(run.code, 1, "1200 MiB over three: {}", run.all());
            assert!(
                run.stderr.contains("1200 MiB") && run.stderr.contains("1024 MiB"),
                "{}",
                run.stderr
            );
            assert_eq!(info_json(&kit, n)["enabled"], false);
        } else {
            assert_eq!(run.code, 0, "{n}: {}", run.all());
        }
    }
    drop(daemon);
}

#[test]
fn a_ninth_service_is_refused() {
    let kit = Kit::new();
    let pkgs: Vec<Packaged> = (1..=9)
        .map(|i| {
            Pkg::service(&format!("svc-{i}"), "0.1.0")
                .perms(json!({"memory_mb": 16}))
                .package()
        })
        .collect();
    let refs: Vec<&Packaged> = pkgs.iter().collect();
    kit.publish(&refs, &IndexOpts::default());
    let daemon = kit.daemon();
    for (i, p) in pkgs.iter().enumerate() {
        import(&kit, &p.name);
        let run = enable_with_fingerprint(&kit, p);
        if i < 8 {
            assert_eq!(run.code, 0, "{}: {}", p.name, run.all());
        } else {
            assert_eq!(run.code, 1, "{}", run.all());
            assert!(
                run.stderr.contains("E_ADAPTER_BUSY") && run.stderr.contains("8 services"),
                "{}",
                run.stderr
            );
        }
    }
    drop(daemon);
}

// ---- the OS layer at enable ------------------------------------------------------------------

/// A copy of the daemon in a directory with no sandbox launcher next to it.
fn daemon_without_launcher(kit: &Kit) -> DaemonProc {
    let dir = kit.dir.path().join("bare-bin");
    std::fs::create_dir_all(&dir).unwrap();
    let copy = dir.join("oxidepmd");
    std::fs::copy(env!("CARGO_BIN_EXE_oxidepmd"), &copy).unwrap();
    let env = kit.daemon_env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    DaemonProc::spawn_binary(&copy, &kit.home, &env)
}

#[test]
fn requires_enforcement_refuses_enable_when_not_enforced() {
    let net = Pkg::tool("netless", "0.1.0")
        .perms(json!({"requires_enforcement": ["network"]}))
        .package();
    let kit = kit_with(&[&net]);
    let daemon = daemon_without_launcher(&kit);
    import(&kit, "netless");
    let run = kit.run(&[
        "adapter",
        "enable",
        "netless",
        "--accept",
        &net.fingerprint(),
    ]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("network") && run.stderr.contains("--allow-unenforced"),
        "the error names the item: {}",
        run.stderr
    );
    assert_eq!(info_json(&kit, "netless")["enabled"], false);
    // --yes does not stand in for it either
    let yes = kit.run(&["adapter", "enable", "netless", "--yes"]);
    assert_eq!(yes.code, 1, "{}", yes.all());
    drop(daemon);
}

#[test]
fn allow_unenforced_is_recorded_in_the_grant() {
    let net = Pkg::tool("netless", "0.1.0")
        .perms(json!({"requires_enforcement": ["network"]}))
        .package();
    let kit = kit_with(&[&net]);
    let daemon = daemon_without_launcher(&kit);
    import(&kit, "netless");
    let run = kit.run(&[
        "adapter",
        "enable",
        "netless",
        "--accept",
        &net.fingerprint(),
        "--allow-unenforced",
    ]);
    assert_eq!(run.code, 0, "{}", run.all());
    let info = info_json(&kit, "netless");
    assert_eq!(info["enforced"]["allow_unenforced"], true, "{info}");
    assert_eq!(
        info["enforced"]["enforced"],
        json!([]),
        "nothing is enforced without the launcher: {info}"
    );
    // `list` shows it
    let list = kit.run(&["adapter", "list"]);
    assert!(
        list.stdout.contains("--allow-unenforced"),
        "{}",
        list.stdout
    );
    drop(daemon);
}

#[test]
fn info_says_which_af_unix_services_the_self_test_reached() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let run = kit.run(&["adapter", "enable", "alpha", "--accept", &a.fingerprint()]);
    assert_eq!(run.code, 0, "{}", run.all());
    let record = info_json(&kit, "alpha")["enforced"].clone();
    let reached = record["unix_reachable"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let info = kit.run(&["adapter", "info", "alpha"]);
    assert_eq!(info.code, 0, "{}", info.all());
    if reached.is_empty() {
        assert!(
            !info.stdout.contains("AF_UNIX services reachable"),
            "{}",
            info.stdout
        );
    } else {
        assert!(
            info.stdout.contains("AF_UNIX services reachable"),
            "{}",
            info.stdout
        );
    }
    drop(daemon);
}

#[test]
fn a_bundled_adapter_can_enable_with_reachable_unix_services() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    sign_bundle(&kit);
    let state = oxidepm_core::Paths::new(kit.home.clone()).adapter_state("alpha");
    let _daemon = kit.daemon_with(&[("TMPDIR", state.to_str().unwrap())]);
    let run = kit.run(&[
        "adapter",
        "import",
        "--bundled",
        "alpha",
        "--enable",
        "--yes",
    ]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("AF_UNIX services reachable:")
            && run.stdout.contains("pathname_socket"),
        "{}",
        run.all()
    );
    let info = info_json(&kit, "alpha");
    assert_eq!(info["enabled"], true, "{info}");
    assert_eq!(info["trust"], "bundled:verified", "{info}");
    assert_eq!(info["enforced"]["allow_unenforced"], false, "{info}");
}

#[test]
fn a_third_party_adapter_can_enable_with_yes_when_unix_services_are_blocked() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let _daemon = kit.daemon_with(&[("TMPDIR", kit.dir.path().to_str().unwrap())]);
    import(&kit, "alpha");
    let run = kit.run(&["adapter", "enable", "alpha", "--yes"]);
    let info = info_json(&kit, "alpha");
    assert_eq!(
        info["trust"],
        format!("signed:{}", kit.key.key_id()),
        "{info}"
    );
    if oxidepm_sandbox::probe().landlock_unix_connect {
        assert_eq!(run.code, 0, "{}", run.all());
        assert_eq!(info["enforced"]["unix_reachable"], json!([]), "{info}");
    } else {
        assert_eq!(run.code, 1, "{}", run.all());
        assert!(run.stderr.contains("pathname_socket"), "{}", run.all());
        assert_eq!(info["enabled"], false, "{info}");
    }
}

// ---- tampering, missing files ------------------------------------------------------------------

#[test]
fn tampered_binary_is_not_executed() {
    let kit = Kit::new();
    let io = kit.io_dir();
    let svc = Pkg::service("svc", "0.1.0").fixture_io(&io).package();
    kit.publish(&[&svc], &IndexOpts::default());
    kit.write_script("svc", json!([]));
    let env = kit.script_env("svc");
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = kit.daemon_with(&env_refs);
    import(&kit, "svc");
    assert_eq!(
        kit.run(&["adapter", "enable", "svc", "--accept", &svc.fingerprint()])
            .code,
        0
    );
    assert_eq!(
        kit.report("svc")
            .iter()
            .filter(|r| r["what"] == "start")
            .count(),
        1
    );
    stop(daemon);

    // the installed executable is changed after the import
    let exe = kit
        .home
        .join("adapters/installed/svc/0.1.0/oxidepm-fixture-adapter");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut bytes = std::fs::read(&exe).unwrap();
    bytes.extend_from_slice(b"tampered");
    std::fs::write(&exe, bytes).unwrap();

    let daemon = kit.daemon_with(&env_refs);
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let state = loop {
        let status = daemon.rpc(json!({"type": "daemon_status"}));
        let adapter = status["adapters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "svc")
            .cloned()
            .unwrap();
        if adapter["state"] == "failed" || std::time::Instant::now() > deadline {
            break adapter;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(state["state"], "failed", "{state}");
    assert!(
        state["note"].as_str().unwrap().contains("tampered"),
        "{state}"
    );
    assert_eq!(
        kit.report("svc")
            .iter()
            .filter(|r| r["what"] == "start")
            .count(),
        1,
        "the changed file was never run"
    );
    drop(daemon);
}

#[test]
fn missing_binary_after_restore_shows_needs_reinstall() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    stop(daemon);
    // the row is there, the files are not (a restore brings back the database only)
    std::fs::remove_dir_all(kit.home.join("adapters/installed/alpha")).unwrap();
    let daemon = kit.daemon();
    let list = kit.run(&["adapter", "list"]);
    assert_eq!(list.code, 0, "{}", list.all());
    assert!(list.stdout.contains("needs_reinstall"), "{}", list.stdout);
    assert!(
        list.stdout.contains("import alpha again"),
        "the command is in the message: {}",
        list.stdout
    );
    let info = info_json(&kit, "alpha");
    assert_eq!(info["state"], "needs_reinstall");
    let enable = kit.run(&["adapter", "enable", "alpha", "--yes"]);
    assert_eq!(enable.code, 1, "{}", enable.all());
    assert!(
        enable.stderr.contains("E_ADAPTER_MISSING")
            && enable.stderr.contains("oxidepm adapter import alpha"),
        "{}",
        enable.stderr
    );
    drop(daemon);
}

#[test]
fn info_stderr_tail_strips_escape_sequences() {
    let kit = Kit::new();
    let io = kit.io_dir();
    let svc = Pkg::service("svc", "0.1.0").fixture_io(&io).package();
    kit.publish(&[&svc], &IndexOpts::default());
    kit.write_script("svc", json!([{"on": "initialize", "do": ["reply", {"stderr_text": {"text": "\u{1b}[2K\u{1b}[1Aall is fine\u{9b}2J"}}]}]));
    let env = kit.script_env("svc");
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = kit.daemon_with(&env_refs);
    import(&kit, "svc");
    assert_eq!(
        kit.run(&["adapter", "enable", "svc", "--accept", &svc.fingerprint()])
            .code,
        0
    );
    // wait for the line to arrive in the tail
    let mut text = String::new();
    for _ in 0..50 {
        text = kit.run(&["adapter", "info", "svc"]).stdout;
        if text.contains("all is fine") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(text.contains("all is fine"), "the tail is shown: {text}");
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{9b}'),
        "no escape sequence reaches the terminal: {text:?}"
    );
    assert!(text.contains('\u{FFFD}'), "they were replaced: {text:?}");
    drop(daemon);
}

// ---- remove ----------------------------------------------------------------------------------

#[test]
fn remove_deletes_the_installed_tree() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(enable_with_fingerprint(&kit, &a).code, 0);
    let run = kit.run(&["adapter", "remove", "alpha"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        !kit.home.join("adapters/installed/alpha").exists(),
        "nothing is left, and the read-only files did not stop the deletion"
    );
    assert!(names_listed(&kit).is_empty());
    let again = kit.run(&["adapter", "info", "alpha"]);
    // not installed any more: the index entry is shown instead
    assert!(again.stdout.contains("Releases:"), "{}", again.all());
    drop(daemon);
}

#[test]
fn remove_keeps_state_unless_purge() {
    let kit = Kit::new();
    let (one, two) = (
        Pkg::tool("one", "0.1.0").package(),
        Pkg::tool("two", "0.1.0").package(),
    );
    kit.publish(&[&one, &two], &IndexOpts::default());
    let daemon = kit.daemon();
    for (name, pkg) in [("one", &one), ("two", &two)] {
        import(&kit, name);
        assert_eq!(enable_with_fingerprint(&kit, pkg).code, 0, "{}", pkg.name);
        let state = kit.home.join("adapters/state").join(name);
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("token.txt"), "a secret").unwrap();
    }
    let keep = kit.run(&["adapter", "remove", "one"]);
    assert_eq!(keep.code, 0, "{}", keep.all());
    assert!(
        keep.stdout.contains("state directory") && keep.stdout.contains("--purge"),
        "{}",
        keep.stdout
    );
    assert!(
        kit.home.join("adapters/state/one/token.txt").exists(),
        "the state is kept"
    );
    let purge = kit.run(&["adapter", "remove", "two", "--purge"]);
    assert_eq!(purge.code, 0, "{}", purge.all());
    assert!(
        !kit.home.join("adapters/state/two").exists(),
        "the state is gone"
    );
    drop(daemon);
}

// ---- human only, dry run -----------------------------------------------------------------------

#[test]
fn adapter_requests_are_a_persons_only() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let requests = [
        json!({"type": "adapter", "op": "enable", "name": "alpha", "yes": true}),
        json!({"type": "adapter", "op": "disable", "name": "alpha"}),
        json!({"type": "adapter", "op": "remove", "name": "alpha", "purge": true}),
        json!({"type": "adapter", "op": "install", "staging": "x", "source": "pinned:x"}),
        json!({"type": "adapter", "op": "index_update", "index_bytes": "{}", "index_sig": ""}),
    ];
    for (i, request) in requests.iter().enumerate() {
        // a claim of being an AI, and a connection on the agent socket
        let claimed = daemon.rpc_on(
            &daemon.socket(),
            &format!("ai-{i}"),
            Some("ai:test"),
            request.clone(),
        );
        assert_eq!(
            claimed["response"]["code"], "E_POLICY_DENIED",
            "{request}: {claimed}"
        );
        let agent = daemon.rpc_on(
            &daemon.agent_socket(),
            &format!("ag-{i}"),
            None,
            request.clone(),
        );
        assert_eq!(
            agent["response"]["code"], "E_POLICY_DENIED",
            "{request}: {agent}"
        );
    }
    // nothing changed; the reads are open to an AI
    assert_eq!(info_json(&kit, "alpha")["enabled"], false);
    let read = daemon.rpc_on(
        &daemon.agent_socket(),
        "read",
        None,
        json!({"type": "adapter", "op": "list"}),
    );
    assert_eq!(read["response"]["type"], "adapter", "{read}");
    assert_eq!(read["response"]["adapters"][0]["name"], "alpha", "{read}");
    drop(daemon);
}

#[test]
fn what_an_adapter_wrote_to_stderr_is_not_shown_to_an_ai() {
    let kit = Kit::new();
    let io = kit.io_dir();
    let svc = Pkg::service("svc", "0.1.0").fixture_io(&io).package();
    kit.publish(&[&svc], &IndexOpts::default());
    kit.write_script("svc", json!([{"on": "initialize", "do": ["reply", {"stderr_text": {"text": "token=hunter2-from-stderr"}}]}]));
    let env = kit.script_env("svc");
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = kit.daemon_with(&env_refs);
    import(&kit, "svc");
    assert_eq!(
        kit.run(&["adapter", "enable", "svc", "--accept", &svc.fingerprint()])
            .code,
        0
    );
    let shown = (0..50)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            let tail = info_json(&kit, "svc")["stderr_tail"].to_string();
            tail.contains("hunter2").then_some(tail)
        })
        .expect("a person sees the tail");
    assert!(shown.contains("token=hunter2-from-stderr"));
    let ai = daemon.rpc_on(
        &daemon.agent_socket(),
        "ai-info",
        None,
        json!({"type": "adapter", "op": "info", "name": "svc"}),
    );
    assert_eq!(ai["response"]["type"], "adapter", "{ai}");
    assert_eq!(ai["response"]["adapter"]["name"], "svc");
    assert!(
        !ai.to_string().contains("hunter2"),
        "an AI does not get the adapter's stderr: {ai}"
    );
    let list = daemon.rpc_on(
        &daemon.agent_socket(),
        "ai-list",
        None,
        json!({"type": "adapter", "op": "list"}),
    );
    assert!(!list.to_string().contains("hunter2"), "{list}");
    drop(daemon);
}

#[test]
fn bundled_import_can_enable_the_pure_adapters_at_once() {
    let a = Pkg::tool("rt-a", "0.1.0").package();
    let b = Pkg::tool("rt-b", "0.1.0").package();
    let kit = kit_with(&[&a, &b]);
    sign_bundle(&kit);
    let daemon = kit.daemon();
    let mut cmd = kit.cli();
    cmd.env("OXIDEPM_TEST_INDEX_URL", "").args([
        "adapter",
        "import",
        "--bundled",
        "--tag",
        "test",
        "--enable",
        "--yes",
    ]);
    let out = cmd.output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    for name in ["rt-a", "rt-b"] {
        let info = info_json(&kit, name);
        assert_eq!(info["enabled"], true, "{name}: {info}");
        assert_eq!(info["source"], "bundled");
    }
    let listed = kit.run(&["adapter", "list"]);
    assert_eq!(listed.code, 0, "{}", listed.all());
    for name in ["rt-a", "rt-b"] {
        assert!(
            listed
                .stdout
                .lines()
                .any(|line| line.starts_with(name) && line.ends_with("bundled with this release")),
            "{}",
            listed.all()
        );
    }
    drop(daemon);
}

#[test]
fn bundled_runtime_import_can_enable_with_yes() {
    let runtime = Pkg::tool("runtime-demo", "0.1.0")
        .with("tags", json!(["runtime"]))
        .with(
            "provides",
            json!({"runtimes": ["demo"], "programs": ["demo"]}),
        )
        .package();
    let kit = Kit::new();
    kit.publish(
        &[&runtime],
        &IndexOpts {
            tags: &["runtime"],
            ..IndexOpts::default()
        },
    );
    sign_bundle(&kit);
    let _daemon = kit.daemon();
    let run = kit.run(&[
        "adapter",
        "import",
        "--bundled",
        "--tag",
        "runtime",
        "--enable",
        "--yes",
    ]);
    assert_eq!(run.code, 0, "{}", run.all());
    let info = info_json(&kit, "runtime-demo");
    assert_eq!(info["trust"], "bundled:verified", "{info}");
    assert_eq!(info["enabled"], true, "{info}");
}

#[test]
fn dry_run_enable_disable_and_remove_change_nothing() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let before = (
        names_listed(&kit),
        info_json(&kit, "alpha"),
        kit.adapters_tree(),
    );

    let enable = kit.run(&["--dry-run", "adapter", "enable", "alpha", "--yes"]);
    assert_eq!(enable.code, 0, "{}", enable.all());
    assert!(
        enable.stdout.contains("Dry run of 'adapter enable'")
            && enable.stdout.contains("grant")
            && enable.stdout.contains("level read"),
        "{}",
        enable.stdout
    );
    let remove = kit.run(&["--dry-run", "adapter", "remove", "alpha", "--purge"]);
    assert_eq!(remove.code, 0, "{}", remove.all());
    assert!(
        remove.stdout.contains("delete") && remove.stdout.contains("delete_state"),
        "{}",
        remove.stdout
    );
    let disable = kit.run(&["--dry-run", "adapter", "disable", "alpha"]);
    assert_eq!(disable.code, 0, "{}", disable.all());

    let after = (
        names_listed(&kit),
        info_json(&kit, "alpha"),
        kit.adapters_tree(),
    );
    assert_eq!(before, after);
    // a rehearsal of a command that only reads is refused
    let read = kit.run(&["--dry-run", "adapter", "list"]);
    assert_eq!(read.code, 2, "{}", read.all());
    drop(daemon);
}

// ---- boot housekeeping -------------------------------------------------------------------------

#[test]
fn old_staging_entries_are_removed_at_daemon_start() {
    let kit = Kit::new();
    let staging = kit.home.join("adapters/staging");
    std::fs::create_dir_all(&staging).unwrap();
    for dir in [kit.home.join("adapters"), staging.clone()] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    for name in ["old", "fresh"] {
        std::fs::create_dir(staging.join(name)).unwrap();
        std::fs::write(staging.join(name).join("file"), "x").unwrap();
    }
    let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::open(staging.join("old"))
        .unwrap()
        .set_modified(hour_ago)
        .unwrap();
    let daemon = kit.daemon();
    daemon.wait_for_boot();
    assert_eq!(
        entries(&staging),
        vec!["fresh"],
        "entries older than 15 minutes are gone, a fresh one stays"
    );
    drop(daemon);
}

#[test]
fn unreferenced_installed_dirs_are_deleted_at_boot() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    stop(daemon);
    let installed = kit.home.join("adapters/installed");
    for stale in ["alpha/9.9.9", "ghost/1.0.0", ".incoming-1-1"] {
        std::fs::create_dir_all(installed.join(stale)).unwrap();
        std::fs::write(installed.join(stale).join("file"), "x").unwrap();
    }
    std::fs::create_dir_all(kit.home.join("adapters/trash/old-1.0.0")).unwrap();
    let daemon = kit.daemon();
    daemon.wait_for_boot();
    assert_eq!(entries(&installed), vec!["alpha"]);
    assert_eq!(
        entries(&installed.join("alpha")),
        vec!["0.1.0"],
        "the version a row names stays"
    );
    assert!(entries(&kit.home.join("adapters/trash")).is_empty());
    drop(daemon);
}

// ---- backup and restore ------------------------------------------------------------------------

fn archive_members(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).unwrap();
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut names: Vec<String> = tar
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().display().to_string())
        .collect();
    names.sort();
    names
}

#[test]
fn backup_has_no_adapter_state() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(enable_with_fingerprint(&kit, &a).code, 0);
    let state = kit.home.join("adapters/state/alpha");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("token.txt"), "TOKEN-123").unwrap();
    stop(daemon);
    let archive = kit.dir.path().join("backup.tar.gz");
    let run = kit.run(&["backup", "--output", archive.to_str().unwrap()]);
    assert_eq!(run.code, 0, "{}", run.all());
    let members = archive_members(&archive);
    assert!(members.contains(&"adapters.db".to_string()), "{members:?}");
    assert!(
        members
            .iter()
            .all(|m| !m.contains("adapters/") && !m.contains("token") && !m.contains("installed")),
        "no state, no executable: {members:?}"
    );
    let mut text = String::new();
    for name in &members {
        let file = std::fs::File::open(&archive).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
        for entry in tar.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().display().to_string() == *name {
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    assert!(
        !text.is_empty() && !text.contains("TOKEN-123"),
        "the database is in the archive and the token is not"
    );
}

#[test]
fn restore_disables_adapters_and_clears_grants() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(enable_with_fingerprint(&kit, &a).code, 0);
    assert_eq!(info_json(&kit, "alpha")["enabled"], true);
    stop(daemon);
    let archive = kit.dir.path().join("backup.tar.gz");
    assert_eq!(
        kit.run(&["backup", "--output", archive.to_str().unwrap()])
            .code,
        0
    );
    let restore = kit.run(&["restore", archive.to_str().unwrap(), "--yes"]);
    assert_eq!(restore.code, 0, "{}", restore.all());
    // the restored rows: disabled, no grant, nothing running after a boot
    let daemon = kit.daemon();
    let info = info_json(&kit, "alpha");
    assert_eq!(info["enabled"], false, "{info}");
    assert!(info["grant"].is_null(), "{info}");
    assert_eq!(info["state"], "disabled");
    // and enabling again needs consent again
    assert_eq!(kit.run(&["adapter", "enable", "alpha"]).code, 2);
    drop(daemon);
}

#[test]
fn restore_keeps_higher_index_sequence() {
    let a = alpha();
    let kit = Kit::new();
    kit.publish(
        &[&a],
        &IndexOpts {
            sequence: 42,
            ..IndexOpts::default()
        },
    );
    let daemon = kit.daemon();
    import(&kit, "alpha");
    stop(daemon);
    let archive = kit.dir.path().join("backup.tar.gz");
    assert_eq!(
        kit.run(&["backup", "--output", archive.to_str().unwrap()])
            .code,
        0
    );

    // later a newer index is seen
    kit.publish(
        &[&a],
        &IndexOpts {
            sequence: 60,
            ..IndexOpts::default()
        },
    );
    let daemon = kit.daemon();
    assert_eq!(kit.run(&["adapter", "list", "--available"]).code, 0);
    stop(daemon);

    assert_eq!(
        kit.run(&["restore", archive.to_str().unwrap(), "--yes"])
            .code,
        0
    );
    let daemon = kit.daemon();
    // the older index (42) the backup held did not lower the mark: 55 is stale
    kit.publish(
        &[&a],
        &IndexOpts {
            sequence: 55,
            ..IndexOpts::default()
        },
    );
    let run = kit.run(&["adapter", "list", "--available"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("E_INDEX_STALE") && run.stderr.contains("60"),
        "{}",
        run.stderr
    );
    drop(daemon);
}

// ---- revocation --------------------------------------------------------------------------------

#[test]
fn revoked_release_is_disabled_when_the_index_is_read() {
    let a = alpha();
    let kit = Kit::new();
    kit.publish(
        &[&a],
        &IndexOpts {
            sequence: 42,
            ..IndexOpts::default()
        },
    );
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(enable_with_fingerprint(&kit, &a).code, 0);
    assert_eq!(info_json(&kit, "alpha")["enabled"], true);

    // the next index revokes this executable
    kit.publish(
        &[&a],
        &IndexOpts {
            sequence: 43,
            revoked: vec![a.exe_sha.clone()],
            ..IndexOpts::default()
        },
    );
    let run = kit.run(&["adapter", "list", "--available"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stderr.contains("alpha") && run.stderr.contains("revokes"),
        "{}",
        run.stderr
    );

    let info = info_json(&kit, "alpha");
    assert_eq!(info["enabled"], false, "{info}");
    assert!(info["note"].as_str().unwrap().contains("revoked"), "{info}");
    let status = daemon.rpc(json!({"type": "daemon_status"}));
    let warnings = status["warnings"].to_string();
    assert!(
        warnings.contains("alpha") && warnings.contains("a leaked credential"),
        "{warnings}"
    );
    // an audit line was written
    let audit = std::fs::read_to_string(kit.home.join("audit/audit.jsonl")).unwrap_or_default()
        + &std::fs::read_to_string(kit.home.join("audit/audit-security.jsonl")).unwrap_or_default();
    assert!(
        audit.contains("\"event\":\"revoked\"") && audit.contains("alpha"),
        "{audit}"
    );
    // and a revoked executable is not offered again, nor installed
    let again = kit.run(&["--dry-run", "adapter", "import", "alpha"]);
    assert_eq!(again.code, 1, "{}", again.all());
    drop(daemon);
}

#[test]
fn a_cached_revocation_applies_when_the_daemon_starts() {
    let a = alpha();
    let kit = Kit::new();
    kit.publish(
        &[&a],
        &IndexOpts {
            sequence: 42,
            ..IndexOpts::default()
        },
    );
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(enable_with_fingerprint(&kit, &a).code, 0);
    stop(daemon);
    // the index is read while the daemon is down is not possible; so it arrives, the daemon stops,
    // and the adapter's row is made enabled again by hand: the cached index still revokes it
    let daemon = kit.daemon();
    kit.publish(
        &[&a],
        &IndexOpts {
            sequence: 43,
            revoked: vec![a.exe_sha.clone()],
            ..IndexOpts::default()
        },
    );
    assert_eq!(kit.run(&["adapter", "list", "--available"]).code, 0);
    stop(daemon);
    let cached = kit.home.join("adapters/index/index.json");
    assert!(cached.exists(), "the index was kept");
    std::fs::remove_dir_all(cached.parent().unwrap()).unwrap();
    let daemon = kit.daemon();
    daemon.wait_for_boot();
    let status = daemon.rpc(json!({"type": "daemon_status"}));
    assert!(
        status["warnings"].to_string().contains("revokes"),
        "{status}"
    );
    let enable = kit.run(&["adapter", "enable", "alpha", "--yes"]);
    assert_ne!(enable.code, 0, "{}", enable.all());
    assert!(enable.stderr.contains("revok"), "{}", enable.all());
    drop(daemon);
}

// ======================================================================================
// 28c: update, rollback, revocation
// ======================================================================================

fn seq(n: u64) -> IndexOpts {
    IndexOpts {
        sequence: n,
        ..IndexOpts::default()
    }
}

fn version_of(kit: &Kit, name: &str) -> String {
    info_json(kit, name)["version"]
        .as_str()
        .unwrap()
        .to_string()
}

fn run_without_index(kit: &Kit, args: &[&str]) -> Run {
    Run::of(
        kit.cli()
            .env("OXIDEPM_TEST_INDEX_URL", "")
            .args(args)
            .output()
            .unwrap(),
    )
}

#[test]
fn bundled_update_by_name_keeps_an_enabled_adapter_enabled() {
    let old = alpha();
    let new = Pkg::tool("alpha", "0.2.0").package();
    let kit = kit_with(&[&old]);
    let _daemon = kit.daemon();
    let import = kit.run(&["adapter", "import", "--bundled", "alpha"]);
    assert_eq!(import.code, 0, "{}", import.all());
    let enable = enable_with_fingerprint(&kit, &old);
    assert_eq!(enable.code, 0, "{}", enable.all());

    kit.publish(&[&new], &seq(2));
    sign_bundle(&kit);
    let run = run_without_index(&kit, &["adapter", "update", "alpha"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("Updated alpha 0.1.0 to 0.2.0"),
        "{}",
        run.all()
    );
    let info = info_json(&kit, "alpha");
    assert_eq!(info["version"], "0.2.0");
    assert_eq!(info["prev_version"], "0.1.0");
    assert_eq!(info["source"], "bundled");
    assert_eq!(info["trust"], "bundled:verified");
    assert_eq!(info["enabled"], true);
}

#[test]
fn bundled_update_all_continues_past_an_adapter_that_needs_consent() {
    let old_alpha = alpha();
    let new_alpha = Pkg::tool("alpha", "0.2.0")
        .perms(json!({"network": {"mode": "outbound"}}))
        .package();
    let old_beta = Pkg::tool("beta", "0.1.0").package();
    let new_beta = Pkg::tool("beta", "0.2.0").package();
    let kit = kit_with(&[&old_alpha, &old_beta]);
    let _daemon = kit.daemon();
    let import = kit.run(&["adapter", "import", "--bundled"]);
    assert_eq!(import.code, 0, "{}", import.all());
    for pkg in [&old_alpha, &old_beta] {
        let enable = enable_with_fingerprint(&kit, pkg);
        assert_eq!(enable.code, 0, "{}", enable.all());
    }

    kit.publish(&[&new_alpha, &new_beta], &seq(2));
    let run = run_without_index(&kit, &["adapter", "update", "--all"]);
    assert_eq!(run.code, 1, "{}", run.all());
    let fingerprint = new_alpha.fingerprint();
    assert!(
        run.stderr.contains(&format!(
            "oxidepm adapter update alpha --accept {fingerprint}"
        )) && run.stderr.contains("Not updated: alpha"),
        "{}",
        run.all()
    );
    assert!(
        run.stdout.contains("Updated beta 0.1.0 to 0.2.0"),
        "{}",
        run.all()
    );
    assert_eq!(version_of(&kit, "alpha"), "0.1.0");
    assert_eq!(version_of(&kit, "beta"), "0.2.0");
    assert_eq!(info_json(&kit, "alpha")["enabled"], true);
    assert_eq!(info_json(&kit, "beta")["enabled"], true);

    let accepted = run_without_index(
        &kit,
        &["adapter", "update", "alpha", "--accept", &fingerprint],
    );
    assert_eq!(accepted.code, 0, "{}", accepted.all());
    let info = info_json(&kit, "alpha");
    assert_eq!(info["version"], "0.2.0");
    assert_eq!(info["grant"]["network"]["mode"], "outbound");
    assert_eq!(info["enabled"], true);
}

#[test]
fn bundled_update_dry_run_verifies_the_archive_and_changes_nothing() {
    let old = alpha();
    let new = Pkg::tool("alpha", "0.2.0").package();
    let kit = kit_with(&[&old]);
    let _daemon = kit.daemon();
    let import = kit.run(&["adapter", "import", "--bundled", "alpha"]);
    assert_eq!(import.code, 0, "{}", import.all());
    kit.publish(&[&new], &seq(2));
    let before = (info_json(&kit, "alpha"), kit.adapters_tree());
    let args = ["--dry-run", "adapter", "update", "alpha"];
    let run = run_without_index(&kit, &args);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("Update    from 0.1.0 to 0.2.0")
            && run.stdout.contains("Dry run: verified"),
        "{}",
        run.all()
    );
    assert_eq!((info_json(&kit, "alpha"), kit.adapters_tree()), before);

    std::fs::write(kit.bundle.join("alpha-0.2.0.tar.gz"), b"changed archive").unwrap();
    let run = run_without_index(&kit, &args);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("E_CHECKSUM_MISMATCH"), "{}", run.all());
    assert_eq!((info_json(&kit, "alpha"), kit.adapters_tree()), before);
}

#[test]
fn bundled_update_reports_when_the_bundle_has_no_newer_release() {
    let old = alpha();
    let new = Pkg::tool("alpha", "0.2.0").package();
    let kit = kit_with(&[&new]);
    let _daemon = kit.daemon();
    let import = kit.run(&["adapter", "import", "--bundled", "alpha"]);
    assert_eq!(import.code, 0, "{}", import.all());

    for releases in [&[&new][..], &[&old][..], &[]] {
        kit.publish(releases, &seq(2));
        let run = run_without_index(&kit, &["adapter", "update", "alpha"]);
        assert_eq!(run.code, 0, "{}", run.all());
        assert!(run.stdout.contains("alpha is up to date"), "{}", run.all());
        assert_eq!(version_of(&kit, "alpha"), "0.2.0");
    }
}

#[test]
fn pinned_update_does_not_use_a_matching_bundle() {
    let old = alpha();
    let new = Pkg::tool("alpha", "0.2.0").package();
    let kit = kit_with(&[&new]);
    let _daemon = kit.daemon();
    let archive = kit.dir.path().join("local.tar.gz");
    std::fs::write(&archive, &old.archive).unwrap();
    let import = kit.run(&[
        "adapter",
        "import",
        archive.to_str().unwrap(),
        "--sha256",
        &old.archive_sha,
    ]);
    assert_eq!(import.code, 0, "{}", import.all());
    let run = run_without_index(&kit, &["adapter", "update", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("no index is configured"),
        "{}",
        run.all()
    );
    let info = info_json(&kit, "alpha");
    assert_eq!(info["version"], "0.1.0");
    assert_eq!(info["trust"], "pinned");
}

#[test]
fn update_with_more_permissions_needs_consent() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0")
        .perms(json!({"network": {"mode": "outbound", "hosts": ["api.example.org"]}}))
        .package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(enable_with_fingerprint(&kit, &v1).code, 0);

    kit.publish(&[&v1, &v2], &seq(2));
    let run = kit.run(&["adapter", "update", "alpha"]);
    assert_eq!(run.code, 2, "{}", run.all());
    assert!(
        run.stdout.contains("[GROWS]") && run.stdout.contains("network.mode"),
        "the preview marks what grew: {}",
        run.stdout
    );
    assert!(
        run.stderr
            .contains(&format!("--accept {}", v2.fingerprint())),
        "{}",
        run.stderr
    );
    // not applied: the old version stays, enabled, with its files
    assert_eq!(version_of(&kit, "alpha"), "0.1.0");
    assert_eq!(info_json(&kit, "alpha")["enabled"], true);
    assert!(!kit.home.join("adapters/installed/alpha/0.2.0").exists());

    // a fingerprint that is not the new release's is refused by the daemon as well
    let wrong = kit.run(&["adapter", "update", "alpha", "--accept", &v1.fingerprint()]);
    assert_eq!(wrong.code, 1, "{}", wrong.all());
    assert_eq!(version_of(&kit, "alpha"), "0.1.0");

    let ok = kit.run(&["adapter", "update", "alpha", "--accept", &v2.fingerprint()]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert!(
        ok.stdout.contains("Updated alpha 0.1.0 to 0.2.0"),
        "{}",
        ok.stdout
    );
    let info = info_json(&kit, "alpha");
    assert_eq!(info["version"], "0.2.0");
    assert_eq!(info["prev_version"], "0.1.0");
    assert_eq!(
        info["grant"]["network"]["mode"], "outbound",
        "the accepted release's permissions are the grant: {info}"
    );
    assert_eq!(info["enabled"], true);
    drop(daemon);
}

#[test]
fn an_update_cannot_exceed_the_enabled_memory_limit() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0")
        .perms(json!({"memory_mb": 256}))
        .package();
    let b = Pkg::tool("beta", "0.1.0")
        .perms(json!({"memory_mb": 400}))
        .package();
    let c = Pkg::tool("gamma", "0.1.0")
        .perms(json!({"memory_mb": 400}))
        .package();
    let kit = Kit::new();
    kit.publish(&[&v1, &b, &c], &seq(1));
    let daemon = kit.daemon();
    for pkg in [&v1, &b, &c] {
        import(&kit, &pkg.name);
        let enable = enable_with_fingerprint(&kit, pkg);
        assert_eq!(enable.code, 0, "{}", enable.all());
    }
    kit.publish(&[&v1, &v2, &b, &c], &seq(2));
    let update = kit.run(&["adapter", "update", "alpha", "--accept", &v2.fingerprint()]);
    assert_ne!(update.code, 0, "{}", update.all());
    assert!(update.stderr.contains("1024 MiB"), "{}", update.all());
    assert_eq!(version_of(&kit, "alpha"), "0.1.0");
    drop(daemon);
}

#[test]
fn update_with_fewer_permissions_reduces_the_grant() {
    let v1 = Pkg::tool("alpha", "0.1.0")
        .perms(json!({"network": {"mode": "outbound", "hosts": ["api.example.org"]}}))
        .package();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(
        kit.run(&["adapter", "enable", "alpha", "--accept", &v1.fingerprint()])
            .code,
        0
    );
    let mut expected = info_json(&kit, "alpha")["grant"].clone();
    expected["network"] = json!({"mode": "none", "hosts": [], "listen": []});

    kit.publish(&[&v1, &v2], &seq(2));
    let run = kit.run(&["adapter", "update", "alpha"]);
    assert_eq!(run.code, 0, "no consent is needed for less: {}", run.all());
    let info = info_json(&kit, "alpha");
    assert_eq!(info["version"], "0.2.0");
    assert_eq!(
        info["grant"], expected,
        "the grant follows the narrower declaration"
    );
    assert_eq!(info["enabled"], true);
    // the fingerprint of the grant now names the new release
    assert_eq!(info["fingerprint"], v2.fingerprint());
    drop(daemon);
}

#[test]
fn rollback_restores_the_previous_enforcement_decision() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(enable_with_fingerprint(&kit, &v1).code, 0);
    assert_eq!(
        info_json(&kit, "alpha")["enforced"]["allow_unenforced"],
        false
    );
    kit.publish(&[&v1, &v2], &seq(2));
    let update = kit.run(&["adapter", "update", "alpha"]);
    assert_eq!(update.code, 0, "{}", update.all());
    let enable = kit.run(&["adapter", "enable", "alpha", "--yes", "--allow-unenforced"]);
    assert_eq!(enable.code, 0, "{}", enable.all());
    assert_eq!(
        info_json(&kit, "alpha")["enforced"]["allow_unenforced"],
        true
    );
    let rollback = kit.run(&["adapter", "update", "alpha", "--rollback"]);
    assert_eq!(rollback.code, 0, "{}", rollback.all());
    let info = info_json(&kit, "alpha");
    assert_eq!(info["version"], "0.1.0");
    assert_eq!(info["enforced"]["allow_unenforced"], false);
    drop(daemon);
}

#[test]
fn rollback_refuses_a_revoked_previous_release() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    kit.publish(&[&v1, &v2], &seq(2));
    let update = kit.run(&["adapter", "update", "alpha"]);
    assert_eq!(update.code, 0, "{}", update.all());
    kit.publish(
        &[&v2],
        &IndexOpts {
            revoked: vec![v1.exe_sha.clone()],
            ..seq(3)
        },
    );
    assert_eq!(kit.run(&["adapter", "list", "--available"]).code, 0);
    kit.publish(&[&v2], &seq(4));
    assert_eq!(kit.run(&["adapter", "list", "--available"]).code, 0);
    let rollback = kit.run(&["adapter", "update", "alpha", "--rollback"]);
    assert_ne!(rollback.code, 0, "{}", rollback.all());
    assert!(rollback.stderr.contains("revok"), "{}", rollback.all());
    stage_by_hand(&kit, &v1, "pinned");
    let pinned = raw_install(&daemon, "pinned", "pinned:archive.tar.gz", None);
    assert!(
        pinned["message"]
            .as_str()
            .is_some_and(|message| message.contains("revok")),
        "{pinned}"
    );
    assert_eq!(version_of(&kit, "alpha"), "0.2.0");
    drop(daemon);
}

#[test]
fn update_that_fails_probation_rolls_back() {
    let kit = Kit::new();
    let io = kit.io_dir();
    let v1 = Pkg::service("svc", "0.1.0").fixture_io(&io).package();
    let v2 = Pkg::service("svc", "0.2.0").fixture_io(&io).package();
    let watcher = Pkg::service("watcher", "0.1.0").fixture_io(&io).perms(json!({
        "env_pass": ["FIXTURE_SCRIPT", "FIXTURE_REPORT"],
        "events": ["adapter"],
        "fs": {"read": ["$STATE", io.display().to_string()], "write": ["$STATE", io.display().to_string()]}
    })).package();
    kit.publish(&[&v1, &watcher], &seq(1));
    // the new version answers `initialize` as another adapter; the old one is fine. Both adapters
    // read the same script, and a rule is limited to the install path of version 0.2.0.
    kit.write_script(
        "svc",
        json!([{"on": "initialize", "only_in": "/svc/0.2.0/", "do": {"lie_name": {}}}]),
    );
    let env = kit.script_env("svc");
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = kit.daemon_with(&env_refs);
    import(&kit, "svc");
    import(&kit, "watcher");
    assert_eq!(
        kit.run(&[
            "adapter",
            "enable",
            "watcher",
            "--accept",
            &watcher.fingerprint()
        ])
        .code,
        0
    );
    assert_eq!(
        kit.run(&["adapter", "enable", "svc", "--accept", &v1.fingerprint()])
            .code,
        0
    );

    kit.publish(&[&v1, &v2, &watcher], &seq(2));
    let run = kit.run(&["adapter", "update", "svc", "--accept", &v2.fingerprint()]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("did not pass its probation")
            && run.stderr.contains("rolled back to 0.1.0"),
        "{}",
        run.stderr
    );

    // the old version is the version, and it runs
    let info = info_json(&kit, "svc");
    assert_eq!(info["version"], "0.1.0", "{info}");
    assert_eq!(info["enabled"], true, "{info}");
    assert!(
        info["prev_version"].is_null(),
        "the failed version is not kept as a rollback step: {info}"
    );
    let started = std::time::Instant::now();
    while info_json(&kit, "svc")["state"] != "ready" {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the old version runs again: {}",
            info_json(&kit, "svc")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !kit.home.join("adapters/installed/svc/0.2.0").exists(),
        "the failed version's files are gone"
    );
    // an audit line, and an `adapter` event to the service that listens
    let audit = std::fs::read_to_string(kit.home.join("audit/audit.jsonl")).unwrap_or_default()
        + &std::fs::read_to_string(kit.home.join("audit/audit-security.jsonl")).unwrap_or_default();
    assert!(
        audit.contains("\"event\":\"rolled_back\"") && audit.contains("\"name\":\"svc\""),
        "{audit}"
    );
    let mut seen = false;
    for _ in 0..100 {
        // both adapters write the one report the daemon's environment names
        seen = kit.report("svc").iter().any(|r| {
            r["what"] == "frame"
                && r["dir"] == "in"
                && r["frame"]["method"] == "event"
                && r["frame"]["params"]["kind"] == "rolled_back"
        });
        if seen {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(seen, "the watcher was told: {:?}", kit.report("svc"));
    drop(daemon);
}

#[test]
fn update_downgrade_needs_allow_downgrade() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v2], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    // an index that offers 0.1.0 and 0.2.0; the newest is installed, so an update has nothing to do
    kit.publish(&[&v1, &v2], &seq(2));
    let up_to_date = kit.run(&["adapter", "update", "alpha"]);
    assert_eq!(up_to_date.code, 0, "{}", up_to_date.all());
    assert!(
        up_to_date.stdout.contains("up to date"),
        "{}",
        up_to_date.stdout
    );
    // naming an older version is refused without the flag
    let refused = kit.run(&["adapter", "update", "alpha", "--version", "0.1.0"]);
    assert_eq!(refused.code, 1, "{}", refused.all());
    assert!(
        refused.stderr.contains("--allow-downgrade"),
        "{}",
        refused.stderr
    );
    assert_eq!(version_of(&kit, "alpha"), "0.2.0");
    let allowed = kit.run(&[
        "adapter",
        "update",
        "alpha",
        "--version",
        "0.1.0",
        "--allow-downgrade",
    ]);
    assert_eq!(allowed.code, 0, "{}", allowed.all());
    assert_eq!(version_of(&kit, "alpha"), "0.1.0");
    drop(daemon);
}

#[test]
fn import_of_installed_name_behaves_like_update() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v2], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    // the index now offers only an older release: importing the name again is an update, and the
    // downgrade check applies
    kit.publish(&[&v1], &seq(2));
    let run = kit.run(&["adapter", "import", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("older than the installed 0.2.0")
            && run.stderr.contains("--allow-downgrade"),
        "{}",
        run.stderr
    );
    assert_eq!(version_of(&kit, "alpha"), "0.2.0");
    // and the same version again is not a second install
    kit.publish(&[&v2], &seq(3));
    let again = kit.run(&["adapter", "import", "alpha"]);
    assert_eq!(again.code, 1, "{}", again.all());
    assert!(
        again.stderr.contains("installed already"),
        "{}",
        again.stderr
    );
    drop(daemon);
}

#[test]
fn import_of_a_name_whose_files_are_gone_brings_them_back() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    stop(daemon);
    std::fs::remove_dir_all(kit.home.join("adapters/installed/alpha")).unwrap();
    let daemon = kit.daemon();
    assert_eq!(info_json(&kit, "alpha")["state"], "needs_reinstall");
    // the message of the list says to import it again, and that works
    let run = kit.run(&["adapter", "import", "alpha"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("Reinstalled the files of alpha 0.1.0"),
        "{}",
        run.stdout
    );
    let info = info_json(&kit, "alpha");
    assert_eq!(info["state"], "disabled", "{info}");
    assert_eq!(enable_with_fingerprint(&kit, &a).code, 0);
    drop(daemon);
}

#[test]
fn update_from_a_different_source_needs_switch_source() {
    let pinned = Pkg::tool("alpha", "0.1.0").package();
    let signed = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&signed], &seq(1));
    let daemon = kit.daemon();
    // installed from a file you pinned
    let file = kit.dir.path().join("alpha.tar.gz");
    std::fs::write(&file, &pinned.archive).unwrap();
    let first = kit.run(&[
        "adapter",
        "import",
        file.to_str().unwrap(),
        "--sha256",
        &pinned.archive_sha,
    ]);
    assert_eq!(first.code, 0, "{}", first.all());
    assert_eq!(info_json(&kit, "alpha")["trust"], "pinned");

    // the signed index must not silently replace it
    let refused = kit.run(&["adapter", "update", "alpha"]);
    assert_eq!(refused.code, 1, "{}", refused.all());
    assert!(
        refused.stderr.contains("--switch-source") && refused.stderr.contains("pinned by hand"),
        "{}",
        refused.stderr
    );
    assert_eq!(version_of(&kit, "alpha"), "0.1.0");
    // and with the flag it does, and the file you pinned is no rollback step (the table has one
    // trust label per row)
    let switched = kit.run(&["adapter", "update", "alpha", "--switch-source"]);
    assert_eq!(switched.code, 0, "{}", switched.all());
    let info = info_json(&kit, "alpha");
    assert_eq!(info["trust"], format!("signed:{}", kit.key.key_id()));
    assert!(info["prev_version"].is_null(), "{info}");
    assert!(!kit.home.join("adapters/installed/alpha/0.1.0").exists());
    let none = kit.run(&["adapter", "update", "alpha", "--rollback"]);
    assert_eq!(none.code, 1, "{}", none.all());

    // a pinned adapter is updated from a new file with its hash
    let other = Pkg::tool("beta", "0.1.0").package();
    let newer = Pkg::tool("beta", "0.1.1").package();
    for (p, name) in [(&other, "beta-1.tar.gz"), (&newer, "beta-2.tar.gz")] {
        std::fs::write(kit.dir.path().join(name), &p.archive).unwrap();
    }
    let f1 = kit.dir.path().join("beta-1.tar.gz");
    let f2 = kit.dir.path().join("beta-2.tar.gz");
    assert_eq!(
        kit.run(&[
            "adapter",
            "import",
            f1.to_str().unwrap(),
            "--sha256",
            &other.archive_sha
        ])
        .code,
        0
    );
    let no_hash = kit.run(&["adapter", "update", "beta", "--from", f2.to_str().unwrap()]);
    assert_eq!(
        no_hash.code,
        2,
        "a pinned adapter needs the hash of the new archive: {}",
        no_hash.all()
    );
    let ok = kit.run(&[
        "adapter",
        "update",
        "beta",
        "--from",
        f2.to_str().unwrap(),
        "--sha256",
        &newer.archive_sha,
    ]);
    assert_eq!(ok.code, 0, "{}", ok.all());
    assert_eq!(version_of(&kit, "beta"), "0.1.1");
    drop(daemon);
}

#[test]
fn rollback_restores_the_previous_version_without_network() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    assert_eq!(enable_with_fingerprint(&kit, &v1).code, 0);
    kit.publish(&[&v1, &v2], &seq(2));
    assert_eq!(kit.run(&["adapter", "update", "alpha"]).code, 0);
    assert_eq!(version_of(&kit, "alpha"), "0.2.0");

    let hits = kit.http.hits();
    let dry = kit.run(&["--dry-run", "adapter", "update", "alpha", "--rollback"]);
    assert_eq!(dry.code, 0, "{}", dry.all());
    assert!(
        dry.stdout.contains("would go back to 0.1.0"),
        "{}",
        dry.stdout
    );
    assert_eq!(
        version_of(&kit, "alpha"),
        "0.2.0",
        "a rehearsal changes nothing"
    );

    let run = kit.run(&["adapter", "update", "alpha", "--rollback"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert_eq!(kit.http.hits(), hits, "no network was used");
    let info = info_json(&kit, "alpha");
    assert_eq!(info["version"], "0.1.0");
    assert_eq!(info["exe_sha256"], v1.exe_sha);
    assert_eq!(info["fingerprint"], v1.fingerprint());
    assert_eq!(
        info["enabled"], true,
        "it was enabled and its grant came back with it: {info}"
    );
    // the swap: the version that was left is now the rollback step
    assert_eq!(info["prev_version"], "0.2.0");
    assert!(kit.home.join("adapters/installed/alpha/0.2.0").exists());
    let back = kit.run(&["adapter", "update", "alpha", "--rollback"]);
    assert_eq!(back.code, 0, "{}", back.all());
    assert_eq!(version_of(&kit, "alpha"), "0.2.0");
    drop(daemon);
}

#[test]
fn rollback_without_an_earlier_version_is_refused() {
    let a = alpha();
    let kit = kit_with(&[&a]);
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let run = kit.run(&["adapter", "update", "alpha", "--rollback"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stderr.contains("no earlier version"), "{}", run.stderr);
    drop(daemon);
}

#[test]
fn update_crash_between_dir_and_db_recovers() {
    let kit = Kit::new();
    let io = kit.io_dir();
    let v1 = Pkg::service("svc", "0.1.0").fixture_io(&io).package();
    let v2 = Pkg::service("svc", "0.2.0").fixture_io(&io).package();
    kit.publish(&[&v1], &seq(1));
    kit.write_script("svc", json!([]));
    let env = kit.script_env("svc");
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let daemon = kit.daemon_with(&env_refs);
    import(&kit, "svc");
    assert_eq!(
        kit.run(&["adapter", "enable", "svc", "--accept", &v1.fingerprint()])
            .code,
        0
    );
    kit.publish(&[&v1, &v2], &seq(2));
    assert_eq!(kit.run(&["adapter", "update", "svc"]).code, 0);
    assert_eq!(version_of(&kit, "svc"), "0.2.0");
    stop(daemon);

    // a crash after the new directory was put in place and before the row recorded it leaves a
    // directory that nothing refers to; one after the row and before the old rollback step was
    // deleted leaves that older directory. Both are what the boot sweeps.
    let installed = kit.home.join("adapters/installed/svc");
    for stale in ["0.3.0", "0.0.9"] {
        std::fs::create_dir_all(installed.join(stale)).unwrap();
        std::fs::copy(
            installed.join("0.2.0/oxidepm-fixture-adapter"),
            installed.join(stale).join("oxidepm-fixture-adapter"),
        )
        .unwrap();
    }
    let daemon = kit.daemon_with(&env_refs);
    daemon.wait_for_boot();
    assert_eq!(
        entries(&installed),
        vec!["0.1.0", "0.2.0"],
        "the version in the row and its rollback step stay"
    );
    // the version the row names runs: no tamper error
    let started = std::time::Instant::now();
    loop {
        let info = info_json(&kit, "svc");
        if info["state"] == "ready" {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20) && info["state"] != "failed",
            "{info}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!info_json(&kit, "svc")["note"]
        .to_string()
        .contains("tampered"));
    drop(daemon);
}

#[test]
fn a_revoked_new_release_is_not_installed() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    kit.publish(
        &[&v1, &v2],
        &IndexOpts {
            sequence: 2,
            revoked: vec![v2.exe_sha.clone()],
            ..IndexOpts::default()
        },
    );
    let run = kit.run(&["adapter", "update", "alpha"]);
    // the revoked release is not offered: the newest that is, is what is installed
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(run.stdout.contains("up to date"), "{}", run.stdout);
    assert_eq!(version_of(&kit, "alpha"), "0.1.0");
    drop(daemon);
}

#[test]
fn update_all_updates_each_adapter_and_reports_the_ones_that_failed() {
    let a1 = Pkg::tool("one", "0.1.0").package();
    let a2 = Pkg::tool("one", "0.2.0").package();
    let b1 = Pkg::tool("two", "0.1.0").package();
    let b2 = Pkg::tool("two", "0.2.0")
        .perms(json!({"network": {"mode": "outbound"}}))
        .package();
    let kit = Kit::new();
    kit.publish(&[&a1, &b1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "one");
    import(&kit, "two");
    assert_eq!(enable_with_fingerprint(&kit, &a1).code, 0);
    assert_eq!(enable_with_fingerprint(&kit, &b1).code, 0);
    kit.publish(&[&a1, &a2, &b1, &b2], &seq(2));
    let run = kit.run(&["adapter", "update", "--all"]);
    // `one` is updated, `two` asks for more and needs its fingerprint
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(
        run.stderr.contains("two") && run.stderr.contains("Not updated: two"),
        "{}",
        run.stderr
    );
    assert_eq!(version_of(&kit, "one"), "0.2.0");
    assert_eq!(version_of(&kit, "two"), "0.1.0");
    drop(daemon);
}

#[test]
fn update_dry_run_downloads_and_verifies_and_changes_nothing() {
    let v1 = Pkg::tool("alpha", "0.1.0").package();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    kit.publish(&[&v1, &v2], &seq(2));
    let state = kit.home.join("adapters/accepted-index.json");
    let before = (
        info_json(&kit, "alpha"),
        kit.adapters_tree(),
        std::fs::read(&state).unwrap(),
    );
    let run = kit.run(&["--dry-run", "adapter", "update", "alpha"]);
    assert_eq!(run.code, 0, "{}", run.all());
    assert!(
        run.stdout.contains("Update    from 0.1.0 to 0.2.0")
            && run.stdout.contains("Dry run: verified"),
        "{}",
        run.stdout
    );
    assert!(!run.stdout.contains("index_checked"), "{}", run.stdout);
    assert_eq!(
        (
            info_json(&kit, "alpha"),
            kit.adapters_tree(),
            std::fs::read(state).unwrap(),
        ),
        before
    );
    drop(daemon);
}

#[test]
fn dry_run_refuses_a_stale_index_before_download() {
    let v1 = alpha();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(42));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let state = kit.home.join("adapters/accepted-index.json");
    let before = (
        info_json(&kit, "alpha"),
        kit.adapters_tree(),
        std::fs::read(&state).unwrap(),
    );

    kit.publish(&[&v1, &v2], &seq(41));
    let hits = kit.http.hits();
    let run = kit.run(&["--dry-run", "adapter", "update", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    assert!(run.stdout.is_empty(), "{}", run.all());
    assert!(run.stderr.contains("E_INDEX_STALE"), "{}", run.all());
    assert!(
        run.stderr.contains("41") && run.stderr.contains("42"),
        "{}",
        run.all()
    );
    assert_eq!(kit.http.hits() - hits, 2, "only the index and signature");
    assert_eq!(
        (
            info_json(&kit, "alpha"),
            kit.adapters_tree(),
            std::fs::read(state).unwrap(),
        ),
        before
    );
    drop(daemon);
}

#[test]
fn an_index_archive_404_keeps_the_installed_version() {
    let v1 = alpha();
    let v2 = Pkg::tool("alpha", "0.2.0").package();
    let kit = Kit::new();
    kit.publish(&[&v1], &seq(1));
    let daemon = kit.daemon();
    import(&kit, "alpha");
    let before = kit.adapters_tree();

    let mut body = kit.index_json(&[&v1, &v2], &seq(2));
    let target = target();
    let missing = kit.http.url(&format!(
        "/releases/download/v0.2.0/oxidepm-adapter-alpha-{target}.tar.gz"
    ));
    body["adapters"][0]["releases"][1]["artifacts"][&target]["url"] = json!(missing);
    let body = body.to_string().into_bytes();
    let sig = kit.key.sign_index(&body);
    kit.http.route("/index.json", body);
    kit.http.route("/index.json.minisig", sig.into_bytes());

    let hits = kit.http.hits();
    let run = kit.run(&["adapter", "update", "alpha"]);
    assert_eq!(run.code, 1, "{}", run.all());
    for detail in [&format!("'alpha' 0.2.0 ({target})"), "HTTP 404", &missing] {
        assert!(
            run.stderr.contains(detail),
            "missing {detail}: {}",
            run.all()
        );
    }
    assert_eq!(
        kit.http.hits() - hits,
        3,
        "only the index, signature and missing archive"
    );
    assert_eq!(version_of(&kit, "alpha"), "0.1.0");
    assert_eq!(kit.adapters_tree(), before);
    drop(daemon);
}
