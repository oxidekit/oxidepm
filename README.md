# OxidePM

A fast, modern process manager for Node.js, Python, Go, Rust, and Cosmos SDK applications. Built in Rust for reliability and performance.

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

- **Multi-runtime support** — Node.js, Python, Go, Cargo, Rust, npm/pnpm/yarn, Cosmos SDK nodes
- **Cosmos node management** — Relay-until-synced lifecycle, upgrade halt detection, double-sign protection
- **Daemon supervision** — Processes persist across terminal sessions
- **Auto-restart** — Configurable restart policies with crash-loop protection
- **Process dependencies** — `depends_on` waits for other processes before starting
- **Cron restarts** — Schedule periodic restarts via cron expressions
- **Watch mode** — Automatic rebuild and restart on file changes
- **Clustering** — Run multiple instances with automatic port assignment
- **Health checks** — HTTP, script-based, and Cosmos RPC health monitoring
- **Graceful reload** — Zero-downtime restarts
- **Log management** — Per-process rotation, gzip compression, tail, follow, grep filtering
- **TUI dashboard** — Real-time monitoring with `monit` command
- **Web API + Prometheus** — REST API, WebSocket, and `/metrics` endpoint for Grafana
- **Multi-channel notifications** — Telegram, Discord, Slack, and generic HTTP webhooks
- **Self-update** — `oxidepm update` downloads and swaps the latest release
- **JSON output** — `--json` flag for machine-readable output on all commands
- **Double-sign protection** — Pre-flight key check + runtime signing sentinel for validators
- **Git clone & start** — One command to clone, setup, and run

## Installation

### Quick Install (Recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/oxidekit/oxidepm/prod/scripts/install.sh | sh
```

### APT (Debian/Ubuntu)

Download the `.deb` from the [Releases page](https://github.com/oxidekit/oxidepm/releases):

```bash
# x86_64
wget https://github.com/oxidekit/oxidepm/releases/latest/download/oxidepm_0.3.0_amd64.deb
sudo dpkg -i oxidepm_0.3.0_amd64.deb

# ARM64
wget https://github.com/oxidekit/oxidepm/releases/latest/download/oxidepm_0.3.0_arm64.deb
sudo dpkg -i oxidepm_0.3.0_arm64.deb
```

### Homebrew (macOS/Linux)

```bash
brew tap oxidekit/homebrew-tap
brew install oxidepm
```

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
```

### Download Binaries

Pre-built binaries available on the [Releases page](https://github.com/oxidekit/oxidepm/releases):

| Platform | Architecture | Download |
|----------|--------------|----------|
| Linux | x86_64 | `oxidepm-x86_64-unknown-linux-gnu.tar.gz` |
| Linux | ARM64 | `oxidepm-aarch64-unknown-linux-gnu.tar.gz` |
| macOS | Intel | `oxidepm-x86_64-apple-darwin.tar.gz` |
| macOS | Apple Silicon | `oxidepm-aarch64-apple-darwin.tar.gz` |

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

# Start with watch mode and memory limit
oxidepm start ./my-project --watch --max-memory 512

# Initialize config by scanning a project
oxidepm init ./my-project

# Clone and start from GitHub
oxidepm start --git https://github.com/user/repo

# View status
oxidepm status

# Real-time resource monitor
oxidepm top

# View logs (with follow)
oxidepm logs my-app -f

# Deploy to production
oxidepm deploy user@server --cwd /srv/myapp --post "npm run build"

# Stop a process
oxidepm stop my-app
```

## Commands

| Command | Description |
|---------|-------------|
| `start <target>` | Start a process or config file |
| `start --git <url>` | Clone repo, setup, and start |
| `stop <selector>` | Stop process(es) |
| `restart <selector>` | Hard restart process(es) |
| `reload <selector>` | Graceful zero-downtime restart |
| `delete <selector>` | Remove from registry |
| `status` | Show status table |
| `logs <name> [-f]` | View/follow logs |
| `show <name>` | Detailed process info |
| `monit` | TUI dashboard |
| `save` | Save current process list |
| `resurrect` | Restore saved processes |
| `startup [systemd\|launchd]` | Generate autostart script |
| `check <target> [--fix]` | Validate project readiness |
| `flush <selector>` | Clear log files |
| `describe <target>` | Show command without starting |
| `web [--port 9615]` | Start Web API server |
| `notify telegram` | Configure Telegram alerts |
| `ping` | Check daemon health |
| `cosmos-status <name>` | Cosmos node sync/lifecycle info |
| `update [--version X]` | Self-update to latest (or specific) release |
| `init [dir]` | Scan project and generate config file |
| `deploy <host>` | SSH deploy: pull code + restart processes |
| `diff <host>` | Show remote git changes since last deploy |
| `env list/set/unset` | Manage process environment variables |
| `top` | Real-time process resource monitor |
| `backup [--output file]` | Export config + process list to tarball |
| `restore <file>` | Restore from backup tarball |
| `completions <shell>` | Generate shell completions (bash/zsh/fish) |
| `man` | Generate man page |
| `kill` | Stop daemon and all processes |

**Global flags:** `--json` for machine-readable output, `-v` for verbose logging.

**Selectors:** Process name, ID, `all`, or `@tag` for groups.

## Start Options

### Git Clone

```bash
# Clone and start (auto-setup)
oxidepm start --git https://github.com/user/repo

# Specify branch
oxidepm start --git https://github.com/user/repo --branch develop

# Custom clone directory
oxidepm start --git https://github.com/user/repo --clone-dir ./projects/repo
```

### Process Configuration

```bash
oxidepm start ./app \
  --name my-app \
  --watch \
  --env NODE_ENV=production \
  --env-file .env.production \
  --env-inherit \
  --tag api \
  --delay 5000 \
  --max-uptime 24h \
  --max-restarts 10 \
  --restart-delay 1000
```

### Clustering

```bash
# Run 4 instances with automatic port assignment
oxidepm start ./server -i 4 --port 3000
```

### Health Checks

```bash
# HTTP health check
oxidepm start ./server --health-check http://localhost:3000/health

# Script-based health check
oxidepm start ./server --health-check ./check-health.sh
```

### Event Hooks

```bash
oxidepm start ./app \
  --on-start "./notify.sh started" \
  --on-stop "./notify.sh stopped" \
  --on-crash "./notify.sh crashed" \
  --on-restart "./notify.sh restarted"
```

## Configuration File

Create `oxidepm.config.toml` or `ecosystem.config.toml`:

```toml
[[apps]]
name = "api"
mode = "cargo"
cwd = "./api"
watch = true
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
depends_on = ["api"]              # Wait for API to be running first
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

Also supports YAML and JSON formats.

## Project Init

Scan a directory and auto-generate a config file:

```bash
oxidepm init                  # Scan current directory
oxidepm init ./my-project     # Scan specific directory
```

Detects Node.js (npm/pnpm/yarn), Cargo, Python, and Go projects. Generates `oxidepm.config.toml` with sensible defaults.

## Deploy

Push code to a remote server and restart processes:

```bash
# Basic: pull latest code and restart all processes
oxidepm deploy user@myserver --cwd /srv/myapp

# With build step and specific process
oxidepm deploy user@myserver --cwd /srv/myapp --post "npm run build" --restart api

# Specify branch
oxidepm deploy user@myserver --cwd /srv/myapp --branch main

# Dry run (show commands without executing)
oxidepm deploy user@myserver --cwd /srv/myapp --dry-run
```

Requires SSH key authentication (no password prompts).

## Environment Management

```bash
# List environment variables for a process
oxidepm env list my-app

# Set a variable (requires restart to take effect)
oxidepm env set my-app NODE_ENV=production

# Remove a variable
oxidepm env unset my-app DEBUG
```

Sensitive values (containing SECRET, TOKEN, KEY, PASSWORD) are automatically masked in output.

## Cosmos Node Management

OxidePM provides first-class support for Cosmos SDK blockchain nodes with `mode = "cosmos"`.

> **Security:** Always run blockchain nodes as a dedicated non-root user (e.g. `monouser`). Use `sudo` only for one-off privileged operations like system-level configuration. Never run validators as `root`.

### Cosmos Config

```toml
[[apps]]
name = "monod"
mode = "cosmos"
binary = "/usr/local/bin/monod"
args = ["start", "--home", "/home/monouser/.mono"]
cwd = "/home/monouser"

# Node configuration
cosmos_mode = "validator"           # validator | seed | relay
rpc_endpoint = "http://localhost:26657"
chain_id = "mono_6940-1"

# Relay-until-synced: start as relay, auto-enable validator key when synced
relay_until_synced = true
validator_key_path = "/home/monouser/.mono/config/priv_validator_key.json"

# System limits
nofile_limit = 65535
shutdown_timeout = 30

# Upgrade halt detection (detects x/upgrade planned halts)
detect_upgrade_halt = true

# Restart policy
restart = "on-crash"                # on-crash | always | never
max_restarts = 5
restart_delay = 10000
```

### Relay-Until-Synced

When `relay_until_synced = true`, OxidePM manages a state machine that prevents validator jailing during initial sync:

1. **RelayingSyncing** — Validator key is disabled (renamed), node syncs as a relay
2. **TransitioningToValidator** — Node is synced, OxidePM stops the process, restores the key, restarts
3. **ValidatorActive** — Node is signing blocks normally
4. **ValidatorCatchingUp** — Validator fell behind (alert only, key stays enabled)

The validator key is only ever renamed (`*.json` ↔ `*.json.disabled`) — OxidePM never reads its contents.

### Upgrade Halt Detection

When a Cosmos chain halts for a governance-approved upgrade, the node exits with a message like:

```
UPGRADE "v2.0.0" NEEDED at height: 500000
```

OxidePM detects this pattern and:
- Sets the process status to `upgrade_halted` (not `errored`)
- Suppresses auto-restart (even with `restart = "always"`)
- Sends a Telegram notification with the required action
- Shows the upgrade info in `oxidepm status` and the TUI

The operator then swaps the binary and manually restarts.

### Double-Sign Protection

OxidePM provides two layers of protection against accidental double signing:

**Pre-flight key check:** When `node_mode` is `relay`, `seed`, `sentry`, or `archive`, OxidePM refuses to start if `priv_validator_key.json` is present. This catches the dangerous scenario where someone creates a server snapshot and launches relay nodes without removing the validator key — CometBFT would silently sign blocks with it. The error message suggests moving the key to a backup location.

**Runtime sentinel:** For validators, OxidePM compares the local `priv_validator_state.json` height against the chain's block height. If the chain is advancing past what the local node signed and the node is not catching up, a **Critical** `double_sign_risk` alert fires. This detects when another instance (e.g., from a forgotten snapshot) is signing with the same key.

> The sentinel is safe during migrations: `catching_up: true` skips the check, and fresh nodes with `height: 0` are exempt.

### Cosmos Status

```bash
# Check cosmos-specific node info
oxidepm cosmos-status monod
```

Shows chain ID, node mode, lifecycle state, block height, catching-up status, and upgrade halt details.

### Cosmos Health Checks

OxidePM automatically monitors Cosmos nodes by polling:
- **CometBFT `/status`** — Sync state, block height, stale detection
- **Slashing module** — Missed blocks and jailed status (validators only)

### Cosmos Notifications

New event types for validators:
- `validator_activated` — Relay-until-synced complete (info)
- `catching_up` — Node falling behind (warning)
- `missed_blocks` — Approaching jail threshold (warning)
- `jailed` — Validator jailed (critical)
- `stale_node` — No new blocks (critical)
- `upgrade_halted` — Planned upgrade halt (warning)
- `double_sign_risk` — Another instance may be signing with the same key (critical)

Filter by severity:

```toml
# In ~/.oxidepm/notify.toml
min_severity = "warning"  # info | warning | critical
```

Use environment variables for bot tokens (recommended for validators):

```toml
[telegram]
bot_token_env = "OXIDEPM_TELEGRAM_TOKEN"  # reads from env var
chat_id = "-100123456789"
```

## Preflight Checks

OxidePM validates your project before starting:

```bash
# Check project readiness
oxidepm check ./my-app

# Auto-fix issues (install deps, create .env)
oxidepm check ./my-app --fix

# Start with auto-setup
oxidepm start ./my-app --setup
```

**Checks performed:**
- Node.js: `node_modules/` exists, lockfile present
- Cargo: `Cargo.lock` exists
- All: `.env` file (copies from `.env.example` with `--fix`)

## Notifications

OxidePM supports multiple notification channels. Configure in `~/.oxidepm/notify.toml`:

```toml
# Events to notify on (empty = all events)
events = ["crash", "restart", "jailed", "double_sign_risk", "upgrade_halted"]
min_severity = "warning"  # info | warning | critical

# Telegram
[telegram]
bot_token_env = "OXIDEPM_TELEGRAM_TOKEN"  # Preferred: reads from env var
chat_id = "-100123456789"

# Discord (webhook URL from channel settings → Integrations → Webhooks)
[discord]
webhook_url = "https://discord.com/api/webhooks/123456/abcdef..."

# Slack (incoming webhook URL)
[slack]
webhook_url = "https://hooks.slack.com/services/T00/B00/xxx..."

# Generic HTTP webhook (POST JSON to any URL)
[webhook]
url = "https://your-server.com/alerts"
secret = "your-hmac-secret"  # Sent as X-Webhook-Secret header
```

CLI configuration:

```bash
# Configure Telegram bot
oxidepm notify telegram --token YOUR_BOT_TOKEN --chat YOUR_CHAT_ID

# Set which events to notify
oxidepm notify events --set crash,restart,jailed,double_sign_risk

# Test notifications
oxidepm notify test

# View config
oxidepm notify status
```

## Web API & Prometheus

Start the API server:

```bash
oxidepm web --port 9615 --api-key your-secret-key
```

### Endpoints

| Endpoint | Method | Auth | Description |
|----------|--------|------|-------------|
| `/api/health` | GET | No | Health check + version |
| `/metrics` | GET | No | Prometheus metrics (text format) |
| `/api/processes` | GET | Yes | All processes status |
| `/api/processes/:selector` | GET | Yes | Single process details |
| `/api/processes/:selector/stop` | POST | Yes | Stop process |
| `/api/processes/:selector/restart` | POST | Yes | Restart process |
| `/api/processes/:selector/logs` | GET | Yes | Process logs |
| `/api/save` | POST | Yes | Save process list |
| `/ws` | WebSocket | Yes | Real-time updates |

Authentication via `X-API-Key` header when `--api-key` is set.

### Prometheus Metrics

The `/metrics` endpoint exports in Prometheus text exposition format:

```
oxidepm_processes_total 3
oxidepm_process_up{name="monod",id="0",mode="cosmos"} 1
oxidepm_process_uptime_seconds{name="monod",id="0",mode="cosmos"} 86400
oxidepm_process_restarts_total{name="monod",id="0",mode="cosmos"} 0
oxidepm_process_memory_bytes{name="monod",id="0",mode="cosmos"} 524288000
oxidepm_process_cpu_percent{name="monod",id="0",mode="cosmos"} 2.30
```

Add to your `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: 'oxidepm'
    static_configs:
      - targets: ['localhost:9615']
```

## VS Code Extension

Manage OxidePM processes directly from VS Code with the [OxidePM extension](https://github.com/oxidekit/oxidepm-vscode):

- Sidebar process explorer with status, port, CPU, memory, uptime
- Start / Stop / Restart via context menu and command palette
- Real-time log viewer via WebSocket streaming
- Status bar showing online/total process count
- Crash and restart notifications with action buttons
- Cosmos mode extras (chain ID, node mode, sync status)

**Install:**
```bash
# Download .vsix from releases and install
code --install-extension oxidepm-0.1.1.vsix

# Or clone and build from source
git clone https://github.com/oxidekit/oxidepm-vscode
cd oxidepm-vscode && npm install && npm run build
npx vsce package && code --install-extension oxidepm-*.vsix
```

Requires `oxidepm web --port 9615` to be running.

## TUI Dashboard

```bash
oxidepm monit
```

Real-time monitoring dashboard with:
- Process list with CPU/memory graphs
- Log viewer
- Start/stop/restart controls
- Keyboard navigation

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

Check what's changed on a remote server before deploying:

```bash
oxidepm diff user@myserver --cwd /srv/myapp --count 10
```

## Docker

```bash
docker build -t oxidepm .
docker run -d --name oxidepm-daemon oxidepm
```

Or use in a multi-service Docker Compose setup where OxidePM manages your processes inside a container.

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
- `oxidepm-ipc` - Unix socket protocol
- `oxidepm-runtime` - Node/Rust/cmd runners
- `oxidepm-watch` - Filesystem watcher
- `oxidepm-logs` - Log rotation + streaming
- `oxidepm-db` - SQLite persistence
- `oxidepm-health` - Health check monitoring (HTTP, script, Cosmos RPC)
- `oxidepm-web` - REST API + WebSocket + Prometheus metrics
- `oxidepm-tui` - Terminal UI (ratatui)
- `oxidepm-notify` - Notifications (Telegram, Discord, Slack, webhooks)

## Data Directory

All data stored in `~/.oxidepm/`:

```
~/.oxidepm/
├── daemon.sock     # IPC socket
├── oxidepm.db      # SQLite database
├── saved.json      # Saved process list
├── notify.toml     # Notification config
├── repos/          # Git cloned repositories
└── logs/           # Process log files
    ├── app-out.log
    └── app-err.log
```

## Comparison with PM2

### Resource Usage

| Metric | OxidePM | PM2 |
|--------|---------|-----|
| Binary size | ~7 MB | ~50 MB (requires Node.js runtime) |
| Daemon memory | ~10 MB | ~50-100 MB |
| Startup time | Instant (<50ms) | 1-2 seconds |
| Per-process overhead | Minimal | Higher (Node.js event loop) |

### Feature Comparison

| Feature | OxidePM | PM2 | Notes |
|---------|:-------:|:---:|-------|
| **Runtime Support** |
| Node.js apps | ✅ | ✅ | Both support Node.js natively |
| npm/yarn/pnpm scripts | ✅ | ✅ | Run package.json scripts |
| Python apps | ✅ | ❌ | Auto-detects virtualenv, unbuffered output |
| Go projects | ✅ | ❌ | `go run` with module detection |
| Rust/Cargo projects | ✅ | ❌ | OxidePM auto-builds and runs Cargo projects |
| Cosmos SDK nodes | ✅ | ❌ | Lifecycle, upgrade halt, double-sign protection |
| Generic commands | ✅ | ✅ | Run any shell command |
| **Process Management** |
| Daemon supervision | ✅ | ✅ | Processes persist across terminal sessions |
| Auto-restart on crash | ✅ | ✅ | Configurable restart policies |
| Crash-loop protection | ✅ | ✅ | Exponential backoff on repeated crashes |
| Graceful reload | ✅ | ✅ | Zero-downtime restarts |
| Clustering | ✅ | ✅ | Run multiple instances |
| **Developer Experience** |
| Watch mode | ✅ | ✅ | Auto-restart on file changes |
| Port conflict detection | ✅ | ❌ | Suggests alternative port when conflict detected |
| Preflight checks | ✅ | ❌ | Validates deps before starting |
| Git clone & start | ✅ | ❌ | One command to clone, setup, and run |
| Project init | ✅ | ❌ | `oxidepm init` scans and generates config |
| SSH deploy | ✅ | ✅ | `oxidepm deploy user@host` |
| Real-time top | ✅ | ❌ | `oxidepm top` — htop for processes |
| Env management | ✅ | ✅ | `oxidepm env list/set/unset` |
| Auto-setup (`--setup`) | ✅ | ❌ | Installs deps, creates .env from template |
| Event hooks | ✅ | ✅ | Run scripts on start/stop/crash |
| **Monitoring** |
| Status table | ✅ | ✅ | CPU, memory, uptime display |
| TUI dashboard | ✅ | ✅ | Real-time terminal UI |
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
| Double-sign protection | ✅ | ❌ | Pre-flight + runtime sentinel |
| Systemd/launchd | ✅ | ✅ | Auto-start on boot |
| **Configuration** |
| TOML config | ✅ | ❌ | Clean, readable config format |
| YAML config | ✅ | ✅ | Ecosystem file support |
| JSON config | ✅ | ✅ | Ecosystem file support |
| Save/Resurrect | ✅ | ✅ | Save and restore process list |

### Why Choose OxidePM?

**Choose OxidePM if you:**
- Work with Rust projects (native Cargo support)
- Want health checks and web API without a subscription
- Prefer lower resource usage (Rust vs Node.js daemon)
- Need port conflict detection and preflight checks
- Want to clone and run projects from git in one command

**Choose PM2 if you:**
- Need the PM2 ecosystem (pm2.io, container support)
- Require Windows support
- Have existing PM2 configurations you don't want to migrate
- Need PM2-specific features like the APM dashboard

## Requirements

- Linux or macOS (Windows not supported yet)
- Rust 1.75+ (only for building from source)
- Runtimes only needed for their respective app types:
  - Node.js for Node/npm/pnpm/yarn apps
  - Python 3 for Python apps
  - Go 1.21+ for Go apps

## License

MIT

## Contributing

Contributions welcome! Please open an issue or PR.
