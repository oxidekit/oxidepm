//! Names, not behaviour: which official adapter provides a runtime or a command.
//!
//! The core uses these tables to name the adapter in a message, and to reserve the names: an
//! adapter, runtime or command name listed here may be used only by an adapter whose archive is
//! covered by a signature from an embedded key, so a look-alike `runtime-node` cannot
//! receive `oxidepm start app.js`. The tables hold only the words; detection and behaviour are not
//! kept here.

/// `(runtime, adapter that provides it)`.
pub const RUNTIME_ADAPTER: &[(&str, &str)] = &[
    ("node", "runtime-node"),
    ("npm", "runtime-node"),
    ("pnpm", "runtime-node"),
    ("yarn", "runtime-node"),
    ("python", "runtime-python"),
    ("go", "runtime-go"),
    ("rust", "runtime-rust"),
    ("cargo", "runtime-rust"),
];

/// `(command, adapter that provides it)`.
pub const VERB_ADAPTER: &[(&str, &str)] = &[
    ("notify", "notifier"),
    ("web", "ui-web"),
    ("monit", "ui-tui"),
    ("check", "check"),
    ("init", "init"),
    ("git-start", "git-start"),
    ("deploy", "deploy"),
    ("diff", "deploy"),
];

/// Services reserved by name.
const SERVICE_ADAPTERS: &[&str] = &["watch"];

/// Adapter names reserved to signed adapters (every adapter a table names).
pub fn reserved_adapter_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = RUNTIME_ADAPTER
        .iter()
        .chain(VERB_ADAPTER)
        .map(|(_, adapter)| *adapter)
        .chain(SERVICE_ADAPTERS.iter().copied())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Runtime names reserved to signed adapters.
pub fn reserved_runtime_names() -> Vec<&'static str> {
    RUNTIME_ADAPTER
        .iter()
        .map(|(runtime, _)| *runtime)
        .collect()
}

/// Command names reserved to signed adapters.
pub fn reserved_verb_names() -> Vec<&'static str> {
    VERB_ADAPTER.iter().map(|(verb, _)| *verb).collect()
}

/// The runtime a file extension means (`app.js` is `node`). Offline and deterministic: nothing is
/// asked of an adapter.
pub fn runtime_for_extension(ext: &str) -> Option<&'static str> {
    match ext.to_lowercase().as_str() {
        "rs" => Some("rust"),
        "js" | "mjs" | "cjs" | "ts" | "mts" | "cts" => Some("node"),
        "py" => Some("python"),
        "go" => Some("go"),
        _ => None,
    }
}

/// The runtime a project directory means, by the file that marks it (`package.json` is `npm`).
pub fn runtime_for_dir(dir: &std::path::Path) -> Option<&'static str> {
    if dir.join("Cargo.toml").exists() {
        return Some("cargo");
    }
    if dir.join("package.json").exists() {
        return Some("npm");
    }
    if dir.join("requirements.txt").exists()
        || dir.join("pyproject.toml").exists()
        || dir.join("setup.py").exists()
    {
        return Some("python");
    }
    if dir.join("go.mod").exists() {
        return Some("go");
    }
    None
}

/// The adapter that provides `runtime`, for a message.
pub fn adapter_for_runtime(runtime: &str) -> Option<&'static str> {
    RUNTIME_ADAPTER
        .iter()
        .find(|(r, _)| *r == runtime)
        .map(|(_, a)| *a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserved_lists_come_from_the_tables() {
        assert!(reserved_adapter_names().contains(&"runtime-node"));
        assert!(reserved_adapter_names().contains(&"notifier"));
        assert!(reserved_adapter_names().contains(&"watch"));
        assert_eq!(
            reserved_adapter_names()
                .iter()
                .filter(|n| **n == "runtime-node")
                .count(),
            1,
            "no duplicates"
        );
        assert!(reserved_runtime_names().contains(&"cargo"));
        assert!(reserved_verb_names().contains(&"git-start"));
        assert_eq!(adapter_for_runtime("yarn"), Some("runtime-node"));
        assert_eq!(adapter_for_runtime("cmd"), None);
    }

    #[test]
    fn extensions_and_project_files_mean_runtimes() {
        assert_eq!(runtime_for_extension("JS"), Some("node"));
        assert_eq!(runtime_for_extension("mts"), Some("node"));
        assert_eq!(runtime_for_extension("rs"), Some("rust"));
        assert_eq!(runtime_for_extension("rb"), None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(runtime_for_dir(dir.path()), None);
        std::fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(runtime_for_dir(dir.path()), Some("go"));
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(
            runtime_for_dir(dir.path()),
            Some("npm"),
            "package.json comes before go.mod"
        );
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(runtime_for_dir(dir.path()), Some("cargo"));
        // every runtime the tables detect is one the tables name an adapter for
        for name in ["rust", "node", "python", "go", "npm", "cargo"] {
            assert!(adapter_for_runtime(name).is_some(), "{name}");
        }
    }
}
