//! `oxidepm adapter ...`: find, inspect and import adapters.
//!
//! The network, the signature check and the archive run here, in the CLI, never in the daemon.
//! What the daemon is asked is one thing at the end of an import, and it checks again what it
//! can check on bytes it already holds.

mod index_src;
mod ops;
mod preview;
mod stage;

use anyhow::{bail, Result};
use colored::Colorize;
use oxidepm_adapter_proto::{host_target, Kind, Manifest};
use oxidepm_core::{constants, Paths};
use serde_json::json;

use crate::cli::{AdapterArgs, AdapterCommand, ImportArgs, UpdateArgs};
use crate::output::is_json_mode;
use preview::{sanitize, Machine, Preview};
use stage::{Source, StageOpts, Staged};

/// An error that carries its stable code (`E_CHECKSUM_MISMATCH: ...`).
pub(crate) fn coded(code: &str, message: impl std::fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{code}: {message}")
}

/// A refusal that is not a failure of the program: exit code 2 and a sentence (a hash to pin, a
/// fingerprint to pass).
#[derive(Debug)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refusal {}

/// The names the core reserves for official adapters, and the subcommands of the CLI.
#[derive(Debug, Default, Clone)]
pub struct Reserved {
    pub adapters: Vec<String>,
    pub runtimes: Vec<String>,
    pub verbs: Vec<String>,
    pub builtin_verbs: Vec<String>,
}

impl Reserved {
    pub fn current() -> Reserved {
        use clap::CommandFactory;
        let builtin_verbs = crate::cli::Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        let owned = |names: Vec<&'static str>| {
            names
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<String>>()
        };
        Reserved {
            adapters: owned(oxidepm_core::runtime_hints::reserved_adapter_names()),
            runtimes: owned(oxidepm_core::runtime_hints::reserved_runtime_names()),
            verbs: owned(oxidepm_core::runtime_hints::reserved_verb_names()),
            builtin_verbs,
        }
    }
}

pub async fn execute(args: AdapterArgs, dry_run: bool) -> Result<()> {
    match args.command {
        AdapterCommand::Import(a) => import(a, dry_run).await,
        AdapterCommand::List { available } => list(available).await,
        AdapterCommand::Update(a) => update(a, dry_run).await,
        AdapterCommand::Enable(a) => ops::enable(&a).await,
        AdapterCommand::Disable { name } => ops::disable(&name).await,
        AdapterCommand::Remove { name, purge } => ops::remove(&name, purge).await,
        AdapterCommand::Search { query } => search(&query).await,
        AdapterCommand::Info { name } => info(&name).await,
    }
}

// ---- list, search, info (the index) ---------------------------------------------------------

struct Row {
    name: String,
    kind: Kind,
    version: String,
    summary: String,
    tags: Vec<String>,
}

fn rows_of(fetched: &index_src::Fetched) -> Vec<Row> {
    let index = &fetched.verified.index;
    let target = host_target();
    let daemon = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0));
    index
        .adapters
        .iter()
        .map(|a| {
            let version = index
                .latest_offer(
                    &a.name,
                    &target,
                    &daemon,
                    oxidepm_adapter_proto::SUPPORTED_PROTOCOLS,
                )
                .map(|o| o.release.version.clone())
                .unwrap_or_else(|| "-".to_string());
            Row {
                name: a.name.clone(),
                kind: a.kind,
                version,
                summary: a.summary.clone(),
                tags: a.tags.clone(),
            }
        })
        .collect()
}

fn print_rows(rows: &[Row], fetched: &index_src::Fetched) -> Result<()> {
    if is_json_mode() {
        let value: Vec<_> = rows
            .iter()
            .map(|r| json!({"name": r.name, "kind": if r.kind == Kind::Service { "service" } else { "tool" }, "version": r.version, "summary": r.summary, "tags": r.tags}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "index": fetched.origin,
                "sequence": fetched.verified.index.sequence,
                "signed_by": fetched.verified.signed_by.key_id,
                "adapters": value,
            }))?
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("No adapters match.");
        return Ok(());
    }
    let width = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<width$}  {:<7}  {:<8}  SUMMARY",
        "NAME", "KIND", "VERSION"
    );
    for r in rows {
        let kind = if r.kind == Kind::Service {
            "service"
        } else {
            "tool"
        };
        println!(
            "{:<width$}  {:<7}  {:<8}  {}",
            sanitize(&r.name, false),
            kind,
            sanitize(&r.version, false),
            sanitize(&r.summary, false)
        );
    }
    println!(
        "\nIndex {} (sequence {}), signed by key {} (official)",
        sanitize(&fetched.origin, false),
        fetched.verified.index.sequence,
        fetched.verified.signed_by.key_id
    );
    Ok(())
}

async fn list(available: bool) -> Result<()> {
    if !available {
        return ops::list().await;
    }
    let fetched = index_src::fetch_index().await?;
    ops::index_update(&fetched, false).await?;
    print_rows(&rows_of(&fetched), &fetched)
}

async fn search(query: &str) -> Result<()> {
    let fetched = index_src::fetch_index().await?;
    ops::index_update(&fetched, false).await?;
    let needle = query.to_lowercase();
    let index = &fetched.verified.index;
    let rows: Vec<Row> = rows_of(&fetched)
        .into_iter()
        .filter(|r| {
            let long = index
                .adapter(&r.name)
                .map(|a| a.description.to_lowercase())
                .unwrap_or_default();
            r.name.to_lowercase().contains(&needle)
                || r.summary.to_lowercase().contains(&needle)
                || r.tags.iter().any(|t| t.to_lowercase().contains(&needle))
                || long.contains(&needle)
        })
        .collect();
    print_rows(&rows, &fetched)
}

async fn info(name: &str) -> Result<()> {
    // an installed adapter first; the index entry if it is not installed
    if ops::info(name).await? {
        return Ok(());
    }
    let fetched = index_src::fetch_index().await?;
    ops::index_update(&fetched, false).await?;
    let index = &fetched.verified.index;
    let Some(adapter) = index.adapter(name) else {
        bail!(
            "{}",
            coded(
                "E_ADAPTER_MISSING",
                format!("the index has no adapter named '{}'", sanitize(name, false))
            )
        );
    };
    let target = host_target();
    let daemon = semver::Version::parse(env!("CARGO_PKG_VERSION"))
        .unwrap_or_else(|_| semver::Version::new(0, 0, 0));
    let offered: Vec<String> = index
        .offered(
            name,
            &target,
            &daemon,
            oxidepm_adapter_proto::SUPPORTED_PROTOCOLS,
        )
        .iter()
        .map(|o| o.release.version.clone())
        .collect();
    if is_json_mode() {
        let releases: Vec<_> = adapter
            .releases
            .iter()
            .map(|r| json!({"version": r.version, "released": r.released.to_string(), "yanked": r.yanked, "yank_reason": r.yank_reason, "offered": offered.contains(&r.version)}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "name": adapter.name, "kind": if adapter.kind == Kind::Service { "service" } else { "tool" },
                "summary": adapter.summary, "description": adapter.description, "tags": adapter.tags,
                "releases": releases, "index": fetched.origin, "sequence": index.sequence,
                "signed_by": fetched.verified.signed_by.key_id, "official": true,
            }))?
        );
        return Ok(());
    }
    let kind = if adapter.kind == Kind::Service {
        "service"
    } else {
        "tool"
    };
    println!(
        "{} ({})  {}",
        sanitize(&adapter.name, false).bold(),
        kind,
        sanitize(&adapter.summary, false)
    );
    for line in sanitize(&adapter.description, true).lines() {
        println!("  {line}");
    }
    println!(
        "Trust:   index {} (sequence {}), signed by key {} (official)",
        sanitize(&fetched.origin, false),
        index.sequence,
        fetched.verified.signed_by.key_id
    );
    println!("Releases:");
    for r in &adapter.releases {
        let mut notes = Vec::new();
        if r.yanked {
            notes.push(format!(
                "yanked: {}",
                sanitize(r.yank_reason.as_deref().unwrap_or("no reason given"), false)
            ));
        }
        if offered.contains(&r.version) {
            notes.push("can be installed here".to_string());
        }
        println!(
            "  {}  {}  {}",
            sanitize(&r.version, false),
            r.released,
            notes.join("; ")
        );
    }
    Ok(())
}

// ---- import ----------------------------------------------------------------------------------

/// What the install request needs besides the staged adapter.
pub struct ImportOptions<'a> {
    pub allow_reserved_name: bool,
    pub builtin_verbs: &'a [String],
    pub allow_downgrade: bool,
    pub switch_source: bool,
    pub accept: Option<String>,
}

/// What the argument of `import` means: an existing file, a URL, or a name.
fn source_of(arg: &str) -> Source {
    if arg.starts_with("https://") || arg.starts_with("http://") {
        Source::PinnedUrl(arg.to_string())
    } else if arg.contains('/')
        || arg.ends_with(".tar.gz")
        || arg.ends_with(".tgz")
        || std::path::Path::new(arg).is_file()
    {
        Source::PinnedFile(std::path::PathBuf::from(arg))
    } else {
        Source::Index(arg.to_string())
    }
}

fn show_preview(staged: &Staged, next: String, installed: Option<&oxidepm_ipc::AdapterInfo>) {
    // what grew over the installed version, for an update
    let growth = installed
        .and_then(|i| Manifest::parse(i.manifest_json.as_bytes()).ok())
        .map(|old| staged.manifest.grows_over(&old))
        .unwrap_or_default();
    let preview = Preview {
        manifest: &staged.manifest,
        source_line: staged.source_line.clone(),
        trust_line: staged.trust_line(),
        fingerprint: staged.fingerprint(),
        machine: Machine::probe(),
        growth,
        installed_version: installed.map(|i| i.version.clone()),
        next,
    };
    if is_json_mode() {
        let m = &staged.manifest;
        let value = json!({
            "name": m.name, "version": m.version,
            "kind": if m.kind == Kind::Service { "service" } else { "tool" },
            "summary": m.summary, "source": staged.source_line, "trust": staged.trust_line(),
            "fingerprint": preview.fingerprint,
            "archive_sha256": staged.archive_sha256, "exe_sha256": staged.exe_sha256,
            "permissions": m.permissions, "broad": m.permissions.broad_items(),
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    } else {
        println!("{}", preview::render(&preview));
    }
}

async fn import(args: ImportArgs, dry_run: bool) -> Result<()> {
    let sources: Vec<Source> = if args.bundled {
        let (_, index) = index_src::bundled_index()?;
        let mut names: Vec<String> = args.names.clone();
        if names.is_empty() {
            names = index
                .adapters
                .iter()
                .filter(|a| {
                    args.tag
                        .as_deref()
                        .is_none_or(|t| a.tags.iter().any(|x| x == t))
                })
                .map(|a| a.name.clone())
                .collect();
        }
        if names.is_empty() {
            bail!("The bundle has no adapter that matches");
        }
        names.into_iter().map(Source::Bundled).collect()
    } else {
        let Some(arg) = args.names.first() else {
            bail!("Name an adapter, a file or a URL to import (or use --bundled)");
        };
        if args.names.len() > 1 {
            bail!("Import one adapter at a time (several names are for --bundled)");
        }
        vec![source_of(arg)]
    };

    let paths = Paths::new(constants::oxidepm_home());
    let reserved = Reserved::current();
    for source in &sources {
        // a real import stages under adapters/staging; a rehearsal touches no adapter directory
        let parent = if dry_run {
            None
        } else {
            paths.ensure_adapter_dirs()?;
            Some(paths.adapter_staging())
        };
        let opts = StageOpts {
            sha256: args.sha256.as_deref(),
            allow_reserved_name: args.allow_reserved_name,
            staging_parent: parent.as_deref(),
            reserved: &reserved,
            record_index: !dry_run,
            version: None,
            only_newer_than: None,
        };
        let staged = stage::stage(source, &opts).await?;
        // a name that is installed already is an update
        let installed = ops::installed(&staged.manifest.name).await?;
        let next = format!(
            "oxidepm adapter enable {} --accept {}",
            staged.manifest.name,
            staged.fingerprint()
        );
        show_preview(&staged, next, installed.as_ref());
        if dry_run {
            // the staging directory is deleted when `staged` is dropped
            if !is_json_mode() {
                println!("\n{} Dry run: verified, nothing was installed.", "ℹ".blue());
            }
            continue;
        }
        let options = ImportOptions {
            allow_reserved_name: args.allow_reserved_name,
            builtin_verbs: &reserved.builtin_verbs,
            allow_downgrade: false,
            switch_source: false,
            accept: None,
        };
        let reply = ops::install(&staged, &options).await?;
        ops::show_installed(&reply)?;
        if args.enable {
            ops::enable_after_import(&staged.manifest.name, &staged.fingerprint(), args.yes)
                .await?;
        }
    }
    Ok(())
}

// ---- update ----------------------------------------------------------------------------------

async fn update(args: UpdateArgs, dry_run: bool) -> Result<()> {
    if args.rollback {
        let Some(name) = &args.name else {
            bail!("Name the adapter to roll back")
        };
        return ops::rollback(name).await;
    }
    let names: Vec<String> = match (&args.name, args.all) {
        (Some(_), true) => bail!("Name one adapter or use --all, not both"),
        (Some(name), false) => vec![name.clone()],
        (None, true) => ops::installed_names().await?,
        (None, false) => bail!("Name the adapter to update, or use --all"),
    };
    if args.all
        && (args.version.is_some()
            || args.from.is_some()
            || args.sha256.is_some()
            || args.accept.is_some())
    {
        bail!("--version, --from, --sha256 and --accept name one release: use them with one adapter, not --all");
    }
    let mut failed = Vec::new();
    for name in &names {
        match update_one(name, &args, dry_run).await {
            Ok(()) => {}
            Err(e) if args.all => {
                eprintln!("{} {}: {:#}", "✗".red(), sanitize(name, false), e);
                failed.push(name.clone());
            }
            Err(e) => return Err(e),
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("Not updated: {}", failed.join(", "))
    }
}

async fn update_one(name: &str, args: &UpdateArgs, dry_run: bool) -> Result<()> {
    let Some(installed) = ops::installed(name).await? else {
        return Err(coded(
            "E_ADAPTER_MISSING",
            format!(
                "adapter '{}' is not installed. Import it first: oxidepm adapter import {}",
                sanitize(name, false),
                sanitize(name, false)
            ),
        ));
    };
    let old_manifest = Manifest::parse(installed.manifest_json.as_bytes())
        .map_err(|e| coded("E_ADAPTER_INVALID", e.detail))?;
    // where the new release comes from: the index, or what the user names for a pinned adapter
    let source = match &args.from {
        Some(from) => source_of(from),
        None => Source::Index(name.to_string()),
    };
    let paths = Paths::new(constants::oxidepm_home());
    let reserved = Reserved::current();
    let parent = if dry_run {
        None
    } else {
        paths.ensure_adapter_dirs()?;
        Some(paths.adapter_staging())
    };
    let opts = StageOpts {
        sha256: args.sha256.as_deref(),
        allow_reserved_name: false,
        staging_parent: parent.as_deref(),
        reserved: &reserved,
        record_index: !dry_run,
        version: args.version.as_deref(),
        only_newer_than: if args.version.is_none() && !args.allow_downgrade {
            Some(installed.version.as_str())
        } else {
            None
        },
    };
    let staged = match stage::stage(&source, &opts).await {
        Ok(staged) => staged,
        Err(e) => {
            if let Some(up) = e.downcast_ref::<stage::UpToDate>() {
                if !is_json_mode() {
                    println!("{} {}", "✓".green(), sanitize(&up.to_string(), false));
                }
                return Ok(());
            }
            return Err(e);
        }
    };

    let growth = staged.manifest.grows_over(&old_manifest);
    let fingerprint = staged.fingerprint();
    let enabled = installed.enabled;
    let next = if enabled && !growth.is_empty() {
        format!("oxidepm adapter update {name} --accept {fingerprint}")
    } else {
        format!(
            "applying it now ({} to {})",
            installed.version, staged.manifest.version
        )
    };
    show_preview(&staged, next, Some(&installed));
    if dry_run {
        if !is_json_mode() {
            println!("\n{} Dry run: verified, nothing was installed.", "ℹ".blue());
        }
        return Ok(());
    }

    // a release that asks for more than the enabled adapter has needs the new fingerprint
    let mut accept = args.accept.clone();
    if enabled && !growth.is_empty() && accept.is_none() {
        if ops::interactive() && !is_json_mode() {
            let yes = dialoguer::Confirm::new()
                .with_prompt(format!(
                    "{name} {} asks for more than {} has (see above). Update?",
                    staged.manifest.version, installed.version
                ))
                .default(false)
                .interact()?;
            if !yes {
                bail!("Not updated");
            }
            accept = Some(fingerprint.clone());
        } else {
            return Err(anyhow::Error::new(Refusal(format!(
                "{name} {} asks for more than {} has. Nothing was changed and {name} stays on {}. To accept it, run: oxidepm adapter update {name} --accept {fingerprint}",
                staged.manifest.version, installed.version, installed.version
            ))));
        }
    }
    let options = ImportOptions {
        allow_reserved_name: false,
        builtin_verbs: &reserved.builtin_verbs,
        allow_downgrade: args.allow_downgrade,
        switch_source: args.switch_source,
        accept,
    };
    let reply = ops::install(&staged, &options).await?;
    ops::show_installed(&reply)
}
