# OxidePM

A fast, modern process manager for Node.js and Rust applications. Built in Rust for reliability and performance.

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

- **Multi-runtime support** - Node.js, npm/pnpm/yarn scripts, Cargo projects, Rust single-file
- **Daemon supervision** - Processes persist across terminal sessions
- **Auto-restart** - Configurable restart policies with crash-loop protection
- **Watch mode** - Automatic rebuild and restart on file changes
- **Clustering** - Run multiple instances with automatic port assignment
- **Health checks** - HTTP and script-based health monitoring
- **Graceful reload** - Zero-downtime restarts
- **Log management** - Rotation, tail, follow, grep filtering
- **TUI dashboard** - Real-time monitoring with `monit` command
- **Web API** - REST API + WebSocket for remote management
- **Telegram alerts** - Notifications for crashes, restarts, memory limits
- **Git clone & start** - One command to clone, setup, and run

## Installation

### Quick Install (Recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/oxidekit/oxidepm/main/scripts/install.sh | sh
```

### Homebrew (macOS/Linux)

```bash
brew tap oxidekit/homebrew-tap
brew install oxidepm
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

# Start from a directory (auto-detects project type)
oxidepm start ./my-project

# Start with watch mode
oxidepm start ./my-project --watch

# Clone and start from GitHub
oxidepm start --git https://github.com/user/repo

# View status
oxidepm status

# View logs
oxidepm logs my-app -f

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
| `kill` | Stop daemon and all processes |

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
```

Start all apps:

```bash
oxidepm start oxidepm.config.toml
```

Also supports YAML and JSON formats.

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

## Telegram Notifications

```bash
# Configure Telegram bot
oxidepm notify telegram --token YOUR_BOT_TOKEN --chat YOUR_CHAT_ID

# Set which events to notify
oxidepm notify events --set start,stop,crash,restart,memory_limit

# Test notifications
oxidepm notify test

# View config
oxidepm notify status
```

## Web API

Start the API server:

```bash
oxidepm web --port 9615 --api-key your-secret-key
```

### Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/api/status` | GET | All processes status |
| `/api/process/:id` | GET | Single process details |
| `/api/process/:id/start` | POST | Start process |
| `/api/process/:id/stop` | POST | Stop process |
| `/api/process/:id/restart` | POST | Restart process |
| `/api/logs/:id` | GET | Process logs |
| `/ws` | WebSocket | Real-time updates |

Authentication via `X-API-Key` header when `--api-key` is set.

## TUI Dashboard

```bash
oxidepm monit
```

Real-time monitoring dashboard with:
- Process list with CPU/memory graphs
- Log viewer
- Start/stop/restart controls
- Keyboard navigation

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
- `oxidepm-health` - Health check monitoring
- `oxidepm-web` - REST API + WebSocket
- `oxidepm-tui` - Terminal UI (ratatui)
- `oxidepm-notify` - Telegram notifications

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
| Rust/Cargo projects | ✅ | ❌ | OxidePM auto-builds and runs Cargo projects |
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
| Auto-setup (`--setup`) | ✅ | ❌ | Installs deps, creates .env from template |
| Event hooks | ✅ | ✅ | Run scripts on start/stop/crash |
| **Monitoring** |
| Status table | ✅ | ✅ | CPU, memory, uptime display |
| TUI dashboard | ✅ | ✅ | Real-time terminal UI |
| Log management | ✅ | ✅ | Rotation, tail, follow, grep |
| Health checks (HTTP) | ✅ | 💰 | PM2 requires Plus subscription |
| Health checks (Script) | ✅ | 💰 | PM2 requires Plus subscription |
| **Integrations** |
| Web API / REST | ✅ | 💰 | PM2 requires Plus subscription |
| WebSocket real-time | ✅ | 💰 | PM2 requires Plus subscription |
| Telegram alerts | ✅ | ❌ | Native Telegram bot integration |
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
- Rust 1.75+ (for building)
- Node.js (for Node.js apps)

## License

MIT

## Contributing

Contributions welcome! Please open an issue or PR.
