//! What the install tests need: adapter archives, a signed index, a local HTTP server, and the
//! real `oxidepm` CLI (built with its test support) pointed at them.
//!
//! The index is signed by throwaway keys generated here; the CLI and the daemon are told to trust
//! them through `OXIDEPM_TEST_INDEX_KEYS`, which they read only in a test build.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use oxidepm_adapter_proto::signing::testkeys::TestKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::fixture::fixture_bin;

pub fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The `oxidepm` CLI built with `--features test-support`, next to the fixture.
pub fn cli_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let fixture = fixture_bin();
        let profile_dir = fixture.parent().expect("profile directory").to_path_buf();
        let target_dir = profile_dir
            .parent()
            .expect("target directory")
            .to_path_buf();
        let mut cmd = Command::new(env!("CARGO"));
        cmd.args([
            "build",
            "-p",
            "oxidepm",
            "--features",
            "test-support",
            "--offline",
            "--quiet",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("CARGO_TARGET_DIR", &target_dir);
        if profile_dir.file_name().is_some_and(|n| n == "release") {
            cmd.arg("--release");
        }
        let output = cmd.output().expect("run cargo to build the CLI");
        assert!(
            output.status.success(),
            "building the CLI failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bin = profile_dir.join("oxidepm");
        assert!(bin.is_file(), "the CLI was not built at {}", bin.display());
        bin
    })
}

/// A tiny HTTP server on the loopback interface that serves fixed bodies.
pub struct Http {
    pub base: String,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Http {
    pub fn start() -> Http {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().unwrap().port();
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::new(Mutex::new(HashMap::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, h, s) = (Arc::clone(&routes), Arc::clone(&hits), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (r, h) = (Arc::clone(&r), Arc::clone(&h));
                        std::thread::spawn(move || serve(stream, r, h));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        Http {
            base: format!("http://127.0.0.1:{port}"),
            routes,
            hits,
            stop,
        }
    }

    pub fn route(&self, path: &str, body: Vec<u8>) {
        self.routes.lock().unwrap().insert(path.to_string(), body);
    }

    pub fn body(&self, path: &str) -> Option<Vec<u8>> {
        self.routes.lock().unwrap().get(path).cloned()
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(
    mut stream: TcpStream,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    hits: Arc<AtomicUsize>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let line = String::from_utf8_lossy(&buf);
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    hits.fetch_add(1, Ordering::SeqCst);
    let body = routes.lock().unwrap().get(&path).cloned();
    let _ = match body {
        Some(body) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(head.as_bytes())
                .and_then(|_| stream.write_all(&body))
        }
        None => stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    };
}

/// A .tar.gz of `files` (name, bytes, mode).
pub fn tar_gz(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, data, mode) in files {
        // the raw name field: the tar crate refuses to write a `..` through its path setter
        let mut header = tar::Header::new_old();
        let bytes = name.as_bytes();
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        header.set_size(data.len() as u64);
        header.set_mode(*mode);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, *data).unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn fixture_bytes() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| std::fs::read(fixture_bin()).expect("read the fixture adapter"))
}

/// An adapter to package.
#[derive(Clone)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub kind: &'static str,
    /// Keys of the manifest to replace (`permissions`, `provides`, `summary`, ...).
    pub manifest: Value,
    /// The executable in the archive; the fixture adapter when `None`.
    pub exec: Option<Vec<u8>>,
}

impl Pkg {
    pub fn tool(name: &str, version: &str) -> Pkg {
        Pkg {
            name: name.into(),
            version: version.into(),
            kind: "tool",
            manifest: json!({}),
            exec: None,
        }
    }

    pub fn service(name: &str, version: &str) -> Pkg {
        Pkg {
            name: name.into(),
            version: version.into(),
            kind: "service",
            manifest: json!({}),
            exec: None,
        }
    }

    pub fn with(mut self, key: &str, value: Value) -> Pkg {
        self.manifest[key] = value;
        self
    }

    pub fn perms(mut self, changes: Value) -> Pkg {
        let mut p = default_permissions();
        for (k, v) in changes.as_object().unwrap() {
            p[k.as_str()] = v.clone();
        }
        self.manifest["permissions"] = p;
        self
    }

    /// The permissions that let the fixture read its script and write its report in `io`, with
    /// the environment names it needs (a broad grant: `env_pass` is never in the pure class).
    pub fn fixture_io(self, io: &Path) -> Pkg {
        let io_text = io.display().to_string();
        self.perms(json!({
            "env_pass": ["FIXTURE_SCRIPT", "FIXTURE_REPORT"],
            "fs": {"read": ["$STATE", io_text.clone()], "write": ["$STATE", io_text]}
        }))
    }

    pub fn exec(mut self, bytes: Vec<u8>) -> Pkg {
        self.exec = Some(bytes);
        self
    }

    pub fn manifest_json(&self) -> Value {
        let mut m = json!({
            "schema": 1,
            "name": self.name,
            "version": self.version,
            "kind": self.kind,
            "summary": format!("The {} test adapter.", self.name),
            "description": "Made for the install tests.",
            "protocol": [1],
            "min_daemon": "0.1.0",
            "exec": "oxidepm-fixture-adapter",
            "provides": {},
            "permissions": default_permissions()
        });
        for (k, v) in self.manifest.as_object().unwrap() {
            m[k.as_str()] = v.clone();
        }
        m
    }

    pub fn package(&self) -> Packaged {
        self.package_with(&[])
    }

    /// The archive, with `extra` files beside the manifest and the executable.
    pub fn package_with(&self, extra: &[(&str, &[u8], u32)]) -> Packaged {
        let manifest = self.manifest_json();
        let manifest_bytes = manifest.to_string().into_bytes();
        // every release is its own file: a native executable ignores bytes after its end
        let exec_bytes = self.exec.clone().unwrap_or_else(|| {
            let mut bytes = fixture_bytes().to_vec();
            bytes.extend_from_slice(format!("\n# {} {}\n", self.name, self.version).as_bytes());
            bytes
        });
        let exec_name = manifest["exec"].as_str().unwrap().to_string();
        let mut files: Vec<(&str, &[u8], u32)> = vec![
            ("manifest.json", &manifest_bytes, 0o644),
            (&exec_name, &exec_bytes, 0o755),
        ];
        files.extend_from_slice(extra);
        let archive = tar_gz(&files);
        Packaged {
            name: self.name.clone(),
            version: self.version.clone(),
            kind: self.kind,
            summary: manifest["summary"].as_str().unwrap().to_string(),
            archive_sha: sha_hex(&archive),
            exe_sha: sha_hex(&exec_bytes),
            manifest_sha: sha_hex(&manifest_bytes),
            archive,
            manifest,
        }
    }
}

/// The pure class: nothing broad.
pub fn default_permissions() -> Value {
    json!({
        "host": {"level": "read", "scope": "all"},
        "events": [],
        "env_pass": [],
        "network": {"mode": "none"},
        "fs": {"read": ["$STATE"], "write": ["$STATE"]},
        "spawn": {"mode": "none"},
        "memory_mb": 64,
        "requires_enforcement": []
    })
}

impl Packaged {
    /// The consent fingerprint of this release.
    pub fn fingerprint(&self) -> String {
        oxidepm_adapter_proto::Manifest::parse(self.manifest.to_string().as_bytes())
            .unwrap()
            .fingerprint(&self.exe_sha)
    }
}

pub struct Packaged {
    pub name: String,
    pub version: String,
    pub kind: &'static str,
    pub summary: String,
    pub archive: Vec<u8>,
    pub archive_sha: String,
    pub exe_sha: String,
    pub manifest_sha: String,
    pub manifest: Value,
}

/// How an index is made.
#[derive(Clone)]
pub struct IndexOpts {
    pub sequence: u64,
    pub generated: &'static str,
    pub expires: &'static str,
    pub revoked: Vec<String>,
    pub yanked: Vec<String>,
}

impl Default for IndexOpts {
    fn default() -> Self {
        IndexOpts {
            sequence: 42,
            generated: "2026-11-20T10:00:00Z",
            expires: "2099-01-20T10:00:00Z",
            revoked: Vec::new(),
            yanked: Vec::new(),
        }
    }
}

/// Everything one test needs.
pub struct Kit {
    pub dir: tempfile::TempDir,
    pub home: PathBuf,
    pub bundle: PathBuf,
    pub http: Http,
    pub key: TestKey,
    /// Public keys, besides `key`, that the CLI and the daemon trust.
    pub more_keys: Vec<String>,
}

pub fn target() -> String {
    oxidepm_adapter_proto::host_target()
}

impl Kit {
    pub fn new() -> Kit {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let bundle = dir.path().join(oxidepm_core::release_bundle::DIRECTORY);
        std::fs::create_dir_all(&bundle).unwrap();
        // the sandbox launcher and the CLI are built before any test uses them
        let _ = cli_bin();
        Kit {
            dir,
            home,
            bundle,
            http: Http::start(),
            key: TestKey::generate(),
            more_keys: Vec::new(),
        }
    }

    /// The index document for `pkgs`. Packages with the same name are releases of one adapter.
    pub fn index_json(&self, pkgs: &[&Packaged], opts: &IndexOpts) -> Value {
        let mut names: Vec<&str> = Vec::new();
        for p in pkgs {
            if !names.contains(&p.name.as_str()) {
                names.push(&p.name);
            }
        }
        let adapters: Vec<Value> = names
            .iter()
            .map(|name| {
                let same: Vec<&&Packaged> = pkgs.iter().filter(|p| p.name == *name).collect();
                let first = same[0];
                let releases: Vec<Value> = same
                    .iter()
                    .map(|p| {
                        let file = format!("{}-{}.tar.gz", p.name, p.version);
                        json!({
                            "version": p.version, "released": "2026-11-20", "min_daemon": "0.1.0", "protocol": [1],
                            "yanked": opts.yanked.contains(&p.version), "yank_reason": if opts.yanked.contains(&p.version) { json!("a bug") } else { Value::Null },
                            "artifacts": {target(): {
                                "url": self.http.url(&format!("/{file}")), "size": p.archive.len(),
                                "sha256": p.archive_sha, "exe_sha256": p.exe_sha, "manifest_sha256": p.manifest_sha
                            }}
                        })
                    })
                    .collect();
                json!({
                    "name": first.name, "kind": first.kind, "tags": ["test"],
                    "summary": first.summary, "description": "Made for the install tests.",
                    "releases": releases
                })
            })
            .collect();
        json!({
            "schema": 1, "sequence": opts.sequence,
            "generated": opts.generated, "expires": opts.expires,
            "revoked": opts.revoked.iter().map(|h| json!({"exe_sha256": h, "reason": "a leaked credential"})).collect::<Vec<_>>(),
            "adapters": adapters
        })
    }

    /// Serve the archives and a signed index; also write them to the bundle directory.
    pub fn publish(&self, pkgs: &[&Packaged], opts: &IndexOpts) {
        self.publish_signed(pkgs, opts, &self.key, false);
    }

    pub fn publish_signed(
        &self,
        pkgs: &[&Packaged],
        opts: &IndexOpts,
        key: &TestKey,
        legacy: bool,
    ) {
        let body = self.index_json(pkgs, opts).to_string().into_bytes();
        let sig = if legacy {
            let index = oxidepm_adapter_proto::Index::parse(&body).unwrap();
            key.sign_legacy(&body, &oxidepm_adapter_proto::trusted_comment_for(&index))
        } else {
            key.sign_index(&body)
        };
        use sha2::{Digest, Sha256};
        let mut sums = format!("{:x}  index.json\n", Sha256::digest(&body));
        for p in pkgs {
            let file = format!("{}-{}.tar.gz", p.name, p.version);
            self.http.route(&format!("/{file}"), p.archive.clone());
            std::fs::write(self.bundle.join(&file), &p.archive).unwrap();
            sums.push_str(&format!("{}  {file}\n", p.archive_sha));
        }
        std::fs::write(self.bundle.join("SHA256SUMS"), sums).unwrap();
        self.http.route("/index.json", body.clone());
        self.http
            .route("/index.json.minisig", sig.clone().into_bytes());
        std::fs::write(self.bundle.join("index.json"), &body).unwrap();
        std::fs::write(self.bundle.join("index.json.minisig"), sig).unwrap();
    }

    /// The comma separated keys the CLI and the daemon trust.
    pub fn trusted_keys(&self) -> String {
        let mut keys = vec![self.key.public_base64()];
        keys.extend(self.more_keys.iter().cloned());
        keys.join(",")
    }

    /// Trust another key as well.
    pub fn trust(&mut self, key: &TestKey) {
        self.more_keys.push(key.public_base64());
    }

    /// The environment of a daemon that trusts this kit's keys.
    pub fn daemon_env(&self) -> Vec<(String, String)> {
        vec![
            ("OXIDEPM_TEST_INDEX_KEYS".into(), self.trusted_keys()),
            (
                "OXIDEPM_TEST_BUNDLE_DIR".into(),
                self.bundle.display().to_string(),
            ),
            ("OXIDEPM_TEST_PROBATION_MS".into(), "1500".into()),
        ]
    }

    /// A daemon on this kit's home that trusts this kit's keys, with `extra` in its environment.
    pub fn daemon_with(&self, extra: &[(&str, &str)]) -> super::daemon::DaemonProc {
        let mut env = self.daemon_env();
        env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        super::daemon::DaemonProc::spawn_with_env(&self.home, &env)
    }

    pub fn daemon(&self) -> super::daemon::DaemonProc {
        self.daemon_with(&[])
    }

    /// A directory outside the home that the fixture may read and write (its script, its report).
    pub fn io_dir(&self) -> PathBuf {
        let io = self.dir.path().join("io");
        std::fs::create_dir_all(&io).unwrap();
        io
    }

    /// Where the fixture of the adapter `name` writes what it saw.
    pub fn report(&self, name: &str) -> Vec<Value> {
        oxidepm_adapter_fixture::report::read_report(
            &self.io_dir().join(format!("{name}-report.jsonl")),
        )
    }

    /// Give the fixture of `name` this script (it finds it through the daemon's environment, see
    /// [`Kit::script_env`]).
    pub fn write_script(&self, name: &str, script: Value) {
        std::fs::write(
            self.io_dir().join(format!("{name}-script.json")),
            script.to_string(),
        )
        .unwrap();
    }

    /// The daemon environment that points the adapter `name` at its script and report. One
    /// adapter per daemon can use it (the variable names are shared).
    pub fn script_env(&self, name: &str) -> Vec<(String, String)> {
        let io = self.io_dir();
        vec![
            (
                "FIXTURE_SCRIPT".into(),
                io.join(format!("{name}-script.json")).display().to_string(),
            ),
            (
                "FIXTURE_REPORT".into(),
                io.join(format!("{name}-report.jsonl"))
                    .display()
                    .to_string(),
            ),
        ]
    }

    /// The CLI, bound to this kit's home, server and key.
    pub fn cli(&self) -> Command {
        let mut cmd = Command::new(cli_bin());
        cmd.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("OXIDEPM_HOME", &self.home)
            .env("OXIDEPM_TEST_INDEX_URL", self.http.url("/index.json"))
            .env("OXIDEPM_TEST_INDEX_KEYS", self.trusted_keys())
            .env("OXIDEPM_TEST_BUNDLE_DIR", &self.bundle)
            .current_dir(self.dir.path())
            .stdin(Stdio::null());
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Run {
        let mut cmd = self.cli();
        cmd.args(args);
        Run::of(cmd.output().expect("run the CLI"))
    }

    /// The index body the server serves now.
    pub fn http_index_body(&self) -> Vec<u8> {
        self.http
            .body("/index.json")
            .expect("an index is published")
    }

    /// The signature the server serves now.
    pub fn http_index_sig(&self) -> String {
        String::from_utf8(
            self.http
                .body("/index.json.minisig")
                .expect("a signature is published"),
        )
        .unwrap()
    }

    /// Files and directories under `<home>/adapters` (empty when the directory does not exist).
    pub fn adapters_tree(&self) -> Vec<String> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let p = e.path();
                out.push(p.strip_prefix(root).unwrap().display().to_string());
                if p.is_dir() {
                    walk(&p, root, out);
                }
            }
        }
        let root = self.home.join("adapters");
        let mut out = Vec::new();
        walk(&root, &self.home, &mut out);
        out.sort();
        out
    }
}

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    pub fn of(o: Output) -> Run {
        Run {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
        }
    }

    pub fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
}
