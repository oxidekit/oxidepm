//! The adapter commands that talk to the daemon: install what was staged, enable with a grant,
//! disable, remove, list and describe what is installed.

use std::io::{self, IsTerminal};

use anyhow::{bail, Result};
use colored::Colorize;
use oxidepm_adapter_proto::signing::{
    is_official_release_trust, BUNDLED_UNVERIFIED_TRUST, BUNDLED_VERIFIED_TRUST,
};
use oxidepm_adapter_proto::{Kind, Manifest, Permissions};
use oxidepm_core::{Error, Paths};
use oxidepm_ipc::{
    AdapterInfo, AdapterReply, AdapterRequest, EnableArgs, InstallArgs, PolicyDecision, Request,
    Response,
};
use serde_json::json;

use super::coded;
use super::preview::{self, sanitize, Machine, Preview};
use super::stage::Staged;
use crate::output::{is_json_mode, print_success};

/// Send an `adapter` request. A rehearsal's plan is shown and `Ok(None)` returned.
async fn call(request: AdapterRequest, autostart: bool) -> Result<Option<AdapterReply>> {
    let client = crate::commands::get_client();
    let request = Request::Adapter(Box::new(request));
    let response = if autostart {
        crate::commands::send_autostart(&client, &request).await?
    } else {
        crate::commands::send(&client, &request).await?
    };
    if let Some(done) = crate::commands::plan_shown(&response) {
        done?;
        return Ok(None);
    }
    match response {
        Response::Adapter(reply) => Ok(Some(*reply)),
        Response::Error { code, message } => Err(coded(code.as_str(), message)),
        _ => bail!("Unexpected response from the daemon"),
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_warnings(warnings: &[String]) {
    for w in warnings {
        println!("  {} {}", "!".yellow(), sanitize(w, false));
    }
}

/// Is a person at a terminal?
pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Ask the daemon to check the index against its history before using it. A dry run checks
/// without recording the index or applying revocations. Reads and previews can proceed without
/// a daemon only if there is no adapter state.
pub async fn index_update(fetched: &super::index_src::Fetched, needs_daemon: bool) -> Result<()> {
    let index_bytes = String::from_utf8(fetched.bytes.clone())
        .map_err(|_| coded("E_ADAPTER_INVALID", "the index is not text"))?;
    let request = Request::Adapter(Box::new(AdapterRequest::IndexUpdate {
        index_bytes,
        index_sig: fetched.sig.clone(),
    }));
    let client = crate::commands::get_client();
    let options = crate::commands::call_options(false);
    let response = if needs_daemon && !options.dry_run {
        client.send_autostart(&request).await
    } else {
        client.send_with(&request, &options).await
    };
    match response {
        Ok(Response::Adapter(reply)) if !options.dry_run => {
            if let AdapterReply::IndexAccepted { disabled, .. } = *reply {
                for name in disabled {
                    eprintln!(
                        "{} adapter '{}' was disabled: the index revokes its executable",
                        "!".yellow(),
                        sanitize(&name, false)
                    );
                }
                return Ok(());
            }
            bail!("Unexpected response from the daemon")
        }
        Ok(Response::DryRun { plan }) if options.dry_run => match plan.decision {
            PolicyDecision::Deny { code, message } => Err(coded(code.as_str(), message)),
            PolicyDecision::Allow => {
                // Older daemons return a plan without checking the index.
                if !matches!(plan.items.as_slice(), [item] if item.action == "index_checked") {
                    bail!("The running daemon cannot check the adapter index during a dry run. Restart it with the current oxidepmd and retry");
                }
                Ok(())
            }
        },
        Ok(Response::Error { code, message }) => Err(coded(code.as_str(), message)),
        Ok(_) => bail!("Unexpected response from the daemon"),
        Err(Error::DaemonNotRunning) if !needs_daemon || options.dry_run => {
            let paths = Paths::from_env()?;
            let clean = [paths.adapters_dir(), paths.adapters_db()]
                .iter()
                .all(|path| {
                    std::fs::symlink_metadata(path)
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                });
            if !clean {
                bail!("Daemon is not running. Run `oxidepm resurrect` before using the adapter index in this home");
            }
            eprintln!("{} The daemon is not running, so the index was not compared with the last one seen", "!".yellow());
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Install the staged adapter: one request, human only.
pub async fn install(staged: &Staged, args: &super::ImportOptions<'_>) -> Result<AdapterReply> {
    let staging = staged
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("the staging directory has no name"))?;
    let (index_bytes, index_sig) = match &staged.index {
        Some(fetched) => (
            Some(
                String::from_utf8(fetched.bytes.clone())
                    .map_err(|_| coded("E_ADAPTER_INVALID", "the index is not text"))?,
            ),
            Some(fetched.sig.clone()),
        ),
        None => (None, None),
    };
    let request = AdapterRequest::Install(Box::new(InstallArgs {
        staging,
        source: staged.source_tag.clone(),
        index_bytes,
        index_sig,
        allow_reserved_name: args.allow_reserved_name,
        allow_downgrade: args.allow_downgrade,
        switch_source: args.switch_source,
        accept: args.accept.clone(),
        builtin_verbs: args.builtin_verbs.to_vec(),
    }));
    match call(request, true).await? {
        Some(reply) => Ok(reply),
        None => bail!("the daemon answered a plan to an install"),
    }
}

/// Print the answer to an install.
pub fn show_installed(reply: &AdapterReply) -> Result<()> {
    if is_json_mode() {
        return print_json(reply);
    }
    if let AdapterReply::Installed {
        message, warnings, ..
    } = reply
    {
        print_success(&sanitize(message, false));
        print_warnings(warnings);
    }
    Ok(())
}

/// What the preview of an installed adapter needs.
fn manifest_of(info: &AdapterInfo) -> Result<Manifest> {
    Manifest::parse(info.manifest_json.as_bytes()).map_err(|e| {
        coded(
            "E_ADAPTER_INVALID",
            format!(
                "the stored manifest of {} is invalid: {}",
                info.name, e.detail
            ),
        )
    })
}

fn trust_line(info: &AdapterInfo) -> String {
    match info.trust.strip_prefix("signed:") {
        Some(key) if is_official_release_trust(&info.trust) => format!(
            "signed by key {key} (official). Program sha256 {}",
            info.exe_sha256
        ),
        Some(key) => format!("signed by key {key}. Program sha256 {}", info.exe_sha256),
        None if info.trust == BUNDLED_VERIFIED_TRUST => format!(
            "bundled with this release. Program sha256 {}",
            info.exe_sha256
        ),
        None if info.trust == BUNDLED_UNVERIFIED_TRUST => {
            format!("bundled, unverified. Program sha256 {}", info.exe_sha256)
        }
        None => format!(
            "pinned by you. Program sha256 {}. No signature",
            info.exe_sha256
        ),
    }
}

/// The installed adapter `name`, if there is one (`None` when it is not installed, or when no
/// daemon runs: nothing can be installed then).
pub async fn installed(name: &str) -> Result<Option<AdapterInfo>> {
    match info_of(name).await {
        Ok(info) => Ok(Some(info)),
        Err(e) => {
            let text = format!("{e:#}");
            if text.contains("E_ADAPTER_MISSING") || text.contains("Daemon is not running") {
                Ok(None)
            } else {
                Err(e)
            }
        }
    }
}

/// The names of the installed adapters.
pub async fn installed_names() -> Result<Vec<String>> {
    match call(AdapterRequest::List, false).await? {
        Some(AdapterReply::List { adapters }) => Ok(adapters.into_iter().map(|a| a.name).collect()),
        _ => Ok(Vec::new()),
    }
}

/// `adapter update --rollback`.
pub async fn rollback(name: &str) -> Result<()> {
    let Some(reply) = call(
        AdapterRequest::Rollback {
            name: name.to_string(),
        },
        false,
    )
    .await?
    else {
        return Ok(());
    };
    show_done(&reply)
}

async fn info_of(name: &str) -> Result<AdapterInfo> {
    match call(
        AdapterRequest::Info {
            name: name.to_string(),
        },
        false,
    )
    .await?
    {
        Some(AdapterReply::Info { adapter }) => Ok(*adapter),
        _ => bail!("Unexpected response from the daemon"),
    }
}

/// `adapter enable`.
pub async fn enable(a: &crate::cli::EnableArgs) -> Result<()> {
    let info = info_of(&a.name).await?;
    let manifest = manifest_of(&info)?;
    let fingerprint = info.fingerprint.clone();
    let tty = interactive();

    // Consent comes from a fingerprint, eligible automatic consent, or a person at a terminal.
    let mut accept = a.accept.clone();
    let needs_official_trust = (!manifest.provides.runtimes.is_empty()
        || !manifest.provides.programs.is_empty())
        && !is_official_release_trust(&info.trust);
    if accept.is_none() && a.yes && (needs_official_trust || !manifest.permissions.is_pure_class())
    {
        let preview = Preview {
            manifest: &manifest,
            source_line: format!(
                "{} installed {}",
                sanitize(&info.source, false),
                info.installed_at
            ),
            trust_line: trust_line(&info),
            fingerprint: fingerprint.clone(),
            machine: Machine::probe(),
            growth: Vec::new(),
            installed_version: None,
            next: format!(
                "oxidepm adapter enable {} --accept {fingerprint}",
                info.name
            ),
        };
        if !is_json_mode() {
            println!("{}", preview::render(&preview));
        }
        let reason = if needs_official_trust {
            format!(
                "{} needs explicit consent to provide runtimes or programs",
                info.name
            )
        } else {
            format!(
                "{} asks for: {}",
                info.name,
                manifest.permissions.broad_items().join(", ")
            )
        };
        return Err(anyhow::Error::new(super::Refusal(format!(
            "--yes is honoured only for adapters with limited permissions. Adapters that provide runtimes or programs also need official release trust. {reason}. Review the adapter and run oxidepm adapter enable {} --accept {fingerprint}",
            info.name
        ))));
    }
    if accept.is_none() && !a.yes {
        let preview = Preview {
            manifest: &manifest,
            source_line: format!(
                "{} installed {}",
                sanitize(&info.source, false),
                info.installed_at
            ),
            trust_line: trust_line(&info),
            fingerprint: fingerprint.clone(),
            machine: Machine::probe(),
            growth: Vec::new(),
            installed_version: None,
            next: format!(
                "oxidepm adapter enable {} --accept {fingerprint}",
                info.name
            ),
        };
        if !tty || is_json_mode() {
            if !is_json_mode() {
                println!("{}", preview::render(&preview));
            }
            return Err(anyhow::Error::new(super::Refusal(format!(
                "Enabling {} needs your consent to what it asks for. Pass --accept {fingerprint}",
                info.name
            ))));
        }
        println!("{}", preview::render(&preview));
        let yes = dialoguer::Confirm::new()
            .with_prompt(format!("Enable {} with this grant?", info.name))
            .default(false)
            .interact()?;
        if !yes {
            bail!("Not enabled");
        }
        accept = Some(fingerprint.clone());
    }

    let request = AdapterRequest::Enable(Box::new(EnableArgs {
        name: a.name.clone(),
        accept,
        yes: a.yes,
        level: a.level.clone(),
        scope: a.scope.clone(),
        allow_unenforced: a.allow_unenforced,
        interactive: tty,
    }));
    let Some(reply) = call(request, false).await? else {
        return Ok(());
    };
    show_enabled(&reply)
}

/// Print the answer to an enable.
pub fn show_enabled(reply: &AdapterReply) -> Result<()> {
    if is_json_mode() {
        return print_json(reply);
    }
    if let AdapterReply::Enabled {
        message,
        enforced,
        unix_note,
        ..
    } = reply
    {
        print_success(&sanitize(message, false));
        if enforced.is_empty() {
            println!("  Enforced by the OS on this machine: nothing");
        } else {
            println!(
                "  Enforced by the OS on this machine: {}",
                enforced.join(", ")
            );
        }
        if let Some(note) = unix_note {
            println!("  {} {}", "!".yellow(), sanitize(note, false));
        }
    }
    Ok(())
}

/// Enable right after an import (`import --enable`).
pub async fn enable_after_import(name: &str, fingerprint: &str, yes: bool) -> Result<()> {
    let args = crate::cli::EnableArgs {
        name: name.to_string(),
        accept: None,
        yes,
        level: None,
        scope: None,
        allow_unenforced: false,
    };
    let _ = fingerprint;
    enable(&args).await
}

/// `adapter disable`.
pub async fn disable(name: &str) -> Result<()> {
    let Some(reply) = call(
        AdapterRequest::Disable {
            name: name.to_string(),
        },
        false,
    )
    .await?
    else {
        return Ok(());
    };
    show_done(&reply)
}

/// `adapter remove`.
pub async fn remove(name: &str, purge: bool) -> Result<()> {
    let Some(reply) = call(
        AdapterRequest::Remove {
            name: name.to_string(),
            purge,
        },
        false,
    )
    .await?
    else {
        return Ok(());
    };
    show_done(&reply)
}

fn show_done(reply: &AdapterReply) -> Result<()> {
    if is_json_mode() {
        return print_json(reply);
    }
    if let AdapterReply::Done { message, warnings } = reply {
        print_success(&sanitize(message, false));
        print_warnings(warnings);
    }
    Ok(())
}

/// `adapter list`.
pub async fn list() -> Result<()> {
    let Some(AdapterReply::List { adapters }) = call(AdapterRequest::List, false).await? else {
        return Ok(());
    };
    if is_json_mode() {
        return print_json(&adapters);
    }
    if adapters.is_empty() {
        println!("No adapters are installed. `oxidepm adapter list --available` shows what the index offers.");
        return Ok(());
    }
    let width = adapters
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    println!(
        "{:<width$}  {:<7}  {:<9}  {:<15}  TRUST",
        "NAME", "KIND", "VERSION", "STATE"
    );
    for a in &adapters {
        let trust = match a.trust.strip_prefix("signed:") {
            Some(key) => format!("signed {key}"),
            None if a.trust == BUNDLED_VERIFIED_TRUST => "bundled with this release".to_string(),
            None if a.trust == BUNDLED_UNVERIFIED_TRUST => "bundled, unverified".to_string(),
            None => "pinned by you".to_string(),
        };
        println!(
            "{:<width$}  {:<7}  {:<9}  {:<15}  {}",
            sanitize(&a.name, false),
            sanitize(&a.kind, false),
            sanitize(&a.version, false),
            sanitize(&a.state, false),
            sanitize(&trust, false)
        );
        if let Some(note) = &a.note {
            println!("{:<width$}  {}", "", sanitize(note, false));
        }
        if let Some(reason) = &a.yanked {
            println!("{:<width$}  yanked: {}", "", sanitize(reason, false));
        }
        if let Some(grant) = grant_of(a) {
            let unenforced = enforced_of(a).is_some_and(|r| r.allow_unenforced);
            if unenforced {
                println!("{:<width$}  runs with --allow-unenforced: the OS does not enforce everything it requires (grant: level {})", "", level_name(&grant));
            }
        }
    }
    Ok(())
}

fn grant_of(a: &AdapterInfo) -> Option<Permissions> {
    a.grant_json
        .as_deref()
        .and_then(|g| serde_json::from_str(g).ok())
}

fn enforced_of(a: &AdapterInfo) -> Option<oxidepm_sandbox::EnforcedRecord> {
    oxidepm_sandbox::EnforcedRecord::parse(a.enforced_json.as_deref())
}

fn level_name(p: &Permissions) -> &'static str {
    match p.host.level {
        oxidepm_adapter_proto::HostLevel::Read => "read",
        oxidepm_adapter_proto::HostLevel::Lifecycle => "lifecycle",
        oxidepm_adapter_proto::HostLevel::Admin => "admin",
    }
}

/// `adapter info` for an installed adapter: `Ok(false)` when it is not installed (or there is no
/// daemon), so the caller can show the index entry instead.
pub async fn info(name: &str) -> Result<bool> {
    let info = match info_of(name).await {
        Ok(info) => info,
        Err(e) => {
            let text = format!("{e:#}");
            if text.contains("E_ADAPTER_MISSING") || text.contains("Daemon is not running") {
                return Ok(false);
            }
            return Err(e);
        }
    };
    if is_json_mode() {
        print_json(&json!({
            "name": info.name, "kind": info.kind, "version": info.version, "summary": info.summary,
            "source": info.source, "trust": info.trust, "enabled": info.enabled, "state": info.state, "note": info.note,
            "fingerprint": info.fingerprint, "exe_sha256": info.exe_sha256,
            "grant": info.grant_json.as_deref().and_then(|g| serde_json::from_str::<serde_json::Value>(g).ok()),
            "enforced": info.enforced_json.as_deref().and_then(|g| serde_json::from_str::<serde_json::Value>(g).ok()),
            "prev_version": info.prev_version, "yanked": info.yanked, "stderr_tail": info.stderr_tail,
            "crashes": info.crashes, "strikes": info.strikes, "installed_at": info.installed_at,
        }))?;
        return Ok(true);
    }
    let manifest = manifest_of(&info)?;
    let kind = if manifest.kind == Kind::Service {
        "service"
    } else {
        "tool"
    };
    println!(
        "{} {} ({})  {}",
        sanitize(&info.name, false).bold(),
        sanitize(&info.version, false),
        kind,
        sanitize(&info.summary, false)
    );
    println!(
        "State:    {}{}",
        sanitize(&info.state, false),
        info.note
            .as_deref()
            .map(|n| format!(" ({})", sanitize(n, false)))
            .unwrap_or_default()
    );
    println!("Source:   {}", sanitize(&info.source, false));
    println!("Trust:    {}", sanitize(&trust_line(&info), false));
    println!("Installed {}", sanitize(&info.installed_at, false));
    if let Some(prev) = &info.prev_version {
        println!(
            "Previous: {} (oxidepm adapter update {} --rollback)",
            sanitize(prev, false),
            sanitize(&info.name, false)
        );
    }
    if let Some(reason) = &info.yanked {
        println!("Yanked:   {}", sanitize(reason, false));
    }
    match grant_of(&info) {
        Some(grant) => println!(
            "Grant:    level {}, scope {}",
            level_name(&grant),
            if grant.host.scope == oxidepm_adapter_proto::HostScope::All {
                "all"
            } else {
                "opted-in"
            }
        ),
        None => println!("Grant:    none (not enabled)"),
    }
    if let Some(record) = enforced_of(&info) {
        let names: Vec<&str> = record
            .enforced
            .iter()
            .map(|e| oxidepm_sandbox::enforce_name(*e))
            .collect();
        println!(
            "Enforced: {}{}",
            if names.is_empty() {
                "nothing".to_string()
            } else {
                names.join(", ")
            },
            if record.allow_unenforced {
                " (you accepted --allow-unenforced)"
            } else {
                ""
            }
        );
        if let Some(note) = record.enforcement_note(&manifest.permissions.requires_enforcement) {
            println!("          {}", sanitize(&note, false));
        }
    }
    println!(
        "Health:   {} crash(es), {} protocol strike(s)",
        info.crashes, info.strikes
    );
    let preview = Preview {
        manifest: &manifest,
        source_line: sanitize(&info.source, false),
        trust_line: trust_line(&info),
        fingerprint: info.fingerprint.clone(),
        machine: Machine::probe(),
        growth: Vec::new(),
        installed_version: None,
        next: format!("fingerprint {}", info.fingerprint),
    };
    println!("\n{}", preview::render(&preview));
    if !info.stderr_tail.is_empty() {
        println!("\nLast lines the adapter wrote to stderr:");
        for line in &info.stderr_tail {
            println!("  {}", sanitize(line, false));
        }
    }
    Ok(true)
}
