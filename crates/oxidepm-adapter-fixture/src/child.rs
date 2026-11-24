//! Things the fixture does through a Unix socket, and the child-process mode that lets it do them
//! from a process other than the adapter itself (to test the attribution of descendants).
//!
//! A child is the fixture binary started as `oxidepm-fixture-adapter --child <mode> <path> <send>`
//! with its standard streams closed; it appends to the same report file.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use crate::report::Report;

/// The line sent when a script gives none: a `ping` in the daemon's envelope.
pub const DEFAULT_SEND: &str = r#"{"protocol":2,"request_id":"fixture","request":{"type":"ping"}}"#;

const IO_TIMEOUT: Duration = Duration::from_secs(3);
/// Longest answer line kept in the report.
const MAX_ANSWER: u64 = 64 * 1024;

/// Connect to `path`, send one line and (unless `read_answer` is false) read one line back.
/// Records a `socket` entry saying what happened; `from` is `fixture` or `child`.
pub fn socket_exchange(report: &Report, from: &str, path: &str, send: &str, read_answer: bool) {
    let mut entry = json!({ "from": from, "path": path, "ppid": unsafe { libc::getppid() } });
    let outcome = (|| -> Result<Option<String>, std::io::Error> {
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.write_all(send.as_bytes())?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        if !read_answer {
            return Ok(None);
        }
        let mut answer = String::new();
        BufReader::new(stream.take(MAX_ANSWER)).read_line(&mut answer)?;
        Ok(Some(answer.trim_end().to_string()))
    })();
    match outcome {
        Ok(answer) => {
            entry["ok"] = Value::Bool(true);
            if let Some(answer) = answer {
                entry["answer"] = Value::String(answer);
            }
        }
        Err(e) => {
            entry["ok"] = Value::Bool(false);
            entry["error"] = json!(e.to_string());
            entry["os_error"] = json!(e.raw_os_error());
        }
    }
    report.record("socket", entry);
}

/// Run the fixture as a child and wait for it. `mode` is `dial` (connect, send, read) or
/// `write_exit` (connect, send, leave at once).
pub fn run_child(report: &Report, mode: &str, path: &str, send: &str) {
    let mut entry = json!({ "mode": mode });
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            entry["error"] = json!(e.to_string());
            report.record("child", entry);
            return;
        }
    };
    let spawned = Command::new(exe)
        .args(["--child", mode, path, send])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            entry["child_pid"] = json!(child.id());
            match child.wait() {
                Ok(status) => entry["exit"] = json!(status.code()),
                Err(e) => entry["error"] = json!(e.to_string()),
            }
        }
        Err(e) => {
            entry["error"] = json!(e.to_string());
            entry["os_error"] = json!(e.raw_os_error());
        }
    }
    report.record("child", entry);
}

/// The body of the child mode; the exit code of the process.
pub fn child_main(args: &[String]) -> i32 {
    let [mode, path, send] = args else { return 2 };
    let report = Report::from_env();
    match mode.as_str() {
        "dial" => socket_exchange(&report, "child", path, send, true),
        "write_exit" => socket_exchange(&report, "child", path, send, false),
        _ => return 2,
    }
    0
}
