//! The saved file (`saved.json`): the list of apps that `resurrect` can bring back when the
//! registry no longer has them.
//!
//! The database is the registry (every app, with its desired state); this file is the backup
//! that `save` writes from it and the shutdown refreshes. Format (version 1):
//!
//! ```json
//! { "version": 1, "saved_at": "<rfc3339>", "apps": [ <AppSpec>, ... ] }
//! ```
//!
//! It is written atomically (a temporary file in the same directory, mode 0600, `fsync`,
//! `rename`, `fsync` of the directory) and read per app: the file is parsed as JSON first, then
//! every entry on its own, so one bad entry never hides the others. A bare JSON array (the
//! format of earlier releases) is refused, never parsed leniently.

use chrono::{SecondsFormat, Utc};
use oxidepm_core::{AppSpec, Error, Paths, Result};
use serde_json::{json, Value};
use std::path::Path;
use tracing::info;

/// The version this build writes and reads.
pub const SAVED_VERSION: u64 = 1;

/// One entry of the saved file.
#[derive(Debug)]
pub enum SavedEntry {
    Spec(Box<AppSpec>),
    /// An entry that is not a usable spec: the name when it could be read, and why.
    Bad {
        name: Option<String>,
        error: String,
    },
}

/// Write the saved file for `specs` (atomic, owner-only, version 1).
pub fn write_saved(paths: &Paths, specs: &[AppSpec]) -> Result<usize> {
    let path = paths.saved();
    let document = json!({
        "version": SAVED_VERSION,
        "saved_at": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "apps": specs,
    });
    write_atomic(
        &path,
        serde_json::to_string_pretty(&document)?.as_bytes(),
        true,
    )?;
    info!("Saved {} apps to {}", specs.len(), path.display());
    Ok(specs.len())
}

/// Replace `path` with `bytes` (owner-only; see [`oxidepm_core::fsutil::write_atomic`]).
pub(super) fn write_atomic(path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
    oxidepm_core::fsutil::write_atomic(path, bytes, durable).map_err(Error::from)
}

/// How many apps the file at `path` lists, or `None` if there is no file. A file that cannot be
/// read or understood counts as non-empty (`usize::MAX`): it is not ours to blank.
pub fn saved_app_count(path: &Path) -> Option<usize> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(_) => return Some(usize::MAX),
    };
    if content.trim().is_empty() {
        return Some(0);
    }
    match serde_json::from_str::<Value>(&content) {
        Ok(Value::Array(apps)) => Some(apps.len()),
        Ok(Value::Object(map)) => match map.get("apps") {
            Some(Value::Array(apps)) => Some(apps.len()),
            _ => Some(usize::MAX),
        },
        _ => Some(usize::MAX),
    }
}

/// The refusal for a save that would empty a file that lists apps.
pub fn would_empty(path: &Path) -> Error {
    Error::SaveWouldEmpty(format!(
        "no app is meant to be running, and {} still lists apps; nothing was written. \
         Use `oxidepm save --force` to replace it with an empty list",
        path.display()
    ))
}

/// The entries of the saved file at `path` (none if there is no file). The file as a whole
/// being unusable (not JSON, the legacy array, a newer version) is an error; an entry that is
/// not a usable spec is a [`SavedEntry::Bad`].
pub fn read_saved(path: &Path) -> Result<Vec<SavedEntry>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    parse_saved(&content, path)
}

fn parse_saved(content: &str, path: &Path) -> Result<Vec<SavedEntry>> {
    let unusable = |why: String| {
        Error::StateIncompatible(format!(
            "{} cannot be read: {}. Fix or delete it",
            path.display(),
            why
        ))
    };
    let value: Value = serde_json::from_str(content).map_err(|e| unusable(e.to_string()))?;
    let map = match value {
        Value::Array(_) => {
            return Err(Error::StateIncompatible(format!(
                "{} is in the pre-1.0 format; run `oxidepm save` again from a running daemon, or delete it",
                path.display()
            )))
        }
        Value::Object(map) => map,
        _ => return Err(unusable("expected a JSON object".to_string())),
    };
    match map.get("version").and_then(Value::as_u64) {
        Some(SAVED_VERSION) => {}
        Some(newer) if newer > SAVED_VERSION => {
            return Err(Error::StateIncompatible(format!(
                "{} is version {}, from a newer OxidePM (this one reads version {}). Move it aside",
                path.display(),
                newer,
                SAVED_VERSION
            )))
        }
        _ => return Err(unusable("it has no usable \"version\"".to_string())),
    }
    let apps = match map.get("apps") {
        Some(Value::Array(apps)) => apps,
        _ => return Err(unusable("it has no \"apps\" list".to_string())),
    };
    Ok(apps.iter().cloned().map(entry).collect())
}

/// One entry, on its own.
fn entry(value: Value) -> SavedEntry {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    match AppSpec::from_json_value(value) {
        Ok(spec) => SavedEntry::Spec(Box::new(spec)),
        Err(e) => SavedEntry::Bad {
            name,
            error: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::Runtime;

    fn spec(name: &str) -> AppSpec {
        AppSpec::new(
            name.to_string(),
            Runtime::CMD,
            "true".to_string(),
            "/".into(),
        )
    }

    #[test]
    fn written_file_reads_back_per_app() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("home"));
        paths.ensure_dirs().unwrap();
        write_saved(&paths, &[spec("a"), spec("b")]).unwrap();
        let entries = read_saved(&paths.saved()).unwrap();
        let names: Vec<_> = entries
            .iter()
            .map(|e| match e {
                SavedEntry::Spec(s) => s.name.clone(),
                SavedEntry::Bad { .. } => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(saved_app_count(&paths.saved()), Some(2));
    }

    #[test]
    fn a_bare_array_is_the_legacy_format() {
        let err = parse_saved("[]", Path::new("/h/saved.json")).unwrap_err();
        assert!(matches!(err, Error::StateIncompatible(_)));
        assert!(err.to_string().contains("pre-1.0 format"), "{err}");
        assert!(err.to_string().contains("oxidepm save"), "{err}");
    }

    #[test]
    fn a_newer_version_and_garbage_are_refused() {
        let newer = parse_saved(r#"{"version": 2, "apps": []}"#, Path::new("s")).unwrap_err();
        assert!(newer.to_string().contains("newer OxidePM"), "{newer}");
        assert!(parse_saved("{ nope", Path::new("s")).is_err());
        assert!(parse_saved(r#"{"apps": []}"#, Path::new("s")).is_err());
        assert!(parse_saved(r#"{"version": 1}"#, Path::new("s")).is_err());
    }

    #[test]
    fn a_bad_entry_does_not_hide_the_others() {
        let good = serde_json::to_value(spec("good")).unwrap();
        let mut removed = serde_json::to_value(spec("old")).unwrap();
        removed["unknown_options"] = json!({ "mode": "x" });
        let mut unknown = serde_json::to_value(spec("odd")).unwrap();
        unknown["bogus_key"] = json!(1);
        let text = json!({ "version": 1, "apps": [good, removed, unknown, 7] }).to_string();
        let entries = parse_saved(&text, Path::new("s")).unwrap();
        assert!(matches!(&entries[0], SavedEntry::Spec(s) if s.name == "good"));
        match &entries[1] {
            SavedEntry::Bad { name, error } => {
                assert_eq!(name.as_deref(), Some("old"));
                assert!(error.contains("unknown_options"), "{error}");
            }
            other => panic!("{other:?}"),
        }
        match &entries[2] {
            SavedEntry::Bad { name, error } => {
                assert_eq!(name.as_deref(), Some("odd"));
                assert!(error.contains("bogus_key"), "{error}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&entries[3], SavedEntry::Bad { name: None, .. }));
    }

    #[test]
    fn counting_apps_in_both_formats() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("saved.json");
        assert_eq!(saved_app_count(&path), None);
        std::fs::write(&path, r#"{"version":1,"apps":[]}"#).unwrap();
        assert_eq!(saved_app_count(&path), Some(0));
        std::fs::write(&path, r#"[{"name":"x"}]"#).unwrap();
        assert_eq!(saved_app_count(&path), Some(1));
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(saved_app_count(&path), Some(usize::MAX));
    }
}
