//! Run `scripts/install.sh` with local release fixtures and a temporary installation prefix.

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

fn target() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64-unknown-linux-gnu",
        "aarch64" => "aarch64-unknown-linux-gnu",
        arch => panic!("unsupported test architecture: {arch}"),
    }
}

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A tar.gz with fake `oxidepm` and `oxidepmd` programs, and the `extra` ones, and the adapters
/// of a release when `bundle` is set. The fake `oxidepm` writes its arguments to the file
/// `OXIDEPM_RECORD` names, and fails an `adapter` command when `OXIDEPM_FAIL_IMPORT` is set.
fn release_archive(dir: &Path, extra: &[&str], bundle: bool) -> PathBuf {
    let stage = dir.join("stage");
    std::fs::create_dir(&stage).unwrap();
    let mut names: Vec<&str> = ["oxidepm", "oxidepmd"]
        .into_iter()
        .chain(extra.iter().copied())
        .collect();
    for name in names.iter().copied() {
        let body = if name == "oxidepm" {
            "#!/bin/sh\n[ -n \"$OXIDEPM_RECORD\" ] && echo \"$@\" >> \"$OXIDEPM_RECORD\"\nif [ -n \"$OXIDEPM_FAIL_IMPORT\" ] && [ \"$1\" = adapter ]; then echo 'E_ADAPTER_UNAVAILABLE: no daemon' >&2; exit 1; fi\necho oxidepm 9.9.9\n".to_string()
        } else {
            format!("#!/bin/sh\necho {name} 9.9.9\n")
        };
        std::fs::write(stage.join(name), body).unwrap();
        std::fs::set_permissions(stage.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    if bundle {
        let adapters = stage.join("share/oxidepm/adapters");
        std::fs::create_dir_all(&adapters).unwrap();
        let mut checksums = String::new();
        for (name, contents) in [
            ("index.json", "{}"),
            ("runtime-node-0.1.0.tar.gz", "archive"),
        ] {
            std::fs::write(adapters.join(name), contents).unwrap();
            checksums.push_str(&format!("{}  {name}\n", hex(&Sha256::digest(contents))));
        }
        std::fs::write(adapters.join("SHA256SUMS"), checksums).unwrap();
        names.push("share");
    }
    let archive = dir.join("release.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .args(&names)
        .status()
        .unwrap();
    assert!(status.success());
    archive
}

struct Run {
    out: Output,
    install_dir: PathBuf,
    /// The arguments of each call of the fake `oxidepm`, one call per line.
    record: PathBuf,
    dir: tempfile::TempDir,
}

impl Run {
    fn text(&self) -> String {
        format!(
            "{}{}",
            String::from_utf8_lossy(&self.out.stdout),
            String::from_utf8_lossy(&self.out.stderr)
        )
    }

    /// The adapters directory the installer made, when it did.
    fn adapters_dir(&self) -> PathBuf {
        self.install_dir
            .parent()
            .unwrap()
            .join("share/oxidepm/adapters")
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(&self.record)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn requests(&self) -> Vec<String> {
        fs::read_to_string(self.dir.path().join("requests"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn installed(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.install_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }
}

/// Run the installer against a release whose archive is `archive_bytes` and whose checksums.txt
/// is `checksums(name, actual_hash)` (`None`: the release has no checksums.txt).
fn install(tamper: bool, checksums: impl Fn(&str, &str) -> Option<String>) -> Run {
    install_with(tamper, checksums, &[])
}

/// Like [`install`], with more programs in the archive.
fn install_with(
    tamper: bool,
    checksums: impl Fn(&str, &str) -> Option<String>,
    extra: &[&str],
) -> Run {
    install_full(tamper, checksums, extra, false, false, false)
}

/// Like [`install_with`], with the adapters of a release in the archive when `bundle` is set, and
/// a failing `oxidepm adapter` command when `fail_import` is, and an old archive in the place of
/// the adapters when `stale` is.
fn install_full(
    tamper: bool,
    checksums: impl Fn(&str, &str) -> Option<String>,
    extra: &[&str],
    bundle: bool,
    fail_import: bool,
    stale: bool,
) -> Run {
    let release = Release::new(tamper, checksums, extra, bundle);
    if stale {
        // what an earlier release left where this one's adapters go
        let old = release.dir.path().join("share/oxidepm/adapters");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("stale-0.0.1.tar.gz"), "x").unwrap();
    }
    release.run(&script_path(), None, fail_import)
}

const CURRENT_PUBLIC_KEY: &str = "RWQMeVEZKn2/BHzgJLldWbECE2BqKV4a0rmNXEzn5N0lxrSi/dDuFjHC";
const NEXT_PUBLIC_KEY: &str = "RWTuJOozYEG8Y1hYBqUF8CBncU1PUQgqfJ/hr6Zcwgt1cDwfeRrZh+q5";
const SIGNED_TAG: &str = "v0.4.0-rc.4";
const TRUSTED_COMMENT: &str = "oxidepm release checksums tag=v0.4.0-rc.4";

fn find_program(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find_map(|path| fs::canonicalize(path).ok().filter(|path| path.is_file()))
}

struct Release {
    dir: tempfile::TempDir,
    served: PathBuf,
    bin: PathBuf,
    install_dir: PathBuf,
}

impl Release {
    fn new(
        tamper: bool,
        checksums: impl Fn(&str, &str) -> Option<String>,
        extra: &[&str],
        bundle: bool,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let archive = release_archive(dir.path(), extra, bundle);
        let mut bytes = std::fs::read(&archive).unwrap();
        let honest = hex(&Sha256::digest(&bytes));
        if tamper {
            // the listed hash is for the real archive; what is served is a different file
            bytes.extend_from_slice(b"tampered");
            std::fs::write(&archive, &bytes).unwrap();
        }
        let name = format!("oxidepm-{}.tar.gz", target());
        let served = dir.path().join("served");
        std::fs::create_dir(&served).unwrap();
        std::fs::rename(&archive, served.join("archive")).unwrap();
        if let Some(text) = checksums(&name, &honest) {
            std::fs::write(served.join("checksums.txt"), text).unwrap();
        }
        // a curl that serves the files above by URL, and fails (as `curl -f` does) for anything else
        let bin = dir.path().join("fakebin");
        std::fs::create_dir(&bin).unwrap();
        for program in [
            "awk", "cat", "chmod", "cp", "cut", "dirname", "grep", "gzip", "id", "install",
            "mkdir", "mktemp", "mv", "rm", "sed", "tar", "uname",
        ] {
            symlink(find_program(program).unwrap(), bin.join(program)).unwrap();
        }
        let checksum = if find_program("sha256sum").is_some() {
            "sha256sum"
        } else {
            "shasum"
        };
        symlink(find_program(checksum).unwrap(), bin.join(checksum)).unwrap();
        let curl = r#"#!/bin/sh
out=""
status=""
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -w) status="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
case "$url" in
  https://github.com/*/releases/latest)
    printf '%s\n' 'release page' >> "$FIXTURE_REQUESTS"
    [ "$out" = /dev/null ] && [ "$status" = '%{url_effective}' ] || exit 22
    [ -s "$FIXTURE_SERVED/redirect" ] || exit 22
    printf '%s%s' "${url%/latest}" "$(cat "$FIXTURE_SERVED/redirect")"
    exit 0
    ;;
  https://api.github.com/*/releases/latest)
    printf '%s\n' 'release API' >> "$FIXTURE_REQUESTS"
    if [ -f "$FIXTURE_SERVED/latest.json" ]; then
      cat "$FIXTURE_SERVED/latest.json"
    else
      printf '{"tag_name": "%s"}\n' "$FIXTURE_LATEST"
    fi
    exit 0
    ;;
  */releases/download/*) printf '%s\n' 'download' >> "$FIXTURE_REQUESTS" ;;
esac
case "$url" in
  */download/"$FIXTURE_TAG"/checksums.txt) src="$FIXTURE_SERVED/checksums.txt" ;;
  */download/"$FIXTURE_TAG"/checksums.txt.minisig) src="$FIXTURE_SERVED/checksums.txt.minisig" ;;
  */download/"$FIXTURE_TAG"/oxidepm-*.tar.gz) src="$FIXTURE_SERVED/archive" ;;
  *) exit 22 ;;
esac
if [ ! -f "$src" ]; then
    [ -z "$status" ] || printf '%s' "${FIXTURE_SIGNATURE_STATUS:-404}"
    exit 22
fi
cp "$src" "$out" || exit
[ -z "$status" ] || printf '%s' 200
"#;
        std::fs::write(bin.join("curl"), curl).unwrap();
        std::fs::set_permissions(bin.join("curl"), std::fs::Permissions::from_mode(0o755)).unwrap();

        let install_dir = dir.path().join("installed");
        std::fs::create_dir(&install_dir).unwrap();
        Self {
            dir,
            served,
            bin,
            install_dir,
        }
    }

    fn with_latest(self, redirect: &str, json: &str) -> Self {
        fs::write(self.served.join("redirect"), redirect).unwrap();
        fs::write(self.served.join("latest.json"), json).unwrap();
        self
    }

    fn run(self, script: &Path, version: Option<&str>, fail_import: bool) -> Run {
        self.run_with_status(script, version, SIGNED_TAG, fail_import, "404")
    }

    fn run_with_status(
        self,
        script: &Path,
        version: Option<&str>,
        latest: &str,
        fail_import: bool,
        signature_status: &str,
    ) -> Run {
        let record = self.dir.path().join("record");
        let mut command = Command::new("/bin/sh");
        command
            .arg(script)
            .env("PATH", &self.bin)
            .env("INSTALL_DIR", &self.install_dir)
            .env("HOME", self.dir.path())
            .env("OXIDEPM_HOME", self.dir.path().join("oxidepm-home"))
            .env("TMPDIR", self.dir.path())
            .env("OXIDEPM_VERSION", version.unwrap_or_default())
            .env("FIXTURE_LATEST", latest)
            .env(
                "FIXTURE_TAG",
                version.map_or_else(
                    || latest.to_owned(),
                    |tag| {
                        if tag.starts_with('v') {
                            tag.to_owned()
                        } else {
                            format!("v{tag}")
                        }
                    },
                ),
            )
            .env("FIXTURE_SERVED", &self.served)
            .env("FIXTURE_REQUESTS", self.dir.path().join("requests"))
            .env("FIXTURE_SIGNATURE_STATUS", signature_status)
            .env_remove("OXIDEPM_FAIL_IMPORT")
            .env("OXIDEPM_RECORD", &record);
        if fail_import {
            command.env("OXIDEPM_FAIL_IMPORT", "1");
        }
        let out = command.output().expect("run sh");
        Run {
            out,
            install_dir: self.install_dir,
            record,
            dir: self.dir,
        }
    }
}

fn release(tamper: bool) -> Release {
    Release::new(
        tamper,
        |name, hash| Some(format!("{hash}  {name}\n")),
        &[],
        true,
    )
}

fn install_latest(tag: &str, redirect: &str, json: &str) -> Run {
    release(false).with_latest(redirect, json).run_with_status(
        &script_path(),
        None,
        tag,
        false,
        "404",
    )
}

#[test]
fn install_script_resolves_latest_from_the_release_page() {
    let run = install_latest(SIGNED_TAG, "/tag/v0.4.0-rc.4", "");
    assert!(run.out.status.success(), "{}", run.text());
    assert_eq!(run.requests(), ["release page", "download", "download"]);
}

#[test]
fn install_script_falls_back_to_a_pretty_api_response() {
    let run = install_latest(
        "v0.4.0",
        "",
        "{\n  \"id\": 42,\n  \"tag_name\": \"v0.4.0\",\n  \"name\": \"Release\"\n}",
    );
    assert!(run.out.status.success(), "{}", run.text());
    assert_eq!(
        run.requests(),
        ["release page", "release API", "download", "download"]
    );
}

#[test]
fn install_script_reads_the_tag_from_compact_json() {
    let run = install_latest(SIGNED_TAG, "", r#"{"tag_name":"v0.4.0-rc.4","id":42}"#);
    assert!(run.out.status.success(), "{}", run.text());
}

#[test]
fn install_script_ignores_later_strings_in_compact_json() {
    let run = install_latest(
        SIGNED_TAG,
        "",
        concat!(
            r#"{"body":"Release notes with \"tag_name\" and {braces}","#,
            r#""metadata":{"tag_name":"v1.2.3"},"#,
            r#""tag_name":"v0.4.0-rc.4","name":"v9.8.7"}"#,
        ),
    );
    assert!(run.out.status.success(), "{}", run.text());
}

#[test]
fn install_script_refuses_missing_or_malformed_latest_tags() {
    for json in [
        "",
        "Not JSON",
        r#"{"message":"Not Found"}"#,
        r#"{"tag_name":"v0.4.0-rc.4""#,
        r#"{"tag_name":"v0.4.0-rc.4"false}"#,
        r#"{"tag_name":null}"#,
        r#"{"tag_name":""}"#,
        r#"{"tag_name":"v0.4"}"#,
        r#"{"tag_name":"v0.4.0-"}"#,
        r#"{"tag_name":"v0.4.0-rc-4"}"#,
    ] {
        let run = install_latest(SIGNED_TAG, "/", json);
        assert!(!run.out.status.success(), "{json}: {}", run.text());
        assert!(run.text().contains("Could not determine latest version"));
        assert_eq!(run.requests(), ["release page", "release API"]);
        assert!(run.installed().is_empty());
    }
}

#[test]
fn install_script_refuses_shell_and_path_characters_in_latest_tags() {
    for tag in ["v0.4.0;echo bad", "v0.4.0/../tag/v0.4.0-rc.4", "v0.4.0?x=1"] {
        let run = install_latest(
            SIGNED_TAG,
            &format!("/tag/{tag}"),
            &format!(r#"{{"tag_name":"{tag}"}}"#),
        );
        assert!(!run.out.status.success(), "{tag}: {}", run.text());
        assert!(run.text().contains("Could not determine latest version"));
        assert_eq!(run.requests(), ["release page", "release API"]);
        assert!(run.installed().is_empty());
    }
}

struct SigningKeys {
    minisign: PathBuf,
    dir: tempfile::TempDir,
    current: String,
    next: String,
}

impl SigningKeys {
    fn new() -> Option<Self> {
        let Some(minisign) = find_program("minisign") else {
            eprintln!("Skipping signature test because minisign is not on PATH.");
            return None;
        };
        let dir = tempfile::tempdir().unwrap();
        let current = Self::generate(&minisign, dir.path(), "current");
        let next = Self::generate(&minisign, dir.path(), "next");
        Some(Self {
            minisign,
            dir,
            current,
            next,
        })
    }

    fn generate(minisign: &Path, dir: &Path, name: &str) -> String {
        let out = Command::new(minisign)
            .current_dir(dir)
            .args(["-G", "-W", "-p", &format!("{name}.pub"), "-s", name])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        fs::read_to_string(dir.join(format!("{name}.pub")))
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .to_owned()
    }

    fn installer(&self, release: &Release) -> PathBuf {
        symlink(&self.minisign, release.bin.join("minisign")).unwrap();
        let script = release.dir.path().join("install.sh");
        let body = fs::read_to_string(script_path())
            .unwrap()
            .replace(CURRENT_PUBLIC_KEY, &self.current)
            .replace(NEXT_PUBLIC_KEY, &self.next);
        fs::write(&script, body).unwrap();
        script
    }

    fn sign(&self, release: &Release, key: &str, trusted_comment: &str, extra: &[&str]) {
        let out = Command::new(&self.minisign)
            .current_dir(&release.served)
            .args(["-S", "-s"])
            .arg(self.dir.path().join(key))
            .args(["-m", "checksums.txt", "-t", trusted_comment])
            .args(extra)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn protect_existing_install(release: &Release) {
    for name in ["oxidepm", "oxidepmd", "oxidepm-sandbox"] {
        fs::write(release.install_dir.join(name), name).unwrap();
    }
    let adapters = release.dir.path().join("share/oxidepm/adapters");
    fs::create_dir_all(&adapters).unwrap();
    fs::write(adapters.join("index.json"), "old index").unwrap();
    fs::write(adapters.join("SHA256SUMS"), "old checksums").unwrap();
}

fn assert_untouched(run: &Run, message: &str) {
    let output = run.text();
    assert!(!run.out.status.success(), "{output}");
    assert!(output.contains(message), "{output}");
    assert!(!output.contains("Extracting"), "{output}");
    assert_eq!(run.installed(), ["oxidepm", "oxidepm-sandbox", "oxidepmd"]);
    for name in run.installed() {
        assert_eq!(
            fs::read_to_string(run.install_dir.join(&name)).unwrap(),
            name
        );
    }
    let adapters = run.adapters_dir();
    assert_eq!(
        fs::read_to_string(adapters.join("index.json")).unwrap(),
        "old index"
    );
    assert_eq!(
        fs::read_to_string(adapters.join("SHA256SUMS")).unwrap(),
        "old checksums"
    );
    assert_eq!(fs::read_dir(adapters).unwrap().count(), 2);
    assert!(run.calls().is_empty());
}

#[test]
fn install_script_accepts_either_pinned_signer() {
    let Some(keys) = SigningKeys::new() else {
        return;
    };
    for key in ["current", "next"] {
        let release = release(false);
        keys.sign(&release, key, TRUSTED_COMMENT, &[]);
        let script = keys.installer(&release);
        let run = release.run(&script, None, false);
        assert!(run.out.status.success(), "{}", run.text());
        assert!(
            run.text().contains("Release signature verified"),
            "{}",
            run.text()
        );
        assert!(run.adapters_dir().join("index.json").is_file());
        assert_eq!(run.calls(), [IMPORT_RUNTIMES]);
    }
}

#[test]
fn install_script_refuses_a_missing_signature_for_a_signed_release() {
    let Some(keys) = SigningKeys::new() else {
        return;
    };
    for version in [None, Some(SIGNED_TAG), Some("v0.4.0-rc.10"), Some("v0.4.0")] {
        let release = release(false);
        protect_existing_install(&release);
        let script = keys.installer(&release);
        assert_untouched(&release.run(&script, version, false), "Could not download");
    }
}

#[test]
fn install_script_allows_unsigned_older_releases_only_when_requested_explicitly() {
    let Some(keys) = SigningKeys::new() else {
        return;
    };
    for version in ["0.4.0-rc.3", "v0.3.2"] {
        let release = release(false);
        let script = keys.installer(&release);
        let run = release.run(&script, Some(version), false);
        assert!(run.out.status.success(), "{}", run.text());
        assert!(
            run.text().contains("predates release signatures"),
            "{}",
            run.text()
        );
        assert!(run.text().contains("Checksum verified"), "{}", run.text());
    }

    for (version, latest, status) in [
        (None, "v0.4.0-rc.3", "404"),
        (Some("v0.4.0-rc.3"), SIGNED_TAG, "503"),
    ] {
        let release = release(false);
        protect_existing_install(&release);
        let script = keys.installer(&release);
        assert_untouched(
            &release.run_with_status(&script, version, latest, false, status),
            "Could not download",
        );
    }
}

#[test]
fn install_script_refuses_invalid_signatures() {
    let Some(keys) = SigningKeys::new() else {
        return;
    };
    SigningKeys::generate(&keys.minisign, keys.dir.path(), "unknown");
    for (case, comment, extra) in [
        ("malformed", TRUSTED_COMMENT, vec![]),
        ("extra line", TRUSTED_COMMENT, vec![]),
        ("legacy", TRUSTED_COMMENT, vec!["-l"]),
        ("unknown", TRUSTED_COMMENT, vec![]),
        ("altered checksums", TRUSTED_COMMENT, vec![]),
        (
            "wrong tag",
            "oxidepm release checksums tag=v0.4.0-rc.3",
            vec!["-c", TRUSTED_COMMENT],
        ),
        (
            "wrong purpose",
            "oxidepm adapter index tag=v0.4.0-rc.4",
            vec![],
        ),
    ] {
        let release = release(false);
        protect_existing_install(&release);
        let signer = if case == "unknown" {
            "unknown"
        } else {
            "current"
        };
        keys.sign(&release, signer, comment, &extra);
        let sig = release.served.join("checksums.txt.minisig");
        if case == "malformed" {
            fs::write(&sig, "invalid signature\n").unwrap();
        } else if case == "extra line" {
            let text = fs::read_to_string(&sig).unwrap();
            fs::write(&sig, format!("{text}extra line\n")).unwrap();
        } else if case == "altered checksums" {
            let sums = release.served.join("checksums.txt");
            let text = fs::read_to_string(&sums).unwrap();
            fs::write(sums, format!("{text}\n")).unwrap();
        }
        let script = keys.installer(&release);
        let run = release.run(&script, None, false);
        assert!(!run.out.status.success(), "{case}: {}", run.text());
        assert_untouched(&run, "Release signature");
    }
}

#[test]
fn install_script_refuses_a_tampered_archive_even_with_signed_checksums() {
    let Some(keys) = SigningKeys::new() else {
        return;
    };
    let release = release(true);
    protect_existing_install(&release);
    keys.sign(&release, "current", TRUSTED_COMMENT, &[]);
    let script = keys.installer(&release);
    let run = release.run(&script, None, false);
    assert!(
        run.text().contains("Release signature verified"),
        "{}",
        run.text()
    );
    assert_untouched(&run, "Checksum mismatch");
}

#[test]
fn install_script_installs_an_archive_that_matches_checksums_txt() {
    let run = install(false, |name, hash| {
        Some(format!(
            "{hash}  {name}\n{}  other.tar.gz\n",
            "0".repeat(64)
        ))
    });
    assert!(run.out.status.success(), "{}", run.text());
    assert!(run.text().contains("Checksum verified"), "{}", run.text());
    assert_eq!(
        run.text()
            .lines()
            .filter(|line| *line
                == "Release signature was not checked because minisign is unavailable.")
            .count(),
        1
    );
    assert_eq!(run.installed(), ["oxidepm", "oxidepmd"]);
}

#[test]
fn install_script_refuses_a_tampered_archive() {
    let run = install(true, |name, hash| Some(format!("{hash}  {name}\n")));
    assert!(!run.out.status.success(), "{}", run.text());
    assert!(run.text().contains("Checksum mismatch"), "{}", run.text());
    assert!(
        run.installed().is_empty(),
        "nothing is installed: {:?}",
        run.installed()
    );
}

#[test]
fn install_script_refuses_when_the_release_does_not_vouch_for_the_archive() {
    // no checksums.txt
    let run = install(false, |_, _| None);
    assert!(!run.out.status.success(), "{}", run.text());
    assert!(run.text().contains("Could not download"), "{}", run.text());
    assert!(run.installed().is_empty());

    // checksums.txt without the archive's exact name (a name that merely contains it is not it)
    let run = install(false, |name, hash| {
        Some(format!("{hash}  v1-{name}\n{hash}  {name}.sig\n"))
    });
    assert!(!run.out.status.success(), "{}", run.text());
    assert!(run.text().contains("does not list"), "{}", run.text());
    assert!(run.installed().is_empty());

    // a digest that is not a hash
    let run = install(false, |name, _| Some(format!("nothex  {name}\n")));
    assert!(!run.out.status.success(), "{}", run.text());
    assert!(run.installed().is_empty());

    // an empty checksums.txt
    let run = install(false, |_, _| Some(String::new()));
    assert!(!run.out.status.success(), "{}", run.text());
    assert!(run.installed().is_empty());
}

#[test]
fn install_script_installs_the_sandbox_launcher_when_the_archive_has_it() {
    let run = install_with(
        false,
        |name, hash| Some(format!("{hash}  {name}\n")),
        &["oxidepm-sandbox"],
    );
    assert!(run.out.status.success(), "{}", run.text());
    assert_eq!(run.installed(), ["oxidepm", "oxidepm-sandbox", "oxidepmd"]);
    let mode = std::fs::metadata(run.install_dir.join("oxidepm-sandbox"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o111, 0, "the launcher is executable");
}

const IMPORT_RUNTIMES: &str = "adapter import --bundled --tag runtime --enable --yes";

#[test]
fn install_script_puts_the_adapters_of_the_release_in_place_and_imports_the_runtimes() {
    let run = install_full(
        false,
        |name, hash| Some(format!("{hash}  {name}\n")),
        &[],
        true,
        false,
        false,
    );
    assert!(run.out.status.success(), "{}", run.text());
    // next to the programs' directory, where `oxidepm adapter import --bundled` looks
    let dir = run.adapters_dir();
    let sums = std::fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
    for file in ["index.json", "runtime-node-0.1.0.tar.gz"] {
        let contents = std::fs::read(dir.join(file)).unwrap();
        let expected = format!("{}  {file}", hex(&Sha256::digest(contents)));
        assert!(sums.lines().any(|line| line == expected), "{file}: {sums}");
    }
    assert_eq!(
        run.installed(),
        ["oxidepm", "oxidepmd"],
        "the programs' directory holds only programs"
    );
    // the command is printed and run
    assert!(run.text().contains(IMPORT_RUNTIMES), "{}", run.text());
    assert_eq!(
        run.calls(),
        [IMPORT_RUNTIMES],
        "run once, with exactly these arguments"
    );
    assert!(
        run.text().contains("The runtimes are installed"),
        "{}",
        run.text()
    );
}

#[test]
fn install_script_replaces_adapters_left_by_an_earlier_release() {
    // an archive the new bundle no longer has is not kept: the index no longer lists it
    let run = install_full(
        false,
        |name, hash| Some(format!("{hash}  {name}\n")),
        &[],
        true,
        false,
        true,
    );
    assert!(run.out.status.success(), "{}", run.text());
    assert!(run.adapters_dir().join("index.json").is_file());
    assert!(!run.adapters_dir().join("stale-0.0.1.tar.gz").exists());
}

#[test]
fn install_script_still_installs_when_the_import_fails_and_says_how_to_run_it_again() {
    let run = install_full(
        false,
        |name, hash| Some(format!("{hash}  {name}\n")),
        &[],
        true,
        true,
        false,
    );
    assert!(
        run.out.status.success(),
        "the programs are installed whatever the import says: {}",
        run.text()
    );
    assert_eq!(run.installed(), ["oxidepm", "oxidepmd"]);
    assert!(
        run.text().contains("The runtimes were not imported"),
        "{}",
        run.text()
    );
    assert!(
        run.text().contains(&format!("oxidepm {IMPORT_RUNTIMES}")),
        "the command to run again: {}",
        run.text()
    );
    assert!(run.text().contains("--allow-unenforced"), "{}", run.text());
}

#[test]
fn install_script_without_adapters_in_the_release_installs_as_before_and_says_so() {
    let run = install(false, |name, hash| Some(format!("{hash}  {name}\n")));
    assert!(run.out.status.success(), "{}", run.text());
    assert!(!run.adapters_dir().exists());
    assert!(
        run.calls().is_empty(),
        "nothing is imported: {:?}",
        run.calls()
    );
    assert!(run.text().contains("carries no adapters"), "{}", run.text());
    assert!(
        run.text()
            .contains("oxidepm adapter import <name> --enable"),
        "{}",
        run.text()
    );
}

#[test]
fn install_script_installs_no_adapters_from_a_tampered_archive() {
    let run = install_full(
        true,
        |name, hash| Some(format!("{hash}  {name}\n")),
        &[],
        true,
        false,
        false,
    );
    assert!(!run.out.status.success(), "{}", run.text());
    assert!(!run.adapters_dir().exists());
    assert!(run.calls().is_empty());
}

fn repo_file(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

#[test]
fn every_install_path_carries_the_adapters_of_the_release() {
    // the runtimes are adapters, so a path that left them out would install a daemon that cannot
    // start a node app; each path puts share/oxidepm/adapters in place and says how to import
    let release = repo_file(".github/workflows/release.yml");
    assert!(release.contains("bash ../oxidepm/scripts/build-adapter-bundle.sh"));
    let bundle = repo_file("scripts/build-adapter-bundle.sh");
    for name in [
        "check",
        "deploy",
        "exporter-prometheus",
        "git-start",
        "init",
        "notifier",
        "runtime-go",
        "runtime-node",
        "runtime-python",
        "runtime-rust",
        "ui-tui",
        "ui-web",
        "watch",
    ] {
        assert!(
            bundle.split_whitespace().any(|word| word == name),
            "the release bundle includes {name}"
        );
    }
    assert!(bundle.contains("sha256sum -c SHA256SUMS"));
    assert!(
        release.contains(r#"-C "$PWD" share"#),
        "the release archives hold share/"
    );
    assert!(
        release.contains(r#"cp -R share/oxidepm/adapters "$pkg/usr/share/oxidepm/""#),
        "the .deb holds the adapters"
    );
    assert!(
        release.contains("oxidepm-adapters-$TARGET.tar.gz"),
        "the bundle is a release file of its own too"
    );

    let install = repo_file("scripts/install.sh");
    assert!(install.contains("share/oxidepm/adapters") && install.contains(IMPORT_RUNTIMES));

    let docker = repo_file("Dockerfile");
    assert!(
        docker.contains("/usr/share/oxidepm/adapters")
            && docker.contains("ADAPTER_BUNDLE_URL")
            && docker.contains("sha256sum -c SHA256SUMS")
            && docker.contains(IMPORT_RUNTIMES)
    );

    let aur = repo_file("dist/aur/PKGBUILD");
    assert!(
        aur.contains("$pkgdir/usr/share/oxidepm/adapters")
            && aur.contains("sha256sum --check SHA256SUMS")
            && aur.contains("install=oxidepm.install")
    );
    assert!(repo_file("dist/aur/oxidepm.install").contains(IMPORT_RUNTIMES));

    // and the notes a package prints name the same command
    assert!(release.contains(IMPORT_RUNTIMES));
}

#[test]
fn the_examples_that_need_a_runtime_say_so() {
    for example in ["node-express", "python-fastapi", "full-stack"] {
        let text = repo_file(&format!("examples/{example}.config.toml"));
        assert!(text.contains("Needs the runtime adapters: oxidepm adapter import --bundled --tag runtime --enable --yes"), "{example}");
    }
}
