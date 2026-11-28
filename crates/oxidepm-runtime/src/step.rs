//! What the core does with the words of a runtime adapter: checks the steps and the plan it
//! returned, runs the steps itself, and hands back a plan it is willing to execute.
//!
//! An adapter returns data and never runs anything. The core limits what it runs on the
//! adapter's word: only programs the manifest lists in `provides.programs`, only environment
//! keys the manifest lists in `env_set`, never a key on the fixed deny list, and nothing that
//! overrides the app's own `env`. Programs are checked before running in their own group.

use crate::resolve::{absolute_search_path, anchor_runtime_path, daemon_vars};
use crate::spawn::{ChildEnv, GroupChild};
use crate::traits::LaunchPlan;
use oxidepm_adapter_proto::messages::{Capture, PlanResult, Step};
use oxidepm_adapter_proto::text::{env_key_denied, is_env_name};
use oxidepm_adapter_proto::{Limits, Manifest};
use oxidepm_core::{AppSpec, Error, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// The last bytes of a step's output that stay in memory for the error text.
pub const STEP_TAIL_BYTES: usize = 64 * 1024;

/// The first bytes of the output of a whole build that go to the build log.
pub const BUILD_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// The default cap on the time a step may ask for.
pub const BUILD_TIMEOUT_MAX: Duration = Duration::from_secs(3600);

/// What a runtime adapter is allowed to make the core run: the lists of its manifest and the
/// core's own caps.
#[derive(Debug, Clone)]
pub struct RuntimeRules {
    /// `provides.programs`: bare file names.
    pub programs: Vec<String>,
    /// `provides.capture_programs`: the programs whose output may be handed back to the adapter.
    pub capture_programs: Vec<String>,
    /// `provides.env_set`: the keys the adapter may add to the environment.
    pub env_set: Vec<String>,
    /// A step may ask for at most this long.
    pub build_timeout_max: Duration,
    pub limits: Limits,
}

impl RuntimeRules {
    pub fn from_manifest(manifest: &Manifest, build_timeout_max: Duration) -> Self {
        Self {
            programs: manifest.provides.programs.clone(),
            capture_programs: manifest.provides.capture_programs.clone(),
            env_set: manifest.provides.env_set.clone(),
            build_timeout_max,
            limits: Limits::PROTOCOL_1,
        }
    }
}

/// A step that passed [`validate_steps`]: everything needed to run it, resolved.
#[derive(Debug, Clone)]
pub struct CheckedStep {
    pub id: String,
    pub description: String,
    pub program: PathBuf,
    invocation: Option<PathBuf>,
    app_dir: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: ChildEnv,
    pub timeout: Duration,
    pub capture: Capture,
    pub capture_max: usize,
    pub optional: bool,
    /// Keys of `env_add` that were left out because the app's own `env` sets them.
    pub dropped_env: Vec<String>,
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::AdapterInvalid(message.into()))
}

fn has_nul(s: &str) -> bool {
    s.contains('\0')
}

/// Resolve an adapter program using the search path chosen by the daemon and app owner.
/// Absolute requests are authorized by file identity as resolved by canonicalization.
fn resolve_program(
    program: &str,
    app_dir: &Path,
    search: Option<&std::ffi::OsStr>,
    rules: &RuntimeRules,
    capturing: bool,
    what: &str,
) -> Result<(PathBuf, Option<PathBuf>)> {
    let path = Path::new(program);
    if program.is_empty() {
        return invalid("the program is empty");
    }
    if program.contains('/') && !path.is_absolute() {
        return invalid(format!(
            "program '{program}' is a relative path; it must be absolute or a bare name"
        ));
    }
    let search = search
        .map(|search| absolute_search_path(search, app_dir))
        .transpose()?;
    let lookup = |name: &str| {
        search
            .as_ref()
            .and_then(|search| which::which_in(name, Some(search), app_dir).ok())
    };
    let may_capture = |name: &str| !capturing || rules.capture_programs.iter().any(|p| p == name);
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let unlisted = || {
        Error::AdapterInvalid(format!(
            "{what}: program '{name}' is not one the adapter's manifest lists"
        ))
    };
    let capture_denied = || {
        Error::AdapterInvalid(format!(
            "{what}: the output of '{name}' may not be captured"
        ))
    };
    if !path.is_absolute() {
        if !rules.programs.iter().any(|p| p == program) {
            return Err(unlisted());
        }
        if !may_capture(program) {
            return Err(capture_denied());
        }
        return lookup(program).map(|path| (path, None)).ok_or_else(|| {
            Error::ProcessStartFailed(format!("program '{program}' not found in PATH"))
        });
    }

    let actual = path.canonicalize().map_err(|_| unlisted())?;
    // An invocation name can select a mode (for example, a Python virtual environment).
    let invocation = rules
        .programs
        .iter()
        .any(|p| p == name)
        .then_some(path)
        .filter(|_| may_capture(name));
    let mut matched = false;
    for declared in &rules.programs {
        let Some(found) = lookup(declared) else {
            continue;
        };
        if found.canonicalize().ok().as_ref() != Some(&actual) {
            continue;
        }
        matched = true;
        if may_capture(declared) {
            let invocation = invocation.map_or(found, Path::to_path_buf);
            return Ok((actual, Some(invocation)));
        }
    }
    Err(if matched {
        capture_denied()
    } else {
        unlisted()
    })
}

/// The search path used for an adapter's programs, independent of what is passed to the child.
fn program_search_path<'a>(
    spec: &'a AppSpec,
    daemon_env: &'a [(String, String)],
) -> Option<&'a std::ffi::OsStr> {
    spec.env
        .get("PATH")
        .or_else(|| {
            daemon_env
                .iter()
                .rev()
                .find(|(key, _)| key == "PATH")
                .map(|(_, value)| value)
        })
        .map(std::ffi::OsStr::new)
}

/// The directory that bounds a runtime adapter's path selections for this app.
pub fn canonical_app_dir(cwd: &Path) -> Result<PathBuf> {
    let dir = cwd
        .canonicalize()
        .map_err(|e| Error::AdapterInvalid(format!("cannot resolve the app directory: {e}")))?;
    if !dir.is_dir() {
        return invalid(format!(
            "app directory '{}' is not a directory",
            cwd.display()
        ));
    }
    Ok(dir)
}

fn checked_cwd(app_dir: &Path, path: &Path) -> Result<PathBuf> {
    let not_directory =
        || Error::AdapterInvalid(format!("cwd '{}' is not a directory", path.display()));
    let cwd = path.canonicalize().map_err(|_| not_directory())?;
    if !cwd.starts_with(app_dir) {
        return invalid(format!(
            "cwd '{}' must be inside the app directory",
            path.display()
        ));
    }
    if !cwd.is_dir() {
        return Err(not_directory());
    }
    Ok(cwd)
}

/// Open the checked working directory for use by the child. Holding the descriptor until spawn
/// prevents a changed path from sending the child to a different directory.
pub fn open_runtime_cwd(app_dir: &Path, cwd: &Path) -> Result<File> {
    let relative = cwd.strip_prefix(app_dir).map_err(|_| {
        Error::AdapterInvalid(format!(
            "cwd '{}' must be inside the app directory",
            cwd.display()
        ))
    })?;
    let relative = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    let directory = open_app_dir(app_dir)?;
    open_beneath(&directory, relative, libc::O_DIRECTORY)
        .map_err(|e| Error::AdapterInvalid(format!("cannot open cwd '{}': {e}", cwd.display())))
}

/// The entries of an adapter's environment that apply, and the keys left out.
type EnvCheck = (Vec<(String, String)>, Vec<String>);

/// Check the adapter's extra environment against the rules. Returns the entries that apply
/// and the keys left out because the app's own `env` has them (the app's value wins).
fn check_env(
    extra: &std::collections::BTreeMap<String, String>,
    spec: &AppSpec,
    rules: &RuntimeRules,
    what: &str,
) -> Result<EnvCheck> {
    let mut keep = Vec::new();
    let mut dropped = Vec::new();
    for (key, value) in extra {
        if !is_env_name(key) {
            return invalid(format!(
                "{what}: '{}' is not an environment variable name",
                key.escape_debug()
            ));
        }
        if has_nul(value) {
            return invalid(format!("{what}: the value of {key} contains a NUL byte"));
        }
        if env_key_denied(key) {
            return invalid(format!("{what}: {key} may never be set by an adapter"));
        }
        if !rules.env_set.iter().any(|k| k == key) {
            return invalid(format!("{what}: {key} is not in the adapter's env_set"));
        }
        if spec.env.contains_key(key) {
            dropped.push(key.clone());
            continue;
        }
        keep.push((key.clone(), value.clone()));
    }
    Ok((keep, dropped))
}

/// Check the steps a runtime adapter returned. Nothing runs here. A step that breaks a rule
/// fails the whole list with `E_ADAPTER_INVALID`, before the first step starts.
pub fn validate_steps(
    steps: &[Step],
    spec: &AppSpec,
    rules: &RuntimeRules,
) -> Result<Vec<CheckedStep>> {
    validate_steps_with(steps, spec, rules, &daemon_vars())
}

/// [`validate_steps`] with the daemon's environment given.
pub fn validate_steps_with(
    steps: &[Step],
    spec: &AppSpec,
    rules: &RuntimeRules,
    daemon_env: &[(String, String)],
) -> Result<Vec<CheckedStep>> {
    if steps.len() > rules.limits.max_steps {
        return invalid(format!(
            "{} steps; at most {} are accepted",
            steps.len(),
            rules.limits.max_steps
        ));
    }
    if steps.is_empty() {
        return Ok(Vec::new());
    }
    let app_dir = canonical_app_dir(&spec.cwd)?;
    let mut ids = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(steps.len());
    for step in steps {
        let id = step.id.as_str();
        if id.is_empty() || id.len() > 64 || !ids.insert(id) {
            return invalid(format!(
                "step id '{}' is empty, too long or used twice",
                id.escape_debug()
            ));
        }
        let what = format!("step '{id}'");
        let Some(argv0) = step.argv.first() else {
            return invalid(format!("{what}: argv is empty"));
        };
        if step.argv.len() > rules.limits.plan_args_items
            || step.argv.iter().map(|a| a.len()).sum::<usize>() > rules.limits.plan_args_bytes
            || step.argv.iter().any(|a| has_nul(a))
        {
            return invalid(format!("{what}: argv is too large or contains a NUL byte"));
        }
        if let Some(cwd) = &step.cwd {
            if has_nul(cwd) {
                return invalid(format!("{what}: cwd contains a NUL byte"));
            }
        }
        let cwd = match &step.cwd {
            Some(c) if !c.is_empty() => checked_cwd(&app_dir, &app_dir.join(c))?,
            _ => app_dir.clone(),
        };
        let capturing = step.capture != Capture::None;
        let capture_max = match step.capture_max {
            Some(n) if n as usize > rules.limits.capture_max => {
                return invalid(format!(
                    "{what}: capture_max {n} is over the limit of {}",
                    rules.limits.capture_max
                ))
            }
            Some(n) => n as usize,
            None => rules.limits.capture_max,
        };
        let (add, dropped_env) = check_env(&step.env_add, spec, rules, &what)?;
        // A capturing step runs without the app's env, so `cargo metadata` can be captured and
        // `env` cannot.
        let mut env = if capturing {
            let mut bare = spec.clone();
            bare.env.clear();
            bare.env_inherit = false;
            crate::resolve::build_env_from(
                &bare,
                &add,
                daemon_env,
                crate::resolve::EnvMode::Isolated,
            )
        } else {
            crate::resolve::build_env_from(spec, &add, daemon_env, crate::resolve::APP_ENV_MODE)
        };
        anchor_runtime_path(&mut env, &app_dir)?;
        let (program, invocation) = resolve_program(
            argv0,
            &app_dir,
            program_search_path(spec, daemon_env),
            rules,
            capturing,
            &what,
        )?;
        let timeout = Duration::from_secs(step.timeout_secs.max(1)).min(rules.build_timeout_max);
        out.push(CheckedStep {
            id: step.id.clone(),
            description: step.description.clone(),
            program,
            invocation,
            app_dir: app_dir.clone(),
            args: step.argv[1..].to_vec(),
            cwd,
            env,
            timeout,
            capture: step.capture,
            capture_max,
            optional: step.optional,
            dropped_env,
        });
    }
    Ok(out)
}

/// A plan that passed [`validate_plan`].
#[derive(Debug, Clone)]
pub struct CheckedPlan {
    pub launch: LaunchPlan,
    /// The invocation path for a program executed via its canonical path.
    pub invocation: Option<PathBuf>,
    pub binary_path: Option<PathBuf>,
    /// A plan that runs a build output needs a successful build step.
    pub requires_build: bool,
    /// The open source file for an executable that needs a digest.
    pub executable: Option<Arc<File>>,
    /// Keys of `extra_env` left out because the app's own `env` sets them.
    pub dropped_env: Vec<String>,
}

/// Check the launch plan of a runtime adapter, a fresh one or a stored one. Nothing runs.
pub fn validate_plan(
    plan: &PlanResult,
    spec: &AppSpec,
    rules: &RuntimeRules,
) -> Result<CheckedPlan> {
    validate_plan_with(plan, spec, rules, &daemon_vars())
}

/// [`validate_plan`] with the daemon's environment given.
pub fn validate_plan_with(
    plan: &PlanResult,
    spec: &AppSpec,
    rules: &RuntimeRules,
    daemon_env: &[(String, String)],
) -> Result<CheckedPlan> {
    validate_plan_with_venv(plan, spec, rules, daemon_env, &mut VenvProbe::default())
}

#[derive(Default)]
struct VenvProbe {
    target: Option<PathBuf>,
    identity: Option<(u64, u64)>,
}

/// Validate a plan, consulting the declared Python on PATH if a virtualenv links to its
/// version-managed interpreter. The probe is fixed by the daemon and its output stays here.
pub async fn validate_runtime_plan(
    plan: &PlanResult,
    spec: &AppSpec,
    rules: &RuntimeRules,
) -> Result<CheckedPlan> {
    let daemon_env = daemon_vars();
    let mut venv = VenvProbe::default();
    let first = validate_plan_with_venv(plan, spec, rules, &daemon_env, &mut venv);
    if first.is_ok() || venv.target.is_none() {
        return first;
    }
    for name in ["python3", "python"] {
        let probe = Step {
            id: "python-identity".into(),
            argv: [
                name,
                "-I",
                "-S",
                "-c",
                "import os; s = os.stat('/proc/self/exe'); print(s.st_dev, s.st_ino)",
            ]
            .map(str::to_owned)
            .into(),
            cwd: None,
            env_add: Default::default(),
            timeout_secs: 5,
            capture: Capture::Stdout,
            capture_max: Some(128),
            optional: true,
            description: "Identify the Python interpreter".into(),
        };
        let Ok(steps) = validate_steps_with(&[probe], spec, rules, &daemon_env) else {
            continue;
        };
        let Ok(outcome) = run_step(&steps[0], &mut BuildLog::none()).await else {
            continue;
        };
        if !outcome.success() {
            continue;
        }
        let Some((device, inode)) = outcome
            .stdout
            .as_deref()
            .and_then(|line| line.trim_end().split_once(' '))
        else {
            continue;
        };
        let (Ok(device), Ok(inode)) = (device.parse::<u64>(), inode.parse::<u64>()) else {
            continue;
        };
        venv.identity = Some((device, inode));
        if let Ok(checked) = validate_plan_with_venv(plan, spec, rules, &daemon_env, &mut venv) {
            return Ok(checked);
        }
    }
    first
}

fn validate_plan_with_venv(
    plan: &PlanResult,
    spec: &AppSpec,
    rules: &RuntimeRules,
    daemon_env: &[(String, String)],
    venv: &mut VenvProbe,
) -> Result<CheckedPlan> {
    let what = "plan";
    if has_nul(&plan.program)
        || has_nul(&plan.cwd)
        || plan.binary_path.as_deref().is_some_and(has_nul)
    {
        return invalid("plan: a path contains a NUL byte");
    }
    if plan.args.len() > rules.limits.plan_args_items
        || plan.args.iter().map(|a| a.len()).sum::<usize>() > rules.limits.plan_args_bytes
    {
        return invalid(format!(
            "plan: at most {} arguments and {} bytes are accepted",
            rules.limits.plan_args_items, rules.limits.plan_args_bytes
        ));
    }
    if plan.args.iter().any(|a| has_nul(a)) {
        return invalid("plan: an argument contains a NUL byte");
    }
    // A build's executable can be named by setting program and binary_path to the same path.
    let built = plan
        .binary_path
        .as_deref()
        .is_some_and(|b| b == plan.program);
    let cwd = PathBuf::from(&plan.cwd);
    if !cwd.is_absolute() {
        return invalid(format!("plan: cwd '{}' is not an absolute path", plan.cwd));
    }
    let app_dir = canonical_app_dir(&spec.cwd)?;
    let cwd = checked_cwd(&app_dir, &cwd)?;
    let (extra, dropped_env) = check_env(&plan.extra_env, spec, rules, what)?;
    let binary_path = match &plan.binary_path {
        Some(p) if !Path::new(p).is_absolute() => {
            return invalid(format!("plan: binary_path '{p}' is not absolute"))
        }
        Some(p) => Some(PathBuf::from(p)),
        None => None,
    };
    let (program, invocation, executable) = if built {
        let (path, source) = check_built_binary(&plan.program, &spec.cwd, &app_dir)?;
        (path, None, Some(Arc::new(source)))
    } else if let Some((path, invocation, file)) =
        python_venv_path(&plan.program, spec, rules, &app_dir)
            .and_then(|path| open_python_venv(&app_dir, path, venv).ok())
    {
        (path, Some(invocation), Some(Arc::new(file)))
    } else {
        let (program, invocation) = resolve_program(
            &plan.program,
            &app_dir,
            program_search_path(spec, daemon_env),
            rules,
            false,
            what,
        )?;
        (program, invocation, None)
    };
    Ok(CheckedPlan {
        launch: LaunchPlan {
            program,
            args: plan.args.clone(),
            cwd,
            extra_env: extra,
        },
        invocation,
        binary_path,
        requires_build: built,
        executable,
        dropped_env,
    })
}

/// Only the app owner can select another environment directory through `VIRTUAL_ENV`.
fn python_venv_path(
    program: &str,
    spec: &AppSpec,
    rules: &RuntimeRules,
    app_dir: &Path,
) -> Option<PathBuf> {
    let path = Path::new(program);
    let name = path.file_name()?.to_str()?;
    if !path.is_absolute()
        || !matches!(name, "python" | "python3")
        || !rules.programs.iter().any(|declared| declared == name)
    {
        return None;
    }
    let relative = path
        .strip_prefix(app_dir)
        .or_else(|_| path.strip_prefix(&spec.cwd))
        .ok()?;
    if !relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        return None;
    }
    let bin = relative.parent()?;
    let venv = bin.parent()?;
    let configured = spec
        .env
        .get("VIRTUAL_ENV")
        .filter(|p| !p.is_empty())
        .map(Path::new);
    let configured = configured.map(|p| {
        p.strip_prefix(app_dir)
            .or_else(|_| p.strip_prefix(&spec.cwd))
            .unwrap_or(p)
    });
    if bin.file_name()? != "bin"
        || venv.as_os_str().is_empty()
        || (!matches!(venv.to_str(), Some(".venv" | "venv")) && configured != Some(venv))
    {
        return None;
    }
    Some(app_dir.join(relative))
}

fn open_python_venv(
    app_dir: &Path,
    invocation: PathBuf,
    probe: &mut VenvProbe,
) -> std::io::Result<(PathBuf, PathBuf, File)> {
    let invalid = || std::io::Error::other("cannot verify the virtual environment's interpreter");
    let directory = open_app_dir(app_dir)?;
    let bin = invocation
        .strip_prefix(app_dir)
        .ok()
        .and_then(Path::parent)
        .ok_or_else(invalid)?;
    let config = bin.with_file_name("pyvenv.cfg");
    if !open_beneath(&directory, &config, 0)?.metadata()?.is_file() {
        return Err(invalid());
    }
    open_beneath(&directory, bin, libc::O_DIRECTORY)?;
    let actual = invocation.canonicalize()?;
    let handle = if let Ok(local) = actual.strip_prefix(app_dir.join(bin)) {
        open_beneath(&directory, &bin.join(local), 0)?
    } else {
        let Some((device, inode)) = probe.identity else {
            if !actual.starts_with(app_dir) {
                probe.target = Some(actual);
            }
            return Err(invalid());
        };
        if probe.target.as_deref() != Some(&actual) {
            return Err(invalid());
        }
        let root = open_app_dir(Path::new("/"))?;
        let handle = open_beneath(&root, actual.strip_prefix("/").map_err(|_| invalid())?, 0)?;
        let meta = handle.metadata()?;
        if (meta.dev(), meta.ino()) != (device, inode) {
            return Err(invalid());
        }
        handle
    };
    let file = readable_executable(&handle)?;
    let mut magic = [0; 4];
    file.read_exact_at(&mut magic, 0)?;
    if &magic != b"\x7fELF" {
        return Err(invalid());
    }
    Ok((actual, invocation, file))
}

/// Open the app's executable without following links below the app directory.
fn check_built_binary(program: &str, app_dir: &Path, root: &Path) -> Result<(PathBuf, File)> {
    let path = Path::new(program);
    if !path.is_absolute() {
        return invalid(format!("plan: binary_path '{program}' is not absolute"));
    }
    let outside = || {
        Error::AdapterInvalid(format!(
            "plan: the binary '{program}' must be inside the app directory"
        ))
    };
    let actual = path.canonicalize().map_err(|e| {
        Error::AdapterInvalid(format!(
            "plan: the binary '{program}' that the build should have made is not there: {e}"
        ))
    })?;
    if !actual.starts_with(root) {
        return Err(outside());
    }
    let relative = path
        .strip_prefix(root)
        .or_else(|_| path.strip_prefix(app_dir))
        .map_err(|_| outside())?;
    let not_executable =
        || Error::AdapterInvalid(format!("plan: '{program}' is not an executable file"));
    let directory = open_app_dir(root)?;
    let handle = open_beneath(&directory, relative, 0).map_err(|_| not_executable())?;
    let meta = handle.metadata()?;
    if !meta.is_file() || meta.mode() & 0o100 == 0 {
        return Err(not_executable());
    }
    if meta.uid() != nix::unistd::geteuid().as_raw() {
        return invalid(format!(
            "plan: '{program}' is not owned by the user the daemon runs as"
        ));
    }
    let source = readable_executable(&handle).map_err(|_| not_executable())?;
    Ok((actual, source))
}

fn readable_executable(handle: &File) -> std::io::Result<File> {
    let meta = handle.metadata()?;
    use nix::sys::statvfs::{fstatvfs, FsFlags};
    if !meta.is_file()
        || meta.mode() & 0o100 == 0
        || fstatvfs(handle)?.flags().contains(FsFlags::ST_NOEXEC)
    {
        return Err(std::io::Error::other("not an executable file"));
    }
    // Reopen the handle for reading after verifying the file type and owner.
    File::open(format!("/proc/self/fd/{}", handle.as_raw_fd()))
}

fn open_app_dir(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

fn open_beneath(directory: &File, path: &Path, flags: libc::c_int) -> std::io::Result<File> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: open_how contains only integers, all of which may be zero.
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = (libc::O_PATH | libc::O_CLOEXEC | flags) as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
    // SAFETY: the directory and the arguments remain live for the entire call.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            directory.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of_val(&how),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat2 returned a new, owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd as _) })
}

/// Where the output of the steps of one build goes: the first [`BUILD_LOG_BYTES`] of it, to
/// a file that is truncated when the build starts.
pub struct BuildLog {
    file: Option<std::fs::File>,
    written: u64,
    cap: u64,
}

impl BuildLog {
    /// Create (truncate) the log file at `path`, mode 0600.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        Self::create_capped(path, BUILD_LOG_BYTES)
    }

    pub fn create_capped(path: &Path, cap: u64) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            file: Some(file),
            written: 0,
            cap,
        })
    }

    /// A log that keeps nothing.
    pub fn none() -> Self {
        Self {
            file: None,
            written: 0,
            cap: 0,
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    fn write(&mut self, data: &[u8]) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let room = self.cap.saturating_sub(self.written) as usize;
        let take = room.min(data.len());
        if take > 0 && file.write_all(&data[..take]).is_ok() {
            self.written += take as u64;
        }
        if take < data.len() {
            // the cap is reached: later output is not kept, and the file says so once
            let _ = file.write_all(b"\n[build log cut here: the cap was reached]\n");
            self.file = None;
        }
    }

    fn header(&mut self, text: &str) {
        self.write(text.as_bytes());
    }
}

/// How a step ended.
#[derive(Debug, Clone)]
pub struct StepOutcome {
    pub id: String,
    /// `None` when the step was killed by a signal (the timeout, or something else).
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The last [`STEP_TAIL_BYTES`] of the output (stdout and stderr together).
    pub tail: String,
    /// The captured stream, when the step asked for it (at most `capture_max` bytes).
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// Output bytes the step produced in total.
    pub output_bytes: u64,
}

impl StepOutcome {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

struct Tail {
    buf: Vec<u8>,
    cap: usize,
}

impl Tail {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        // let it grow to twice the cap, then cut: the copy is amortised
        if self.buf.len() > self.cap * 2 {
            let cut = self.buf.len() - self.cap;
            self.buf.drain(..cut);
        }
    }

    fn text(&self) -> String {
        let start = self.buf.len().saturating_sub(self.cap);
        String::from_utf8_lossy(&self.buf[start..]).into_owned()
    }
}

fn push_capped(buf: &mut Vec<u8>, data: &[u8], max: usize) {
    let room = max.saturating_sub(buf.len());
    buf.extend_from_slice(&data[..room.min(data.len())]);
}

/// Run one checked step in its own process group and wait for it.
///
/// Output is read in chunks and kept in three bounded places only: the tail, the build log and
/// the capture, so a step that prints without end costs no memory. The process group is
/// killed when the step ends (anything it left behind goes too), when the timeout is up, and
/// when this future is dropped (a Stop during the build).
pub async fn run_step(step: &CheckedStep, log: &mut BuildLog) -> Result<StepOutcome> {
    let mut cmd = crate::spawn::group_command(&step.program);
    if let Some(invocation) = &step.invocation {
        cmd.arg0(invocation);
    }
    let cwd = open_runtime_cwd(&step.app_dir, &step.cwd)?;
    cmd.args(&step.args)
        .current_dir(format!("/proc/self/fd/{}", cwd.as_raw_fd()));
    if step.env.clear {
        cmd.env_clear();
    }
    cmd.envs(step.env.vars.iter().map(|(k, v)| (k, v)));
    let mut child = GroupChild::spawn(&mut cmd).map_err(|e| {
        Error::ProcessStartFailed(format!(
            "step '{}': cannot start '{}': {e}",
            step.id,
            step.program.display()
        ))
    })?;
    let mut stdout = child.take_stdout();
    let mut stderr = child.take_stderr();

    log.header(&format!("== step {}: {}\n", step.id, step.description));

    let capture_out = matches!(step.capture, Capture::Stdout | Capture::Both);
    let capture_err = matches!(step.capture, Capture::Stderr | Capture::Both);
    let mut tail = Tail {
        buf: Vec::new(),
        cap: STEP_TAIL_BYTES,
    };
    let mut cap_out: Vec<u8> = Vec::new();
    let mut cap_err: Vec<u8> = Vec::new();
    let mut total: u64 = 0;

    let result = {
        let pump = async {
            let mut b1 = vec![0u8; 16 * 1024];
            let mut b2 = vec![0u8; 16 * 1024];
            let (mut out_done, mut err_done) = (stdout.is_none(), stderr.is_none());
            while !(out_done && err_done) {
                tokio::select! {
                    n = async { stdout.as_mut().unwrap().read(&mut b1).await }, if !out_done => {
                        match n {
                            Ok(n) if n > 0 => {
                                total += n as u64;
                                tail.push(&b1[..n]);
                                log.write(&b1[..n]);
                                if capture_out { push_capped(&mut cap_out, &b1[..n], step.capture_max); }
                            }
                            _ => out_done = true,
                        }
                    }
                    n = async { stderr.as_mut().unwrap().read(&mut b2).await }, if !err_done => {
                        match n {
                            Ok(n) if n > 0 => {
                                total += n as u64;
                                tail.push(&b2[..n]);
                                log.write(&b2[..n]);
                                if capture_err { push_capped(&mut cap_err, &b2[..n], step.capture_max); }
                            }
                            _ => err_done = true,
                        }
                    }
                }
            }
        };
        tokio::time::timeout(step.timeout, async {
            tokio::join!(child.finish_group(), pump).0
        })
        .await
    };

    let (exit_code, timed_out) = match result {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => {
            child.kill_and_wait().await;
            return Err(Error::ProcessStartFailed(format!(
                "step '{}': waiting failed: {e}",
                step.id
            )));
        }
        Err(_) => {
            child.kill_and_wait().await;
            (None, true)
        }
    };
    if timed_out {
        log.header(&format!(
            "\n[step {} killed: no result after {} s]\n",
            step.id,
            step.timeout.as_secs()
        ));
    }
    let text = |buf: &[u8]| String::from_utf8_lossy(buf).into_owned();
    Ok(StepOutcome {
        id: step.id.clone(),
        exit_code,
        timed_out,
        tail: tail.text(),
        stdout: capture_out.then(|| text(&cap_out)),
        stderr: capture_err.then(|| text(&cap_err)),
        output_bytes: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_output_over_cap_does_not_grow_memory_tail_buffer() {
        let mut tail = Tail {
            buf: Vec::new(),
            cap: STEP_TAIL_BYTES,
        };
        let chunk = vec![b'x'; 16 * 1024];
        for _ in 0..4096 {
            tail.push(&chunk);
            assert!(tail.buf.len() <= STEP_TAIL_BYTES * 2 + chunk.len());
        }
        assert_eq!(tail.text().len(), STEP_TAIL_BYTES);
        let mut cap = Vec::new();
        for _ in 0..100 {
            push_capped(&mut cap, &chunk, 1000);
        }
        assert_eq!(cap.len(), 1000);
    }
}
