//! `scripts/install.sh` checks the archive against `checksums.txt` of the release before it
//! installs anything. The script runs for real, with a fake `curl` first in PATH that serves a
//! release from a temporary directory (no network), and an install directory in the same place.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

fn target() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        _ => "x86_64-apple-darwin",
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
    _dir: tempfile::TempDir,
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
    let dir = tempfile::tempdir().unwrap();
    if stale {
        // what an earlier release left where this one's adapters go
        let old = dir.path().join("share/oxidepm/adapters");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("stale-0.0.1.tar.gz"), "x").unwrap();
    }
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
    std::fs::write(served.join("latest"), "{\"tag_name\": \"v9.9.9\"}\n").unwrap();

    // a curl that serves the files above by URL, and fails (as `curl -f` does) for anything else
    let bin = dir.path().join("fakebin");
    std::fs::create_dir(&bin).unwrap();
    let curl = format!(
        r#"#!/bin/sh
out=""
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
case "$url" in
  */releases/latest) src="{served}/latest" ;;
  */checksums.txt) src="{served}/checksums.txt" ;;
  */oxidepm-*.tar.gz) src="{served}/archive" ;;
  *) exit 22 ;;
esac
[ -f "$src" ] || exit 22
if [ -n "$out" ]; then cp "$src" "$out"; else cat "$src"; fi
"#,
        served = served.display()
    );
    std::fs::write(bin.join("curl"), curl).unwrap();
    std::fs::set_permissions(bin.join("curl"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let install_dir = dir.path().join("installed");
    std::fs::create_dir(&install_dir).unwrap();
    let record = dir.path().join("record");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new("sh");
    command
        .arg(script_path())
        .env("PATH", path)
        .env("INSTALL_DIR", &install_dir)
        .env("TMPDIR", dir.path())
        .env("OXIDEPM_RECORD", &record);
    if fail_import {
        command.env("OXIDEPM_FAIL_IMPORT", "1");
    }
    let out = command.output().expect("run sh");
    Run {
        out,
        install_dir,
        record,
        _dir: dir,
    }
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
    for runtime in ["node", "python", "go", "rust"] {
        assert!(
            release.contains(&format!("runtime-{runtime}:$bin/oxidepm-runtime-{runtime}")),
            "the release bundle includes the {runtime} runtime"
        );
    }
    assert!(release.contains("shasum -a 256 -c SHA256SUMS"));
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

    let formula = repo_file("dist/homebrew/generate.rb");
    assert!(formula.contains(r#"share.install "share/oxidepm""#));
    assert!(formula.contains("share/\"oxidepm/adapters/SHA256SUMS\""));
    assert!(formula.contains(IMPORT_RUNTIMES));

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
