//! The report: what the fixture observed, as JSON Lines in the file named by `FIXTURE_REPORT`.
//!
//! One object per line: `{"ms": <since start>, "pid": <pid>, "what": "<kind>", ...fields}`. The
//! file is opened for appending and every record is one `write` call, so a record is never torn
//! (also across the fixture's own child processes) and a crash loses nothing. If the file cannot
//! be opened (a sandbox may refuse it) the fixture runs without a report.
//!
//! Kinds: `start`, `frame` (`dir` is `in` or `out`), `violation`, `overflow`, `truncated`, `rule`
//! (a rule fired: its index, trigger and actions), `skipped` (an action needed a request and
//! there was none), the answer to a `host.request` (`host_answer`), and one kind per behaviour
//! that has something to say (`env`, `fds`, `limits`, `nnp`, `touch`, `inet_connect`, `exec`,
//! `exec_shell`, `alloc`, `busy_loop`, `fork_bomb`, `fill_state_dir`, `socket`, `child`,
//! `grandchild`, `flood`, `giant_line`).

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Map, Value};

pub struct Report {
    file: Option<Mutex<File>>,
    start: Instant,
}

impl Report {
    /// Open the file named by `FIXTURE_REPORT`, if the variable is set.
    pub fn from_env() -> Report {
        let file = std::env::var_os("FIXTURE_REPORT")
            .and_then(|path| OpenOptions::new().create(true).append(true).open(path).ok())
            .map(Mutex::new);
        Report {
            file,
            start: Instant::now(),
        }
    }

    pub fn disabled() -> Report {
        Report {
            file: None,
            start: Instant::now(),
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Append one record. `fields` should be a JSON object; anything else is stored as `value`.
    pub fn record(&self, what: &str, fields: Value) {
        let Some(file) = &self.file else { return };
        let mut object = Map::new();
        object.insert("ms".into(), json!(self.elapsed_ms()));
        object.insert("pid".into(), json!(std::process::id()));
        object.insert("what".into(), json!(what));
        match fields {
            Value::Object(map) => object.extend(map),
            Value::Null => {}
            other => {
                object.insert("value".into(), other);
            }
        }
        let mut line = serde_json::to_vec(&Value::Object(object)).unwrap_or_default();
        line.push(b'\n');
        if let Ok(mut file) = file.lock() {
            let _ = file.write_all(&line);
        }
    }
}

/// Read a report file back: every line that parses, in order.
pub fn read_report(path: &std::path::Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
