//! Startup command implementation

use anyhow::Result;
use std::fs;
use std::process::Command;

use crate::cli::StartupTarget;
use crate::output::{print_info, print_success, print_error};

pub fn execute(target: Option<StartupTarget>) -> Result<()> {
    let target = target.unwrap_or_else(|| {
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
        StartupTarget::Systemd => install_systemd(),
        StartupTarget::Launchd => install_launchd(),
    }
}

fn install_systemd() -> Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    let binary = std::env::current_exe().unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());
    let unit_path = "/etc/systemd/system/oxidepmd.service";

    // Check if already installed
    if std::path::Path::new(unit_path).exists() {
        print_success("Already configured (systemd unit exists)");
        // Make sure it's enabled
        let _ = Command::new("sudo").args(["systemctl", "enable", "oxidepmd"]).output();
        return Ok(());
    }

    let unit = format!(
        r#"[Unit]
Description=OxidePM Process Manager
After=network.target

[Service]
Type=simple
User={user}
ExecStart={binary} daemon
Restart=on-failure
RestartSec=10
Environment=HOME={home}

[Install]
WantedBy=multi-user.target
"#,
        user = user,
        binary = binary.display(),
        home = home.display(),
    );

    println!("Installing systemd service...");

    // Write to temp file
    let tmp_path = "/tmp/oxidepmd.service";
    fs::write(tmp_path, &unit)?;

    // Copy with sudo
    let status = Command::new("sudo")
        .args(["cp", tmp_path, unit_path])
        .status();

    let _ = fs::remove_file(tmp_path);

    match status {
        Ok(s) if s.success() => {
            print_success(&format!("Service installed at {}", unit_path));
        }
        _ => {
            print_error("Failed to install service (need sudo)");
            // Fall back to printing instructions
            print_info("Install manually:");
            println!("{}", unit);
            println!("  sudo cp <file> {}", unit_path);
            println!("  sudo systemctl daemon-reload");
            println!("  sudo systemctl enable oxidepmd");
            return Ok(());
        }
    }

    // Reload, enable, start
    let _ = Command::new("sudo").args(["systemctl", "daemon-reload"]).status();
    print_success("systemd reloaded");

    let _ = Command::new("sudo").args(["systemctl", "enable", "oxidepmd"]).status();
    print_success("Enabled (starts on boot)");

    let _ = Command::new("sudo").args(["systemctl", "start", "oxidepmd"]).status();
    print_success("Started");

    // Save processes
    let _ = Command::new("oxidepm").args(["save"]).output();
    print_success("Processes saved");

    println!();
    println!("OxidePM will auto-start on reboot and restore your processes.");

    Ok(())
}

fn install_launchd() -> Result<()> {
    let home = dirs::home_dir().unwrap_or_default();
    let binary = std::env::current_exe().unwrap_or_default();
    let plist_dir = home.join("Library/LaunchAgents");
    let plist_path = plist_dir.join("com.oxidepm.daemon.plist");

    // Check if already installed
    if plist_path.exists() {
        print_success("Already configured (launchd plist exists)");
        return Ok(());
    }

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.oxidepm.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>{binary}</string>
        <string>daemon</string>
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
        binary = binary.display(),
        home = home.display(),
    );

    println!("Installing launchd service...");

    fs::create_dir_all(&plist_dir)?;
    fs::write(&plist_path, &plist)?;
    print_success(&format!("Plist installed at {}", plist_path.display()));

    let status = Command::new("launchctl")
        .args(["load", &plist_path.to_string_lossy()])
        .status();

    match status {
        Ok(s) if s.success() => {
            print_success("Loaded into launchd");
        }
        _ => {
            print_error("Failed to load — run manually:");
            println!("  launchctl load {}", plist_path.display());
        }
    }

    println!();
    println!("OxidePM will auto-start on login.");

    Ok(())
}
