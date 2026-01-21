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

### From Source

```bash
git clone https://github.com/your-org/oxidepm
cd oxidepm
cargo build --workspace --release

# Install binaries
cargo install --path crates/oxidepm
cargo install --path crates/oxidepmd
```

### Binaries

Pre-built binaries for Linux and macOS coming soon.

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

| Feature | OxidePM | PM2 |
|---------|---------|-----|
| Language | Rust | Node.js |
| Binary size | ~6.6 MB | ~50 MB (with Node) |
| Memory usage | ~10 MB | ~50-100 MB |
| Startup time | Instant | ~1-2s |
| Node.js support | ✓ | ✓ |
| Rust/Cargo support | ✓ | ✗ |
| Watch mode | ✓ | ✓ |
| Clustering | ✓ | ✓ |
| Health checks | ✓ | ✓ (Plus) |
| Web API | ✓ | ✓ (Plus) |
| TUI dashboard | ✓ | ✓ |
| Git clone start | ✓ | ✗ |
| Preflight checks | ✓ | ✗ |
| Telegram alerts | ✓ | ✗ |

## Requirements

- Linux or macOS (Windows not supported yet)
- Rust 1.75+ (for building)
- Node.js (for Node.js apps)

## License

MIT

## Contributing

Contributions welcome! Please open an issue or PR.
