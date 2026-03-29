//! Init command — scan project and generate oxidepm config file

use anyhow::{bail, Result};
use colored::Colorize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

pub async fn execute(dir: Option<String>) -> Result<()> {
    let project_dir = dir
        .map(|d| std::path::PathBuf::from(d))
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    if !project_dir.exists() {
        bail!("Directory not found: {}", project_dir.display());
    }

    let config_path = project_dir.join("oxidepm.config.toml");
    if config_path.exists() {
        bail!(
            "Config already exists: {}\nDelete it first or edit manually.",
            config_path.display()
        );
    }

    println!(
        "{} Scanning {}...",
        "oxidepm init".cyan().bold(),
        project_dir.display()
    );

    let mut apps = Vec::new();

    // Detect project type
    if project_dir.join("Cargo.toml").exists() {
        apps.push(detect_cargo(&project_dir)?);
    }

    if project_dir.join("package.json").exists() {
        apps.push(detect_node(&project_dir)?);
    }

    if project_dir.join("requirements.txt").exists()
        || project_dir.join("pyproject.toml").exists()
        || project_dir.join("setup.py").exists()
    {
        apps.push(detect_python(&project_dir)?);
    }

    if project_dir.join("go.mod").exists() {
        apps.push(detect_go(&project_dir)?);
    }

    if apps.is_empty() {
        println!("  {} No recognized project type found", "!".yellow());
        println!("  Creating generic config...");
        apps.push(AppEntry {
            name: dir_name(&project_dir),
            mode: "cmd".to_string(),
            command: None,
            script: None,
            binary: None,
            cwd: ".".to_string(),
            env: HashMap::new(),
            watch: false,
        });
    }

    // Generate TOML
    let mut toml_content = String::new();
    for app in &apps {
        toml_content.push_str("[[apps]]\n");
        toml_content.push_str(&format!("name = \"{}\"\n", app.name));
        toml_content.push_str(&format!("mode = \"{}\"\n", app.mode));

        if let Some(ref cmd) = app.command {
            toml_content.push_str(&format!("script = \"{}\"\n", cmd));
        }
        if let Some(ref bin) = app.binary {
            toml_content.push_str(&format!("bin = \"{}\"\n", bin));
        }
        if let Some(ref script) = app.script {
            toml_content.push_str(&format!("script = \"{}\"\n", script));
        }

        toml_content.push_str(&format!("cwd = \"{}\"\n", app.cwd));

        if app.watch {
            toml_content.push_str("watch = true\n");
        }

        if !app.env.is_empty() {
            toml_content.push_str("env = { ");
            let pairs: Vec<String> = app
                .env
                .iter()
                .map(|(k, v)| format!("{} = \"{}\"", k, v))
                .collect();
            toml_content.push_str(&pairs.join(", "));
            toml_content.push_str(" }\n");
        }

        toml_content.push('\n');
    }

    fs::write(&config_path, &toml_content)?;

    println!("  {} {}", "Created".green(), config_path.display());
    for app in &apps {
        println!(
            "    {} {} ({})",
            "→".dimmed(),
            app.name.cyan(),
            app.mode
        );
    }
    println!(
        "\nStart with: {}",
        format!("oxidepm start {}", config_path.display()).cyan()
    );

    Ok(())
}

struct AppEntry {
    name: String,
    mode: String,
    command: Option<String>,
    script: Option<String>,
    binary: Option<String>,
    cwd: String,
    env: HashMap<String, String>,
    watch: bool,
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("app")
        .to_string()
}

fn detect_cargo(dir: &Path) -> Result<AppEntry> {
    println!("  {} Cargo project", "✓".green());

    // Try to get binary name from Cargo.toml
    let cargo_content = fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default();
    let name = if let Some(line) = cargo_content.lines().find(|l| l.starts_with("name")) {
        line.split('=')
            .nth(1)
            .map(|v| v.trim().trim_matches('"').to_string())
            .unwrap_or_else(|| dir_name(dir))
    } else {
        dir_name(dir)
    };

    Ok(AppEntry {
        name,
        mode: "cargo".to_string(),
        command: None,
        script: None,
        binary: None,
        cwd: ".".to_string(),
        env: HashMap::from([("RUST_LOG".to_string(), "info".to_string())]),
        watch: true,
    })
}

fn detect_node(dir: &Path) -> Result<AppEntry> {
    let pkg_content = fs::read_to_string(dir.join("package.json")).unwrap_or_default();

    // Detect package manager
    let mode = if dir.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if dir.join("yarn.lock").exists() {
        "yarn"
    } else {
        "npm"
    };

    // Check for start script
    let has_start = pkg_content.contains("\"start\"");
    let has_dev = pkg_content.contains("\"dev\"");

    let script = if has_start {
        "start"
    } else if has_dev {
        "dev"
    } else {
        "start"
    };

    // Try to get name from package.json
    let name = pkg_content
        .lines()
        .find(|l| l.contains("\"name\""))
        .and_then(|l| {
            l.split(':')
                .nth(1)
                .map(|v| v.trim().trim_matches(|c| c == '"' || c == ',' || c == ' ').to_string())
        })
        .unwrap_or_else(|| dir_name(dir));

    println!("  {} Node.js project ({}, script: {})", "✓".green(), mode, script);

    Ok(AppEntry {
        name,
        mode: mode.to_string(),
        command: None,
        script: Some(script.to_string()),
        binary: None,
        cwd: ".".to_string(),
        env: HashMap::from([("NODE_ENV".to_string(), "production".to_string())]),
        watch: true,
    })
}

fn detect_python(dir: &Path) -> Result<AppEntry> {
    println!("  {} Python project", "✓".green());

    // Try to find main entry point
    let entry = if dir.join("main.py").exists() {
        "main.py"
    } else if dir.join("app.py").exists() {
        "app.py"
    } else if dir.join("manage.py").exists() {
        "manage.py runserver"
    } else {
        "app.py"
    };

    Ok(AppEntry {
        name: dir_name(dir),
        mode: "python".to_string(),
        command: Some(entry.to_string()),
        script: None,
        binary: None,
        cwd: ".".to_string(),
        env: HashMap::new(),
        watch: true,
    })
}

fn detect_go(dir: &Path) -> Result<AppEntry> {
    println!("  {} Go project", "✓".green());

    // Get module name from go.mod
    let go_mod = fs::read_to_string(dir.join("go.mod")).unwrap_or_default();
    let name = go_mod
        .lines()
        .find(|l| l.starts_with("module"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|m| m.rsplit('/').next())
        .unwrap_or("app")
        .to_string();

    Ok(AppEntry {
        name,
        mode: "go".to_string(),
        command: Some(".".to_string()),
        script: None,
        binary: None,
        cwd: ".".to_string(),
        env: HashMap::new(),
        watch: true,
    })
}
