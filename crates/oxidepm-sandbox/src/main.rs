//! `oxidepm-sandbox`: the launcher that restricts one adapter process and executes it.
//!
//! ```text
//! oxidepm-sandbox run --profile <json> [--allow-unenforced] [--arg0 <name>] [-- <args>...]
//! oxidepm-sandbox selftest --profile <json> --plan <json> [--allow-unenforced]
//! ```
//!
//! The verified executable arrives on descriptor 3. The launcher is single threaded, has no
//! async runtime and no network code. In order it: closes every descriptor above 3, sets
//! `PR_SET_NO_NEW_PRIVS`, the resource limits, the niceness and the OOM score, builds the Landlock
//! ruleset, installs the seccomp filter, and executes descriptor 3. If a step fails, or the
//! profile requires something the kernel cannot do and `--allow-unenforced` was not given, it
//! writes one line to stderr and exits 126; the adapter is not started.
//!
//! `selftest` applies the same restrictions to itself, then tries what the profile forbids (a
//! write, a socket, an exec, reads, connections to the services the plan names) and prints what the
//! kernel said as one JSON report on stdout. It executes nothing on behalf of an adapter. It is the
//! only mode that opens a socket, and only to try one.

mod baseline;
mod landlock_fs;
mod paths;
mod seccomp_filter;
mod selftest;

use std::process::ExitCode;

use oxidepm_sandbox::{Profile, EXIT_SANDBOX_FAILED};

/// What the command line asked for.
struct Run {
    profile: Profile,
    allow_unenforced: bool,
    arg0: Option<String>,
    args: Vec<String>,
}

fn parse_run(mut args: impl Iterator<Item = String>) -> Result<Run, String> {
    let (mut profile, mut allow_unenforced, mut arg0) = (None, false, None);
    let mut rest = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => profile = Some(args.next().ok_or("--profile needs a value")?),
            "--allow-unenforced" => allow_unenforced = true,
            "--arg0" => arg0 = Some(args.next().ok_or("--arg0 needs a value")?),
            "--" => {
                rest.extend(args.by_ref());
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let profile = Profile::from_json(&profile.ok_or("--profile is required")?)?;
    Ok(Run {
        profile,
        allow_unenforced,
        arg0,
        args: rest,
    })
}

/// What `selftest` was asked for.
struct SelfTest {
    profile: Profile,
    plan: oxidepm_sandbox::SelftestPlan,
    allow_unenforced: bool,
}

fn parse_selftest(mut args: impl Iterator<Item = String>) -> Result<SelfTest, String> {
    let (mut profile, mut plan, mut allow_unenforced) = (None, None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--profile" => profile = Some(args.next().ok_or("--profile needs a value")?),
            "--plan" => plan = Some(args.next().ok_or("--plan needs a value")?),
            "--allow-unenforced" => allow_unenforced = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    let profile = Profile::from_json(&profile.ok_or("--profile is required")?)?;
    let plan = serde_json::from_str(&plan.ok_or("--plan is required")?)
        .map_err(|e| format!("the plan is not valid: {e}"))?;
    Ok(SelfTest {
        profile,
        plan,
        allow_unenforced,
    })
}

fn selftest(args: impl Iterator<Item = String>) -> ExitCode {
    let test = match parse_selftest(args) {
        Ok(t) => t,
        Err(e) => return die(&e),
    };
    if let Err(e) = baseline::restrict(&test.profile, test.allow_unenforced, false) {
        return die(&e);
    }
    let report = selftest::run(&test.plan);
    println!(
        "{}",
        serde_json::to_string(&report).unwrap_or_else(|_| "{}".to_string())
    );
    ExitCode::SUCCESS
}

fn die(message: &str) -> ExitCode {
    eprintln!("oxidepm-sandbox: {message}");
    ExitCode::from(EXIT_SANDBOX_FAILED as u8)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("run") => match parse_run(args) {
            Ok(run) => {
                // only returns on failure
                let error = baseline::launch(&run.profile, run.allow_unenforced, run.arg0.as_deref(), &run.args);
                die(&error)
            }
            Err(e) => die(&e),
        },
        Some("selftest") => selftest(args),
        _ => die("usage: oxidepm-sandbox run --profile <json> [--allow-unenforced] [--arg0 <name>] [-- <args>...] | selftest --profile <json> --plan <json>"),
    }
}
