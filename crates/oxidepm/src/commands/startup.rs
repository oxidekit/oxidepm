//! Startup command implementation

use anyhow::Result;

use crate::cli::StartupTarget;
use crate::output::{print_info, print_success};

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
        StartupTarget::Systemd => print_systemd_instructions(),
        StartupTarget::Launchd => print_launchd_instructions(),
    }

    Ok(())
}

fn print_systemd_instructions() {
    let home = dirs::home_dir().unwrap_or_default();
    let binary = std::env::current_exe().unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_else(|_| "user".to_string());

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

    print_info("Systemd unit file:");
    println!();
    println!("{}", unit);
    println!();
    print_success("To install:");
    println!("  1. Save to /etc/systemd/system/oxidepmd.service");
    println!("  2. sudo systemctl daemon-reload");
    println!("  3. sudo systemctl enable oxidepmd");
    println!("  4. sudo systemctl start oxidepmd");
}

fn print_launchd_instructions() {
    let home = dirs::home_dir().unwrap_or_default();
    let binary = std::env::current_exe().unwrap_or_default();

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

    print_info("Launchd plist file:");
    println!();
    println!("{}", plist);
    println!();
    print_success("To install:");
    println!(
        "  1. Save to ~/Library/LaunchAgents/com.oxidepm.daemon.plist"
    );
    println!("  2. launchctl load ~/Library/LaunchAgents/com.oxidepm.daemon.plist");
}
