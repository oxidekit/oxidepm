//! Startup command implementation

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::StartupTarget;
use crate::output::{is_json_mode, print_error, print_info, print_plan, print_success};
use oxidepm_ipc::{DryRunPlan, PlannedItem};

const UNIT_PATH: &str = "/etc/systemd/system/oxidepmd.service";

pub fn execute(target: Option<StartupTarget>, dry_run: bool) -> Result<()> {
    let target = target.unwrap_or({
        #[cfg(target_os = "macos")]
        {
            StartupTarget::Launchd
        }
        #[cfg(target_os = "linux")]
        {
            StartupTarget::Systemd
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            StartupTarget::Systemd
        }
    });

    match target {
        StartupTarget::Systemd => install_systemd(dry_run),
        StartupTarget::Launchd => install_launchd(dry_run),
    }
}

/// `--dry-run`: say what would be installed and where, print the file, and run and write nothing
/// (no `sudo`, no `systemctl`, no `launchctl`, no directory). `existing` is what is there now.
fn show_rehearsal(path: &Path, content: &str, existing: Option<&str>) -> Result<()> {
    let (action, what) = match existing {
        None => ("write", "a new file"),
        Some(old) if old == content => ("unchanged", "the file is already up to date"),
        Some(_) => ("replace", "it differs from the file there now"),
    };
    let plan = DryRunPlan::allowed(
        "startup",
        vec![PlannedItem::new(None, path.display().to_string(), action).detail(content.to_string())],
        vec![format!(
            "{what}; nothing was written and no service command was run"
        )],
    );
    if is_json_mode() {
        return print_plan(&plan);
    }
    println!(
        "Dry run of 'startup': {} {} ({}). Nothing was changed.",
        action,
        path.display(),
        what
    );
    println!();
    print!("{}", content);
    Ok(())
}

/// The daemon is the separate `oxidepmd` binary installed next to `oxidepm`.
fn resolve_daemon_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("Cannot determine the path of the oxidepm binary")?;
    daemon_next_to(&exe)
}

fn daemon_next_to(exe: &Path) -> Result<PathBuf> {
    let dir = exe
        .parent()
        .context("Cannot determine the directory of the oxidepm binary")?;
    let daemon = dir.join("oxidepmd");
    if !daemon.is_file() {
        bail!(
            "oxidepmd not found next to {} (expected {}). Install the daemon alongside the CLI first",
            exe.display(),
            daemon.display()
        );
    }
    Ok(daemon)
}

/// The account the service runs as: the invoking user (or the user who ran
/// sudo). Never guessed; running as root has to be explicit.
fn resolve_user() -> Result<String> {
    user_from(std::env::var("SUDO_USER").ok(), std::env::var("USER").ok())
}

fn user_from(sudo_user: Option<String>, user: Option<String>) -> Result<String> {
    let user = sudo_user
        .filter(|u| !u.is_empty())
        .or_else(|| user.filter(|u| !u.is_empty()))
        .context(
            "Cannot determine which user the service should run as (set USER or run via sudo)",
        )?;
    if !user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        bail!(
            "Refusing to use unusual user name {:?} in a service unit",
            user
        );
    }
    Ok(user)
}

/// Home directory of `user`: the current one when it is us, otherwise from /etc/passwd.
fn resolve_home(user: &str) -> Result<PathBuf> {
    if std::env::var("USER").ok().as_deref() == Some(user) {
        if let Some(home) = dirs::home_dir() {
            return Ok(home);
        }
    }
    let passwd = fs::read_to_string("/etc/passwd").context("Cannot read /etc/passwd")?;
    passwd
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 6 && f[0] == user).then(|| PathBuf::from(f[5]))
        })
        .next()
        .with_context(|| format!("Cannot find the home directory of user {}", user))
}

/// Quote one word for a systemd unit file value.
fn systemd_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Seconds systemd waits for the daemon to stop before it kills it: the daemon's shutdown budget
/// (what it spends stopping the apps) plus the fixed parts of its shutdown (the hook drain, the
/// adapters, the audit log and some overhead). A shorter timeout would make systemd kill the
/// daemon in the middle of an orderly stop.
fn stop_timeout_secs(shutdown_budget_secs: u64) -> u64 {
    shutdown_budget_secs + 40
}

/// The shutdown budget the daemon of `home` will run with: `daemon.toml` in its data directory,
/// read now, when the unit is written. A file the daemon would refuse is an error here too.
fn shutdown_budget_of(home: &Path) -> Result<u64> {
    let paths = oxidepm_core::Paths::new(home.join(".oxidepm"));
    let config = oxidepm_core::DaemonConfig::load(&paths.daemon_config()).map_err(|e| {
        anyhow::anyhow!(
            "{e}. The daemon would not start with it; fix it before installing the service"
        )
    })?;
    Ok(config.shutdown_budget_secs)
}

fn render_systemd_unit(
    daemon: &Path,
    user: &str,
    home: &Path,
    shutdown_budget_secs: u64,
) -> String {
    format!(
        r#"[Unit]
Description=OxidePM Process Manager
After=network.target

[Service]
Type=simple
User={user}
ExecStart={exec}
Restart=on-failure
RestartSec=10
TimeoutStopSec={stop_timeout}
KillMode=mixed
Environment={env}

[Install]
WantedBy=multi-user.target
"#,
        user = user,
        exec = systemd_quote(&daemon.to_string_lossy()),
        stop_timeout = stop_timeout_secs(shutdown_budget_secs),
        env = systemd_quote(&format!("HOME={}", home.display())),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_launchd_plist(daemon: &Path, home: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.oxidepm.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>{home}</string>
    <key>StandardErrorPath</key>
    <string>{home}/.oxidepm/daemon.err.log</string>
    <key>StandardOutPath</key>
    <string>{home}/.oxidepm/daemon.out.log</string>
</dict>
</plist>
"#,
        binary = xml_escape(&daemon.to_string_lossy()),
        home = xml_escape(&home.to_string_lossy()),
    )
}

fn is_root() -> bool {
    fs::metadata("/proc/self")
        .map(|m| m.uid() == 0)
        .unwrap_or(false)
}

/// A command that runs with root privileges (directly when we already are root).
fn privileged(program: &str) -> Command {
    if is_root() {
        Command::new(program)
    } else {
        let mut cmd = Command::new("sudo");
        cmd.arg(program);
        cmd
    }
}

fn run_checked(mut cmd: Command, what: &str) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("Failed to run: {}", what))?;
    if !status.success() {
        bail!("{} failed ({})", what, status);
    }
    Ok(())
}

/// Write `content` to a root-owned path without any intermediate file.
fn write_privileged(path: &str, content: &str) -> Result<()> {
    let mut cmd = privileged("tee");
    let mut child = cmd
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .with_context(|| format!("Failed to run tee to write {}", path))?;
    child
        .stdin
        .take()
        .context("No stdin for tee")?
        .write_all(content.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("Could not write {} ({})", path, status);
    }
    Ok(())
}

/// Run `oxidepm save` (never auto-starts a daemon); true when it succeeded.
fn run_save() -> bool {
    std::env::current_exe()
        .ok()
        .map(|exe| Command::new(exe).arg("save").output())
        .and_then(|r| r.ok())
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// True when a daemon currently answers `oxidepm ping`.
fn daemon_is_up() -> bool {
    std::env::current_exe()
        .ok()
        .map(|exe| Command::new(exe).arg("ping").output())
        .and_then(|r| r.ok())
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn install_systemd(dry_run: bool) -> Result<()> {
    let daemon = resolve_daemon_binary()?;
    let user = resolve_user()?;
    let home = resolve_home(&user)?;
    let unit = render_systemd_unit(&daemon, &user, &home, shutdown_budget_of(&home)?);

    let existing = fs::read_to_string(UNIT_PATH).ok();
    if dry_run {
        return show_rehearsal(Path::new(UNIT_PATH), &unit, existing.as_deref());
    }
    let changed = existing.as_deref() != Some(unit.as_str());

    if changed {
        if existing.is_some() {
            println!("Updating existing systemd service...");
        } else {
            println!("Installing systemd service...");
        }
        if let Err(e) = write_privileged(UNIT_PATH, &unit) {
            print_error(&format!("Failed to install service: {}", e));
            print_info("Install manually:");
            println!("{}", unit);
            println!("  sudo tee {} < <unit above>", UNIT_PATH);
            println!("  sudo systemctl daemon-reload");
            println!("  sudo systemctl enable --now oxidepmd");
            bail!("Service was not installed");
        }
        print_success(&format!("Service installed at {}", UNIT_PATH));

        let mut reload = privileged("systemctl");
        reload.arg("daemon-reload");
        run_checked(reload, "systemctl daemon-reload")?;
        print_success("systemd reloaded");
    } else {
        print_success("Service unit is already up to date");
    }

    let mut enable = privileged("systemctl");
    enable.args(["enable", "oxidepmd"]);
    run_checked(enable, "systemctl enable oxidepmd")?;
    print_success("Enabled (starts on boot)");

    // A changed unit needs a restart to take effect; an unchanged one only needs
    // a (idempotent) start so a healthy daemon and its processes are left alone.
    if changed && daemon_is_up() {
        // Persist processes started under a manually launched daemon before it
        // is replaced; it resurrects them from the saved list on restart.
        if !run_save() {
            print_error("Could not save the process list before restarting the daemon");
        }
    }
    let action = if changed { "restart" } else { "start" };
    let mut start = privileged("systemctl");
    start.args([action, "oxidepmd"]);
    run_checked(start, &format!("systemctl {} oxidepmd", action))?;

    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", "oxidepmd"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !active {
        bail!("oxidepmd is not running after start; check `journalctl -u oxidepmd`");
    }
    print_success("Started");

    // The daemon binds its socket only after it has resurrected processes, so
    // give it a moment before saving.
    let mut saved = false;
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        if run_save() {
            saved = true;
            break;
        }
    }
    if saved {
        print_success("Processes saved");
    } else {
        print_error("Could not save the process list (run `oxidepm save` later)");
    }

    println!();
    println!("OxidePM will auto-start on reboot and restore your processes.");

    Ok(())
}

fn install_launchd(dry_run: bool) -> Result<()> {
    let daemon = resolve_daemon_binary()?;
    let home = dirs::home_dir().context("Cannot determine the home directory")?;
    let plist_dir = home.join("Library/LaunchAgents");
    let plist_path = plist_dir.join("com.oxidepm.daemon.plist");
    let plist = render_launchd_plist(&daemon, &home);

    let existing = fs::read_to_string(&plist_path).ok();
    if dry_run {
        return show_rehearsal(&plist_path, &plist, existing.as_deref());
    }
    if existing.as_deref() == Some(plist.as_str()) {
        print_success("Already configured (launchd plist is up to date)");
        return Ok(());
    }

    if existing.is_some() {
        println!("Updating existing launchd service...");
        // Unload the old definition; it may not be loaded, so ignore the result.
        let _ = Command::new("launchctl")
            .args(["unload", &plist_path.to_string_lossy()])
            .status();
    } else {
        println!("Installing launchd service...");
    }

    fs::create_dir_all(&plist_dir)?;
    fs::write(&plist_path, &plist)?;
    print_success(&format!("Plist installed at {}", plist_path.display()));

    let mut load = Command::new("launchctl");
    load.args(["load", &plist_path.to_string_lossy()]);
    if let Err(e) = run_checked(load, "launchctl load") {
        print_error(&format!(
            "Failed to load — run manually: launchctl load {}",
            plist_path.display()
        ));
        return Err(e);
    }
    print_success("Loaded into launchd");

    println!();
    println!("OxidePM will auto-start on login.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_runs_oxidepmd_without_a_daemon_argument() {
        let unit = render_systemd_unit(
            Path::new("/opt/oxide pm/oxidepmd"),
            "alice",
            Path::new("/home/alice"),
            120,
        );
        let exec = unit
            .lines()
            .find_map(|l| l.strip_prefix("ExecStart="))
            .expect("ExecStart line");
        // exactly one quoted word: the daemon binary, no subcommand
        assert_eq!(exec, "\"/opt/oxide pm/oxidepmd\"");
        let exe_name = Path::new(exec.trim_matches('"')).file_name().unwrap();
        assert_eq!(exe_name, "oxidepmd");
        assert!(!unit.contains(" daemon"));
        assert!(unit.contains("User=alice"));
        assert!(unit.contains("Environment=\"HOME=/home/alice\""));
    }

    fn unit_with_budget(budget: u64) -> String {
        render_systemd_unit(
            Path::new("/opt/oxidepmd"),
            "alice",
            Path::new("/home/alice"),
            budget,
        )
    }

    fn unit_value(unit: &str, key: &str) -> Option<String> {
        unit.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")).map(str::to_string))
    }

    #[test]
    fn unit_has_timeout_stop_sec_from_the_budget() {
        // the daemon needs its whole shutdown budget, plus 10 s for the hooks, 5 s for the
        // adapters, 1 s for the audit log and some slack: budget + 40
        for (budget, expected) in [(120, "160"), (45, "85"), (10, "50"), (900, "940")] {
            let unit = unit_with_budget(budget);
            assert_eq!(
                unit_value(&unit, "TimeoutStopSec").as_deref(),
                Some(expected),
                "budget {budget}:\n{unit}"
            );
        }
        // it is in the [Service] section, where systemd reads it
        let unit = unit_with_budget(120);
        let service = unit
            .split("[Service]")
            .nth(1)
            .unwrap()
            .split("[Install]")
            .next()
            .unwrap();
        assert!(service.contains("TimeoutStopSec="));
    }

    #[test]
    fn unit_budget_comes_from_daemon_toml() {
        let home = tempfile::tempdir().unwrap();
        // no file: the default budget
        assert_eq!(
            shutdown_budget_of(home.path()).unwrap(),
            oxidepm_core::daemon_config::DEFAULT_SHUTDOWN_BUDGET_SECS
        );
        fs::create_dir(home.path().join(".oxidepm")).unwrap();
        fs::write(
            home.path().join(".oxidepm/daemon.toml"),
            "shutdown_budget_secs = 300\n",
        )
        .unwrap();
        let budget = shutdown_budget_of(home.path()).unwrap();
        assert_eq!(budget, 300);
        let unit = render_systemd_unit(Path::new("/opt/oxidepmd"), "alice", home.path(), budget);
        assert_eq!(unit_value(&unit, "TimeoutStopSec").as_deref(), Some("340"));
        // a file the daemon would refuse is refused here as well
        fs::write(
            home.path().join(".oxidepm/daemon.toml"),
            "shutdown_budget_secs = 5\n",
        )
        .unwrap();
        let err = shutdown_budget_of(home.path()).unwrap_err().to_string();
        assert!(
            err.contains("shutdown_budget_secs") && err.contains("fix it before installing"),
            "{err}"
        );
    }

    #[test]
    fn unit_uses_kill_mode_mixed() {
        let unit = unit_with_budget(120);
        assert_eq!(
            unit_value(&unit, "KillMode").as_deref(),
            Some("mixed"),
            "{unit}"
        );
        let service = unit
            .split("[Service]")
            .nth(1)
            .unwrap()
            .split("[Install]")
            .next()
            .unwrap();
        assert!(service.contains("KillMode=mixed"));
    }

    #[test]
    fn systemd_quote_escapes_specials() {
        assert_eq!(systemd_quote(r#"a"b\c%d$e"#), r#""a\"b\\c%%d$$e""#);
    }

    #[test]
    fn plist_uses_the_daemon_binary_only() {
        let plist = render_launchd_plist(Path::new("/opt/a&b/oxidepmd"), Path::new("/Users/x"));
        assert!(plist.contains("<string>/opt/a&amp;b/oxidepmd</string>"));
        assert!(!plist.contains("<string>daemon</string>"));
    }

    #[test]
    fn daemon_binary_must_exist_next_to_the_cli() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("oxidepm");
        let err = daemon_next_to(&exe).unwrap_err().to_string();
        assert!(err.contains("oxidepmd not found"), "{err}");

        fs::write(dir.path().join("oxidepmd"), b"").unwrap();
        assert_eq!(daemon_next_to(&exe).unwrap(), dir.path().join("oxidepmd"));
    }

    #[test]
    fn user_is_never_defaulted_to_root() {
        assert!(user_from(None, None).is_err());
        assert!(user_from(Some(String::new()), Some(String::new())).is_err());
        assert_eq!(
            user_from(Some("alice".into()), Some("root".into())).unwrap(),
            "alice"
        );
        assert_eq!(user_from(None, Some("bob".into())).unwrap(), "bob");
        assert!(user_from(Some("a\nb".into()), None).is_err());
    }
}
