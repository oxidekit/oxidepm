# OxidePM

OxidePM is a process manager written in Rust for any runtime. Adapters add language runtimes, tooling, and integrations.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ id │ name        │ mode  │ pid   │ ↺ │ status │ cpu  │ mem    │ uptime     │
├────┼─────────────┼───────┼───────┼───┼────────┼──────┼────────┼────────────┤
│  0 │ api         │ cargo │ 12345 │ 0 │ online │ 0.5% │ 24.2M  │ 2h         │
│  1 │ web         │ npm   │ 12346 │ 2 │ online │ 1.2% │ 128.5M │ 45m        │
│  2 │ worker      │ node  │ 12347 │ 0 │ online │ 0.1% │ 45.0M  │ 1d         │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Features

- **Multi-runtime support** — Node.js, Python, Go, Cargo, Rust, npm/pnpm/yarn, and any shell command; the language runtimes are [adapters](#runtimes) that a release carries
- **Daemon supervision** — Processes persist across terminal sessions
- **Auto-restart** — Configurable restart policies with crash-loop protection
- **Process dependencies** — `depends_on` waits until the apps it names are running, and fails closed when they are not
- **Cron restarts** — Schedule periodic restarts via cron expressions
- **Clustering** — Run multiple instances with automatic port assignment
- **Health checks** — HTTP, TCP and script-based health monitoring
- **Log management** — Per-process rotation, gzip compression, tail, follow, grep filtering
- **TUI dashboard** — Real-time monitoring with the `monit` command, from the `ui-tui` adapter
- **Web API + Prometheus** — REST API and WebSocket from the `ui-web` adapter; the `/metrics` endpoint for Grafana comes from the `exporter-prometheus` adapter
- **Multi-channel notifications** — Telegram, Discord, Slack, and signed HTTP webhooks, from the `notifier` adapter
- **Self-update** — `oxidepm update` downloads and swaps the latest release
- **JSON output** — `--json` flag for machine-readable output on all commands
- **Git clone & start** — The `git-start` adapter clones and launches projects, with optional dependency installation

## Installation

### Quick Install

The install script installs the latest release (currently 0.4.0-rc.2).

```bash
curl -fsSL https://raw.githubusercontent.com/oxidekit/oxidepm/prod/scripts/install.sh | sh
```

### APT (Debian/Ubuntu)

Download the `.deb` from the [Releases page](https://github.com/oxidekit/oxidepm/releases):

```bash
# x86_64
sudo dpkg -i oxidepm_0.4.0.rc.2_amd64.deb

# ARM64
sudo dpkg -i oxidepm_0.4.0.rc.2_arm64.deb
```

### Homebrew (macOS/Linux)

Download `oxidepm.rb` from the release and add it to a local tap:

```bash
brew tap-new local/oxidepm
cp oxidepm.rb "$(brew --repository local/oxidepm)/Formula/oxidepm.rb"
brew install local/oxidepm/oxidepm
```

On Linux, [import the bundled runtimes](#runtimes). On macOS, use the `cmd` runtime.

### Arch Linux (AUR)

```bash
yay -S oxidepm
# or
git clone https://github.com/oxidekit/oxidepm.git && cd oxidepm/dist/aur && makepkg -si
```

### From Source

```bash
git clone https://github.com/oxidekit/oxidepm
cd oxidepm
cargo build --workspace --release

# Install binaries
cargo install --path crates/oxidepm
cargo install --path crates/oxidepmd
cargo install --path crates/oxidepm-sandbox
```

A source build supports the `cmd` runtime. To use the language runtimes, copy
`share/oxidepm/adapters` from the release archive to the same path under your installation
prefix and import them as described under [Runtimes](#runtimes).

### Download binaries

Pre-built binaries available on the [Releases page](https://github.com/oxidekit/oxidepm/releases):

| Platform | Architecture | Download |
|----------|--------------|----------|
| Linux | x86_64 | `oxidepm-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | ARM64 | `oxidepm-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | Intel | `oxidepm-x86_64-apple-darwin.tar.gz` |
| macOS | Apple Silicon | `oxidepm-aarch64-apple-darwin.tar.gz` |

To install an archive, verify its digest against `checksums.txt` and extract it.
Place `oxidepm`, `oxidepmd` and `oxidepm-sandbox` in your prefix's `bin`, and copy
`share/oxidepm/adapters` to the same path under the prefix. On Linux, import the runtimes as shown below.

When upgrading from 0.3.2, keep a copy of your app configuration and stop the old daemon
before installing. The new daemon uses `state.db` and a versioned `saved.json`. Start
your apps from their configs and run `oxidepm save` to record the new state.

## Runtimes

`cmd` (a program and its arguments) is the one runtime the daemon runs by itself. `node`, `npm`,
`pnpm`, `yarn`, `python`, `go`, `rust` and `cargo` are provided by adapters (`runtime-node`,
`runtime-python`, `runtime-go` and `runtime-rust`): programs of their own that tell the daemon what
to run, while the daemon starts and supervises the app. They have no network and read none of your
files.

The release archive ships all four runtime adapters under `share/oxidepm/adapters` with
`index.json` and `SHA256SUMS`. The CLI and daemon verify the catalog and selected archives
against `SHA256SUMS` on import. The release archive's digest is listed in `checksums.txt`.
On Linux, import and enable the bundled runtimes offline with:

```bash
oxidepm adapter import --bundled --tag runtime --enable --yes
```

The install script attempts this import for you. Linux uses Landlock and seccomp where the
kernel supports them. macOS supports the built-in `cmd` runtime. The adapter launcher and
sandbox are Linux-only in this release.
Until a runtime is imported, `oxidepm start app.js` fails with the adapter's name and the command
that imports it, and creates nothing. `examples/*.config.toml` that use node, python or go work as
soon as the runtimes are imported.

What the adapters do differently from the runners they replaced: `npm run` gets `--` before the
arguments, so `--port 80` reaches the script; a python command of several words is split the way a
shell splits it and the interpreter is the project's `venv` or `.venv` when there is one; a cargo
project is built with the app's environment and a time limit, and `cargo metadata` names the binary
and the target directory, so workspaces and a `target-dir` setting work; the build shows as
`building`. A script that does not exist is reported by the program that runs it, in the app's log.

## Optional adapters

On Linux, the release bundle provides the language runtimes. For the optional tools and services below,
build an archive from the [adapter sources](https://github.com/oxidekit/oxidepm-adapters).
With the daemon running, import it using the SHA-256 from your build's `SHA256SUMS`:

```bash
oxidepm adapter import ./watch-0.4.0-rc.2.tar.gz --sha256 <digest> --allow-reserved-name --enable
```

See the adapter repository for build instructions. This release ships without a configured
network catalog.

## Quick Start

```bash
# Start a Node.js app
oxidepm start app.js

# Start a Python app (auto-detects virtualenv)
oxidepm start app.py

# Start a Go project
oxidepm start ./my-go-project

# Start from a directory (auto-detects project type)
oxidepm start ./my-project

# Start with a memory limit
oxidepm start ./my-project --max-memory 512

# Generate a config and start a project (requires the init adapter)
oxidepm init ./my-project
oxidepm start ./my-project/oxidepm.config.toml

# Clone and start from GitHub (requires the git-start adapter)
oxidepm git-start https://github.com/user/repo

# View status
oxidepm status

# Real-time resource monitor
oxidepm top

# View logs (with follow)
oxidepm logs my-app -f

# Deploy to production (requires the deploy adapter)
oxidepm deploy user@server --cwd /srv/myapp --post "npm run build"

# Stop a process
oxidepm stop my-app
```

## Commands

| Command | Description |
|---------|-------------|
| `start <target> [--check]` | Start a process, or every app in a config file. For a single target, `--check` runs preflight through the check adapter |
| `git-start <url>` | Clone or update a repository and start it through the git-start adapter |
| `stop <selector>` | Stop process(es) |
| `restart <selector>` | Hard restart process(es) |
| `reload <selector>` | The same as `restart` (there is no zero-downtime reload) |
| `delete <selector>` | Remove from registry |
| `status [--more\|--daemon]` | Show status table (`--more` adds cwd, port and the policy markers; `--daemon` shows the daemon itself: audit log state, agent socket, warnings) |
| `policy set <selector> [--protected[=bool]] [--ai-control[=bool]]` | Change the policy markers of running apps without a restart, a cluster with its instances (see [AI-SAFETY](docs/AI-SAFETY.md)) |
| `logs <name> [-f]` | View/follow logs |
| `audit [--tail N] [--actor X] [--op X] [--since T] [--security]` | Read the audit log (works with the daemon down; `--json` prints one object per line; see [AI-SAFETY](docs/AI-SAFETY.md)) |
| `show <name> [--reveal]` | Detailed process info (`--reveal` also prints environment values and hook scripts) |
| `monit` | Provided by the `ui-tui` adapter ([install the adapter](#optional-adapters)): the terminal dashboard (a session verb that runs on your terminal) |
| `save [--force]` | Write `saved.json` from the apps that should be running (refused when that would blank a file that lists apps, unless `--force`) |
| `resurrect` | Start the apps that should be running and are not up (an app you stopped stays stopped), and add apps from `saved.json` that were deleted since |
| `startup [systemd\|launchd]` | Generate autostart script. The systemd unit sets `TimeoutStopSec` to the daemon's shutdown budget (`daemon.toml`, 120 s by default) plus 40 s, and `KillMode=mixed`, so systemd sends the stop signal to the daemon alone and leaves it time to stop the apps in order; run `startup` again to get this in an existing unit |
| `check [target] [--fix] [--strict] [--set-env KEY=VALUE]` | Check project readiness through the check adapter |
| `flush <selector>` | Truncate the active log files (rotated files stay); a file that cannot be emptied is reported for that app |
| `describe <target>` | Show command without starting |
| `web [status\|new-key]` | Provided by the `ui-web` adapter ([install the adapter](#optional-adapters)): say where the web API listens, print its access key once, or replace the key |
| `notify status\|test\|events\|severity\|telegram\|remove` | Provided by the `notifier` adapter ([install the adapter](#optional-adapters)): show, test and configure the notification channels |
| `ping` | Check daemon health |
| `update [--version X] [--force]` | Self-update to the latest stable release or an explicit version such as `0.4.0-rc.2`: the archive must match the sha256 in the release's `checksums.txt` (integrity; there is no release signing key yet, so authenticity is not checked), the asset is the one named exactly `oxidepm-<target>.tar.gz`, it is unpacked safely, a running daemon is stopped, the files are replaced atomically with the old ones kept as `.pre-update`, and the new `oxidepm --version` is run (everything is put back if that fails). An older version needs `--force` |
| `init [dir]` | Generate a project config through the init adapter |
| `deploy <host>` | Pull code and restart processes over SSH through the deploy adapter |
| `diff <host>` | Show recent remote commits and working-tree changes through the deploy adapter |
| `env list/set/unset` | `list` shows the keys of an app's environment; `set` and `unset` are refused (the daemon cannot change a saved environment: use `--env` or the config file) |
| `top` | Real-time process resource monitor |
| `backup [--output file]` | Export config + process list to tarball |
| `restore <file>` | Restore from backup tarball (the daemon must not be running) |
| `completions <shell>` | Generate shell completions (bash/zsh/fish) |
| `man` | Generate man page |
| `kill` | Stop daemon and all processes |

**Global flags:** `--json` for machine-readable output, `-v` for verbose logging, `--dry-run` to see what a command would do and change nothing.

**Dry run:** `--dry-run` works on `start`, `stop`, `restart`, `reload`, `delete`, `flush`, `save`,
`resurrect`, `kill` and `policy set`. The daemon returns a plan with targets, order and refusals.
`startup`, `update`, `backup` and `restore` also support it. An update preview verifies and unpacks
the download in a temporary directory, then removes it. `start --check` cannot be combined with
`--dry-run`.

The `init`, `git-start`, `deploy` and `diff` adapters run their previews through a running daemon.
`check` has no dry run. Use plain `check` to report findings without making project changes.

**Selectors:** Process name, ID, `all`, or `@tag` for groups.

## Start Options

### Git clone and start

Start the daemon with `oxidepm resurrect` if it is down, then import and enable the [`git-start`
adapter](#optional-adapters). The adapter keeps clones in its own state
directory and prints the commit it prepared. It fetches and updates an existing checkout when
online. `--offline` uses an existing checkout. `--install` opts into npm dependency installation.
Lifecycle scripts require `--allow-scripts` as well. `--no-start` prepares the checkout without
starting an app, and `--dry-run` prints the plan without cloning, installing or starting.

```bash
oxidepm git-start https://github.com/user/repo
oxidepm git-start https://github.com/user/repo --branch develop --install --no-start
oxidepm --dry-run git-start https://github.com/user/repo
```

### Process Configuration

```bash
oxidepm start ./app \
  --name my-app \
  --env NODE_ENV=production \
  --env-file .env.production \
  --env-inherit \
  --tag api \
  --delay 5000 \
  --max-uptime 24h \
  --max-restarts 10 \
  --restart-delay 1000
```

### File watching

Import and enable the [`watch` adapter](#optional-adapters).
Opt in for each app in its config. Paths are relative to the app's working directory.

```toml
[apps.adapters.watch]
paths = ["src"]
ignore = ["generated/*"]
debounce_ms = 200
```

Omit `paths` to watch the app's working directory. The adapter adds your patterns to
its default ignores, including `.git`, `node_modules` and `target`. Replace the
legacy `--watch`, `watch` and `ignore` settings with this table.

An app does not get the daemon's environment: it gets `PATH`, `HOME`, `USER`, `LOGNAME`,
`SHELL`, `LANG`, `LC_*`, `TZ` and `TMPDIR`, then `--env` and `--env-file`, and the daemon sets
`OPM_APP_ID` and `OPM_APP_NAME` (`OPM_INSTANCE_ID` for a cluster instance). `--env-inherit` gives the
app the daemon's whole environment instead (the environment of the daemon, not of the shell
that ran `oxidepm start`). Build steps (`cargo build`, `rustc`) always get the daemon's environment.

### Clustering

```bash
# Run 4 instances with automatic port assignment
oxidepm start ./server -i 4 --port 3000
```

A cluster is a parent (`web`, no process) and one app per instance (`web-0`, `web-1`, ...); at most 256 instances and a cluster name of at most 59 characters. Instance `i` gets port `port + i`, or `port_range` start `+ i` (the range must hold every instance, and `port` and `port_range` together are refused). The cluster is created all at once: a name that is already taken (`web-1` included) refuses the start with `E_APP_EXISTS` and writes nothing, and if an instance cannot launch on that first start the whole cluster is deleted again and the instance's error is returned. Starting a cluster that exists starts only the instances that are at rest (`stop web-1`, then `start web` brings back `web-1` and leaves the others running); if one of them fails to launch it stays `errored` and the others are not touched. The number of instances cannot change (`E_INVALID_SPEC`: delete and recreate), and that includes starting an existing single app with `-i` greater than 1, or an existing cluster with one instance: the start is refused before anything else is looked at, whether the app is up or at rest. `delete` of a cluster removes the parent and all instance rows in one transaction before any instance is stopped. A tag or `all` that matches a cluster and its instances acts on the cluster once; `web-1` by name acts on that instance only.

### Process states

`status` shows one of: `waiting` (a start is accepted and waits for `depends_on` or `startup_delay_ms`), `building`, `starting` (spawned, health check not yet passed), `online` (`running`), `unhealthy` (alive, health check failing), `backoff` (crashed, an automatic restart is scheduled), `stopping`, `stopped` and `errored` (crash loop, launch failure or an exit that is not restarted). A stop acts on every state except `stopped` and `errored`: it also cancels a pending start, a build and a pending restart. `reload` is a restart; there is no zero-downtime reload. `start` of an app that is not at rest (anything but `stopped` and `errored`, `backoff` and `waiting` included) is refused with `E_APP_EXISTS` and changes nothing.

### Crash restarts

A crashed app waits in `backoff` and is restarted automatically unless `--no-autorestart` or the config key `restart` says otherwise. `restart` is `on-crash` (the default: a non-zero exit or death by a signal is restarted, an exit with code 0 is not and leaves the app `errored`), `always` (any exit is restarted) or `never`. A restart caused by a memory or uptime limit, by a health check with `action = "restart"` or by a launch that failed during a restart ignores both settings. The delay is `restart_delay` (default 500 ms) for the first restart and doubles for every crash in a row, up to `max_restart_delay` (default 60000 ms). A run of `stable_uptime` seconds (default 60) starts the delay over. More than `max_restarts` (default 15) restarts inside any `crash_window` seconds (default 600) are a crash loop: the app ends `errored` with `last_error = "crash loop: N exits in W s"`. A launch that fails during an automatic restart (the program is gone, a log file cannot be opened) is one more attempt with a longer delay, not the end. `start` and `restart` by hand give the app a fresh budget. The daemon's status data carries the attempt, the time of the next restart and `last_error`. In a config file the keys are `restart_delay`, `max_restart_delay` (ms), `stable_uptime` and `crash_window` (seconds).

### Dependencies

`depends_on = ["db"]` (config file) makes an app wait for the apps it names. The checks run when the app is started:

- a name that is not registered is refused at once (`E_DEPENDENCY_MISSING`), so is a cycle (`E_DEPENDENCY_CYCLE`, the message names it), and a single `start` of an app whose dependency is `stopped` or `errored` right now is refused with the remedy (`E_DEPENDENCY_FAILED`: "dependency 'db' is stopped; start it first"). This check is skipped for an app that is already up (or a cluster whose instances all are): starting it again is answered `E_APP_EXISTS` whatever its dependencies are doing, in a single start and for each app of a config file started again;
- otherwise the start is accepted and the app shows `waiting` until every dependency is `running`. The start returns at once, nothing else is blocked, and a `stop` cancels the wait.

The wait fails closed. A dependency that ends `errored` (or is deleted) fails the dependent at once, and so does `depends_timeout` (seconds, 1 to 3600, default 60): the app ends `errored` with `last_error = "dependency 'db' not running after 60s"` (`E_DEPENDENCY_TIMEOUT`) and is never launched. `depends_fail_open = true` turns that timeout into a warning and a launch; it is discouraged. Once an app is running, a later failure of its dependency does not touch it.

The wait applies to a start or restart by hand of an app that was at rest. A `restart` of an app that is up, and every automatic restart (crash, limit, cron, health), skip it: a crashed app comes back through the normal backoff even while its dependency is down. An app that waits holds no build slot; at most `launch_concurrency` apps (default 4, `daemon.toml`, 1 to 64) build and spawn at once, so a reboot does not run twenty builds together.

`oxidepm start <config file>` sends the whole file as one batch, so an app may be listed before its dependencies; each app gets its own result (an app that cannot start does not stop the others, but the apps that depend on it are refused too). `resurrect` starts the saved apps the same way. `stop all` (and a tag) stops dependents before their dependencies, layer by layer; the daemon's shutdown does the same within its time budget.

### Memory and uptime limits

`--max-memory <MB>` and `--max-uptime <duration>` (`max_memory_mb`, `max_uptime_secs` in a config file) replace a child that is over the limit: the daemon stops its process group, waits the restart delay and starts it again with the same plan. A limit restart ignores the `restart` setting and `no_autorestart`, and a run of 5 seconds or more never counts towards `max_restarts`, so a periodic restart (`--max-uptime 24h`) cannot end in a crash loop; a limit that fires within 5 seconds of every start is counted like a crash and the breaker ends it. A `stop` or `restart` by hand always wins over a limit restart that is still on its way. Memory is sampled every 2 seconds from the resident size of the app's first process (the one the daemon started): with `npm start` that is `npm`, not the `node` child under it, so give a wrapped app the limit of the wrapper or start the program directly. Uptime counts from the launch.

### Health Checks

```bash
# HTTP health check
oxidepm start ./server --health-check http://localhost:3000/health

# Script-based health check
oxidepm start ./server --health-check ./check-health.sh
```

A check runs every `interval_secs` (default 30), a probe times out after `timeout_secs` (default 5) and an HTTP check passes on status 200 unless `expected_status` lists others. An HTTP check does not follow redirects: a 301 is a failure unless it is listed in `expected_status`. In a config file a check is exactly one of `http_url`, `tcp` (`"host:port"`, healthy when the connection is accepted) or `script`.

An app with a check is `starting` until its first probe has passed; the first probe runs `start_period_secs` (default 5) after the launch, then one every `interval_secs`. After `retries` failures in a row the app is `unhealthy` (the hooks `on_error` and the `health_check` notification fire once for that streak) and goes back to `running` when a probe passes again. The child is left alone unless the check sets `action = "restart"`, which replaces it with an automatic restart.

### Event Hooks

```bash
oxidepm start ./app \
  --on-start "./notify.sh started" \
  --on-stop "./notify.sh stopped" \
  --on-crash "./notify.sh crashed" \
  --on-restart "./notify.sh restarted"
```

The command line has the four hooks above; a config file also has `on_error`, which runs when the app ends `errored` or turns `unhealthy`. A hook runs as `sh -c` in its own process group and is killed with everything it started after 30 seconds. Its output is appended to `~/.oxidepm/logs/<name>-hooks.log`. It gets `PATH`, `HOME` and a few other basics from the daemon, the app's own `env`, and `OPM_APP_ID`, `OPM_APP_NAME`, `OPM_EVENT`, `OPM_PID`, `OPM_EXIT_CODE`, `OPM_REASON` and `OPM_RESTARTS` (where they apply); the daemon's other environment variables are not passed on, not even with `--env-inherit`. Script health checks run the same way, in the app's directory.

## Configuration File

Create `oxidepm.config.toml` or `ecosystem.config.toml`:

```toml
[[apps]]
name = "api"
mode = "cargo"
cwd = "./api"
env = { RUST_LOG = "info" }
tags = ["backend"]

[[apps]]
name = "web"
mode = "npm"
script = "start"
cwd = "./web"
env_file = ".env"
instances = 2
port = 3000

[[apps]]
name = "worker"
mode = "node"
script = "worker.js"
max_restarts = 5
restart_delay = 2000
depends_on = ["api"]              # Wait for API to be running first (see Dependencies)
restart_cron = "0 4 * * *"        # Restart daily at 4am

# Log rotation (optional — defaults: 10MB, 5 files, no compression)
log_max_size = "50mb"
log_max_files = 10
log_compress = true
```

Start all apps:

```bash
oxidepm start oxidepm.config.toml
```

Also supports YAML and JSON formats. Config files are checked strictly: an unknown
key, a bad cron expression or log size, an unusable health check, a missing
`env_file`, or a `script` whose runtime cannot be inferred from its extension is
reported with the app name before anything starts. Set `runtime` or its alias `mode`
when the extension is ambiguous. Cron expressions use the usual 5 fields (UTC; Sunday is 0 or 7).

The keys of an app are `name`, `runtime` (alias `mode`), `script`, `bin`, `binary`, `cwd`, `args`, `env`,
`env_file`, `instances`, `port`, `port_range` (`start`, `end`), `tags`, `depends_on`,
`depends_timeout`, `depends_fail_open`, `restart` (`on-crash`, `always`, `never`),
`no_autorestart`, `max_restarts`, `restart_delay`, `max_restart_delay`, `stable_uptime`,
`crash_window`, `kill_timeout`, `max_memory_mb`, `max_uptime_secs`, `restart_cron`,
`log_max_size`, `log_max_files`, `log_compress`, `protected`, `ai_control` (see
[AI-SAFETY](docs/AI-SAFETY.md)), the tables `health_check` (`http_url`,
`tcp` or `script`, plus `expected_status`, `interval_secs`, `timeout_secs`, `retries`,
`start_period_secs`, `action`) and `hooks` (`on_start`, `on_stop`, `on_restart`,
`on_crash`, `on_error`), and per-app adapter options such as `[apps.adapters.watch]`.
Use a single-app `start` for `--delay` and `--env-inherit`. The top-level
`adapters` and `policy` tables are reserved and must be empty. Move legacy
`watch`, `ignore` and `ignore_patterns` to the watch adapter, and use `git-start`
for Git checkouts. For legacy `setup`, run `oxidepm check --fix` separately.
Preflight is opt-in through `start --check`, so remove old `no_check` keys.

A config file is read only by `oxidepm start <file>`. The daemon keeps the spec an app was
started with: `restart` and `reload` reuse it, and `start` of an app that is up is refused.
To apply a changed file, `stop` the app and start the file again (an app that is at rest
takes the new spec).

`daemon.toml` in the data directory holds the daemon's own settings: `shutdown_budget_secs`
(10 to 900, default 120), `launch_concurrency` (1 to 64, default 4) and `agent_socket`
(default `true`, see [AI-SAFETY](docs/AI-SAFETY.md)) and `reuse_plan` (default `true`): an app
whose runtime an adapter provides keeps the launch plan the adapter made, and a resurrect at
boot or an automatic launch may start from it when the app, the adapter and its executable are
unchanged (at once when the plan had no build steps, otherwise only when the adapter cannot be
asked, and then with a warning); `false` always asks the adapter. A start or restart by a person
never uses a stored plan. An unknown key or a value out of range keeps the daemon from starting.

## Project init

Start the daemon and enable the [`init` adapter](#optional-adapters).
It detects Cargo, Node.js, Python and Go projects and writes `oxidepm.config.toml` within
the current directory or a subdirectory. An existing config is preserved.

```bash
oxidepm --dry-run init ./my-project
oxidepm init ./my-project
```

## Deploy

Start the daemon and enable the [`deploy` adapter](#optional-adapters).
Both `deploy` and `diff` require noninteractive SSH authentication and a host key already in
`known_hosts`. If you use an SSH agent, set `SSH_AUTH_SOCK` in the daemon's environment.
An enforced pathname socket grant requires Linux Landlock ABI 9 or later and a socket owned
by the daemon user outside the OxidePM home.

```bash
oxidepm deploy user@myserver --cwd /srv/myapp
oxidepm deploy user@myserver --cwd /srv/myapp --branch main --post "npm run build" --restart api
oxidepm --dry-run deploy user@myserver --cwd /srv/myapp
```

Deploy pulls from `origin` using the current branch on the remote checkout unless
`--branch` is given. It runs the optional post-deploy command and then restarts the chosen
selector (default `all`) using `oxidepm` or the path given by `--remote-bin`. A restart
failure exits with status 3 after any successful pull and post-deploy command. A dry run
shows the SSH commands without connecting.

## Environment Management

```bash
# List environment variables for a process
oxidepm env list my-app

# Set or remove a variable: refused for now
oxidepm env set my-app NODE_ENV=production
oxidepm env unset my-app DEBUG
```

`env set` and `env unset` print an error and change nothing, because the daemon has no request that changes the environment of a saved app. Pass `--env KEY=VALUE` when starting the app, or set `env` in the config file and start the file again.

`status`, `show`, `describe` and `env list` do not show environment values: the daemon replaces every value with `<redacted>` (keys stay visible), and hook and health-check scripts show as `<set>`. `oxidepm show <name> --reveal` is the one way to see them; the daemon writes a line to its log each time it is asked to.

## Preflight checks

Start the daemon and enable the [`check` adapter](#optional-adapters).
It checks dependencies, lockfiles and environment files, and reports configured ports.
A loopback listener is reported as a conflict. Checking other interfaces and suggesting
a free port require permission to bind test sockets. If the sandbox denies those binds,
an inconclusive check is a warning. Add `--strict` to fail on warnings.

```bash
oxidepm check ./my-app --strict
oxidepm start ./my-app --check
(cd my-app && oxidepm check --fix --set-env PORT=3000)
```

`start --check` runs preflight before starting a single target and stops if it fails.
For a config file, run `check` on the project separately. `--fix` can install Node dependencies
and create `.env` from a template. `--set-env KEY=VALUE` sets an entry in `.env`.
Run these write options from the project directory or a parent directory.

## Notifications

[Import and enable the `notifier` adapter](#optional-adapters) to send notifications.

It listens to the lifecycle events of the daemon (start, stop, restart, crash, crash loop, memory
limit, failed health check) and sends them to Telegram, Discord, Slack and a signed HTTP webhook.
Its configuration is `config.toml` in its own state directory (`oxidepm notify status` prints the
path), not in the home directory, and `oxidepm backup` does not include it because it holds tokens:

```toml
events = ["crash", "restart"]   # empty or absent: every event
min_severity = "warning"        # info | warning | critical

[telegram]
bot_token_env = "OXIDEPM_TELEGRAM_TOKEN"   # read from the daemon's environment; or bot_token = "..."
chat_id = "-100123456789"

[discord]
webhook_url = "https://discord.com/api/webhooks/123456/abcdef..."

[slack]
webhook_url = "https://hooks.slack.com/services/T00/B00/xxx..."

[webhook]
url = "https://your-server.com/alerts"
secret = "your-hmac-secret"   # signs every request (X-OxidePM-Signature)
```

```bash
oxidepm notify telegram --chat YOUR_CHAT_ID   # the token comes from OXIDEPM_TELEGRAM_TOKEN or bot_token; never from the command line
oxidepm notify events --set crash,restart,health_check
oxidepm notify severity warning
oxidepm notify test      # sends to every channel whatever the filters say; exits non-zero if one fails
oxidepm notify status    # the channels, the filters, and whether the file is usable
```

Without the adapter `oxidepm notify` prints how to install it and no notifications are sent.

## Web API & Prometheus

The REST API and the WebSocket are served by the `ui-web` adapter, a separate program that `oxidepmd` supervises. Every call it receives is passed to the daemon as the actor `adapter:ui-web`, so the daemon's policy and audit log apply to it. [Import and enable the adapter](#optional-adapters).

```bash
oxidepm web              # where it listens, and its access key (shown once)
```

* It listens on `127.0.0.1:9615`. Every request, loopback included, needs the access key (`X-API-Key` header); only `/api/health`, which answers `{"ok":true}`, needs none. The first start makes a random key in the adapter's `config.toml` (mode 0600); `oxidepm web new-key` replaces it. A browser WebSocket sends the key as the subprotocol `oxidepm-key.<key>` (offered together with `oxidepm.v1`), never in the URL.
* The `Host` header must be the bound address, and `Origin` must be absent or the one `origin` set in `config.toml`, on every request that changes something and on the WebSocket upgrade; so a web page you visit cannot drive the API, even by DNS rebinding.
* By default it works on the apps that list `ui-web` under `adapters` (the scope `opted-in`), at the `lifecycle` level: it shows every app and restarts, stops and reloads the opted-in ones, and **cannot create apps, delete them or flush logs** (`POST /api/processes` answers 403 with the reason). `DELETE /api/processes/all` needs `{"confirm":"all"}` or `?confirm=all`.
* There is no TLS: put a reverse proxy in front for anything but a trusted network, and list its host name under `allowed_hosts` in `config.toml`.
* The adapter's `/metrics` is gone from here: Prometheus metrics come from the `exporter-prometheus` adapter (below).

### Endpoints

| Endpoint | Method | Key | Description |
|----------|--------|------|-------------|
| `/api/health` | GET | No | `{"ok":true}` |
| `/api/processes` | GET | Yes | All processes status (asked of the daemon once a second at most) |
| `/api/processes` | POST | Yes | Start an app (`name`, `command`, absolute `cwd`, `runtime` (default `cmd`), `args`, `env`, `instances`, `port`); needs the adapter to have the `admin` level, otherwise 403 |
| `/api/processes/:selector` | GET | Yes | Single process details |
| `/api/processes/:selector` | DELETE | Yes | Delete process (the daemon refuses it below the `admin` level) |
| `/api/processes/:selector/stop` | POST | Yes | Stop process |
| `/api/processes/:selector/restart` | POST | Yes | Restart process |
| `/api/processes/:selector/logs?lines=N` | GET | Yes | Process logs; `@tag` returns the lines of every app with the tag |
| `/api/ping` | GET | Yes | Ask the daemon whether it is alive |
| `/api/save` | POST | Yes | Save process list |
| `/api/resurrect` | POST | Yes | Start the saved apps that are not up |
| `/ws` | WebSocket | Yes | The daemon's lifecycle events as they happen (`{"type":"event","kind":"stopped","app":{...},...}`) |

Errors are `{"success":false,"error":"...","code":"E_..."}` with the HTTP status the code deserves (403 `E_POLICY_DENIED`, 404, 429, 503 when the daemon cannot be reached).

### Prometheus Metrics

Prometheus metrics are served by the `exporter-prometheus` adapter, a separate program that `oxidepmd` supervises. It has read access only: on each scrape it asks the daemon for the status of the apps (at most once a second) and never touches them.

[Import and enable the adapter](#optional-adapters).

It listens on `127.0.0.1:9616` and exports the Prometheus text exposition format:

```
oxidepm_up 1
oxidepm_processes_total 3
oxidepm_process_up{name="api",id="0",mode="node"} 1
oxidepm_process_uptime_seconds{name="api",id="0",mode="node"} 86400
oxidepm_process_restarts_total{name="api",id="0",mode="node"} 0
oxidepm_process_memory_bytes{name="api",id="0",mode="node"} 524288000
oxidepm_process_cpu_percent{name="api",id="0",mode="node"} 2.30
```

Label values are escaped. If the daemon cannot be asked the answer is HTTP 503 with only `oxidepm_up 0`, so a stopped daemon does not look like a healthy host with no apps. `/metrics` is the only path. An address that is not loopback needs a `key` in the adapter's `config.toml` and every request must then send `Authorization: Bearer <key>`; without a key the `Host` header must be a loopback name. Add to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'oxidepm'
    static_configs:
      - targets: ['127.0.0.1:9616']
```

## VS Code Extension

Manage OxidePM processes directly from VS Code with the [OxidePM extension](https://github.com/oxidekit/oxidepm-vscode):

- Sidebar process explorer with status, port, CPU, memory, uptime
- Start / Stop / Restart via context menu and command palette
- Real-time log viewer via WebSocket streaming
- Status bar showing online/total process count
- Crash and restart notifications with action buttons

**Install:**
```bash
# Download .vsix from releases and install
code --install-extension oxidepm-0.1.1.vsix

# Or clone and build from source
git clone https://github.com/oxidekit/oxidepm-vscode
cd oxidepm-vscode && npm install && npm run build
npx vsce package && code --install-extension oxidepm-*.vsix
```

Requires the `ui-web` adapter to be running (`oxidepm web` shows where it listens and prints its access key once).

## TUI Dashboard

The dashboard is the `ui-tui` adapter, a separate program that `oxidepmd` starts on your terminal for as long as the dashboard is open. It stops and restarts apps as the actor `adapter:ui-tui`, so the daemon's policy and audit log apply to it, and it asks for `y` or `n` before it does either.

[Import and enable the adapter](#optional-adapters), then run `oxidepm monit`.

Real-time monitoring dashboard with:
- Process list with CPU and memory
- Details of the selected app
- Log viewer that follows the log and scrolls (arrow keys, PageUp, PageDown, End)
- Stop and restart, each confirmed with `y`
- Errors from the daemon in the status line (a protected app, a refused request)
- Keyboard navigation: `Tab` switches the view, `q` quits

It needs a terminal for both input and output, and it takes no arguments.

## Shell Completions

```bash
# Bash
oxidepm completions bash > /etc/bash_completion.d/oxidepm

# Zsh
oxidepm completions zsh > ~/.zfunc/_oxidepm

# Fish
oxidepm completions fish > ~/.config/fish/completions/oxidepm.fish
```

## Backup & Restore

Migrate your config to another machine:

```bash
# Export
oxidepm backup --output my-server-backup.tar.gz

# Transfer and restore
scp my-server-backup.tar.gz user@newserver:~
ssh user@newserver "oxidepm restore my-server-backup.tar.gz && oxidepm resurrect"
```

## Remote Diff

Use the enabled deploy adapter to show recent commits, status and a diff summary over SSH.

```bash
oxidepm diff user@myserver --cwd /srv/myapp --count 10
oxidepm --dry-run diff user@myserver --cwd /srv/myapp
```

## Docker

```bash
docker build -t oxidepm .
docker run -d --name oxidepm-daemon oxidepm
```

For a build with the bundled runtimes, pass the release download directory as
`--build-arg ADAPTER_BUNDLE_URL=<release-download-url>` and import the runtimes inside the container.
Install the interpreters and toolchains your apps use.

## Architecture

```
oxidepm (CLI) ──IPC──> oxidepmd (daemon)
                           │
                    ┌──────┴──────┐
                    │   SQLite    │
                    │  ~/.oxidepm │
                    └─────────────┘
```

**Crates:**
- `oxidepm` - CLI binary
- `oxidepmd` - Daemon/supervisor
- `oxidepm-core` - Types, config, process spec
- `oxidepm-ipc` - Unix socket protocol (see below)
- `oxidepm-runtime` - Command execution and process groups
- `oxidepm-adapter-proto` - Adapter messages and manifests
- `oxidepm-sandbox` - Adapter launcher and sandbox profiles
- `oxidepm-logs` - Log rotation + streaming
- `oxidepm-db` - SQLite persistence
- `oxidepm-health` - Health check monitoring (HTTP, TCP, script)

**IPC:** the CLI and the daemon talk over the Unix socket `daemon.sock` (mode 0600; the daemon drops connections from any other user; `OXIDEPM_SOCKET` names another socket for the CLI). A request is one line of JSON in an envelope that carries the protocol version (2) and a request id, and the answer is one line; a line may be up to 10 MiB and a connection that sends nothing for 5 minutes is closed. The daemon also binds `agent.sock`, whose every connection counts as an AI agent (see [AI-SAFETY](docs/AI-SAFETY.md)); there a line may be up to 1 MiB, 32 connections are served at a time and a connection that sends nothing for 60 seconds is closed. A config file is sent as one `StartBatch` request that answers per app, so the CLI and the daemon must come from the same release for `oxidepm start <config file>`. The daemon takes an exclusive lock on `oxidepmd.lock`, so there is one daemon per home.

## Data Directory

All data stored in `~/.oxidepm/` (the `OXIDEPM_HOME` environment variable names another directory):

```
~/.oxidepm/
├── daemon.sock     # IPC socket
├── agent.sock      # IPC socket for AI agents (every connection is class ai)
├── oxidepmd.lock   # held by the running daemon
├── daemon.toml     # optional daemon settings (shutdown_budget_secs, launch_concurrency, agent_socket, reuse_plan)
├── state.db        # SQLite database (full app specs and the two policy markers, schema version 2; an old oxidepm.db is not migrated)
├── daemon.log      # Fatal daemon startup errors
├── saved.json      # Saved process list (version 1; written atomically, mode 0600; a file from an older release is refused: run `oxidepm save` again)
├── pids.json       # Live app processes; if the daemon dies by SIGKILL, the next daemon kills what is still listed here before it starts anything
├── adapters/state/git-start/clones/<hash>  # Git checkouts
└── logs/           # Process log files
    ├── app-out.log
    ├── app-err.log
    └── app-hooks.log   # output of the app's hooks
```

## Comparison with PM2

### Feature Comparison

| Feature | OxidePM | PM2 | Notes |
|---------|:-------:|:---:|-------|
| **Runtime Support** |
| Node.js apps | ✅ | ✅ | OxidePM uses the runtime-node adapter |
| npm/yarn/pnpm scripts | ✅ | ✅ | Run package.json scripts |
| Python apps | ✅ | ❌ | Auto-detects virtualenv, unbuffered output |
| Go projects | ✅ | ❌ | `go run` with module detection |
| Rust/Cargo projects | ✅ | ❌ | OxidePM auto-builds and runs Cargo projects |
| Generic commands | ✅ | ✅ | Run any shell command |
| **Process Management** |
| Daemon supervision | ✅ | ✅ | Processes persist across terminal sessions |
| Auto-restart on crash | ✅ | ✅ | Configurable restart policies |
| Crash-loop protection | ✅ | ✅ | Exponential backoff on repeated crashes |
| Graceful reload | ❌ | ✅ | `oxidepm reload` is a plain restart |
| Clustering | ✅ | ✅ | Run multiple instances |
| **Developer Experience** |
| Watch mode | ✅ | ✅ | Opt in per app with the watch adapter |
| Port conflict detection | ✅ | ❌ | The check adapter checks loopback listeners and probes binds where permitted |
| Preflight checks | ✅ | ❌ | Use the check adapter, with optional `start --check` |
| Git clone & start | ✅ | ❌ | The git-start adapter clones and optionally installs dependencies |
| Project init | ✅ | ❌ | The init adapter scans and generates a config |
| SSH deploy | ✅ | ✅ | The deploy adapter adds `deploy` and `diff` |
| Real-time top | ✅ | ❌ | `oxidepm top` — htop for processes |
| Env management | ✅ | ✅ | `oxidepm env list/set/unset` |
| Project setup | ✅ | ❌ | `check --fix` can install Node dependencies and create `.env` |
| Event hooks | ✅ | ✅ | Run scripts on start/stop/crash |
| **Monitoring** |
| Status table | ✅ | ✅ | CPU, memory, uptime display |
| TUI dashboard | ✅ | ✅ | Real-time terminal UI (`ui-tui` adapter) |
| Log management | ✅ | ✅ | Per-process rotation, gzip compression, follow, grep |
| Health checks (HTTP) | ✅ | 💰 | PM2 requires Plus subscription |
| Health checks (Script) | ✅ | 💰 | PM2 requires Plus subscription |
| Prometheus metrics | ✅ | 💰 | PM2 requires Plus subscription |
| **Integrations** |
| Web API / REST | ✅ | 💰 | PM2 requires Plus subscription |
| WebSocket real-time | ✅ | 💰 | PM2 requires Plus subscription |
| Telegram alerts | ✅ | ❌ | Native integration |
| Discord/Slack webhooks | ✅ | ❌ | Native webhook integration |
| Generic HTTP webhooks | ✅ | ❌ | POST JSON to any URL |
| JSON output (`--json`) | ✅ | ✅ | Machine-readable output |
| Self-update | ✅ | ✅ | `oxidepm update` |
| **Advanced** |
| Process dependencies | ✅ | ❌ | `depends_on = ["postgres"]` |
| Cron restarts | ✅ | ❌ | `restart_cron = "0 4 * * *"` |
| Systemd/launchd | ✅ | ✅ | Auto-start on boot |
| **Configuration** |
| TOML config | ✅ | ❌ | Clean, readable config format |
| YAML config | ✅ | ✅ | Ecosystem file support |
| JSON config | ✅ | ✅ | Ecosystem file support |
| Save/Resurrect | ✅ | ✅ | Save and restore process list |

### Why Choose OxidePM?

**Choose OxidePM if you:**
- Work with Rust projects (the runtime-rust adapter handles Cargo builds)
- Want health checks and web API without a subscription
- Want opt-in project preflight checks
- Want to clone and run projects from git in one command

**Choose PM2 if you:**
- Need the PM2 ecosystem (pm2.io, container support)
- Require Windows support
- Have existing PM2 configurations you don't want to migrate
- Need PM2-specific features like the APM dashboard

## Requirements

- Linux for all features. macOS for the built-in `cmd` runtime.
- A current stable Rust toolchain (only for building from source)
- Runtimes only needed for their respective app types:
  - Node.js for Node/npm/pnpm/yarn apps
  - Python 3 for Python apps
  - Go 1.21+ for Go apps

## Contributing

Contributions welcome! Please open an issue or PR.

## License

Licensed under either the [MIT License](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option.
