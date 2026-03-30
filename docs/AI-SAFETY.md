# AI-Assisted Process Management — OxidePM Safety Runbook

This document covers the safe use of AI assistants (Claude, ChatGPT, Gemini, Codex, or any LLM) when managing processes through OxidePM, particularly for blockchain validator nodes.

---

## Why This Document Exists

OxidePM manages critical infrastructure processes — including blockchain validators where misconfiguration can result in financial loss (slashing, jailing, double signing). AI assistants can compose and execute OxidePM commands, but they may not understand the safety implications of process management for consensus-participating nodes.

**Rule of thumb: Use Monarch CLI for node management. Use OxidePM directly only for process-level debugging.**

---

## OxidePM vs Monarch CLI

| Use case | Use this | Not this |
|----------|----------|----------|
| Join a network | `monarch join` | Manual config + `oxidepm start` |
| Start/stop nodes | `monarch start/stop` | `oxidepm start/stop` |
| Health checks | `monarch doctor` | Manual RPC queries |
| Config changes | `monarch node configure` | Editing TOML + restart |
| Upgrades | `monarch upgrade` | Binary swap + restart |
| Process debugging | `oxidepm status/logs` | — |

**Monarch CLI wraps OxidePM with validation, safety checks, and correct configuration. Prefer Monarch for all node operations.**

---

## Command Risk Classification

### Safe (Read-Only)

```
oxidepm status [--more]         # Process list with CPU/memory
oxidepm show <name>             # Single process details
oxidepm cosmos-status <name>    # Block height, sync, validator info
oxidepm logs <name>             # Process log output
oxidepm ping                    # Daemon health check
oxidepm --version               # Version info
```

### Caution (Write Operations)

| Command | Risk | Notes |
|---------|------|-------|
| `oxidepm start <config>` | Medium | Starts process — verify config is correct first |
| `oxidepm stop <name>` | Medium | Stops process — validator will miss blocks |
| `oxidepm restart <name>` | Medium | Brief downtime during restart |
| `oxidepm save` | Low | Persists process list for boot recovery |
| `oxidepm startup` | Low | Installs systemd service for boot persistence |
| `oxidepm resurrect` | Medium | Restores saved processes — verify saved state is valid |

### Dangerous

| Command | Why |
|---------|-----|
| `oxidepm kill` | Hard kills the daemon and all managed processes |
| `oxidepm flush` | Removes process from management without stopping |
| `oxidepm delete <name>` | Stops and removes process from management |

---

## Cosmos Mode Safety Features

OxidePM has special handling for Cosmos SDK nodes (`mode = "cosmos"` in config):

### Relay-Until-Synced
- Validator key is disabled (renamed to `.disabled`) until the node is fully synced
- Prevents jailing from signing at the wrong height
- Automatically transitions to validator mode once caught up
- **AI must never bypass this** by manually renaming the key file

### Double-Sign Detection
- Blocks startup when `priv_validator_key.json` exists on a non-validator node
- Prevents accidental block signing on relay, archive, seed, or sentry nodes
- **AI must never work around this** by deleting the key and restarting

### Upgrade Halt Detection
- Detects `UPGRADE ... NEEDED` in stdout and stops the process
- Does NOT auto-restart during an upgrade halt
- **AI should use `monarch upgrade`** to handle binary swaps

### Validator Key Handling
- OxidePM only renames the key file (`.json` ↔ `.json.disabled`)
- It NEVER reads the contents of the key
- It NEVER logs key paths, contents, or IPs in notifications
- **This is by design — do not circumvent it**

---

## Ecosystem Config Safety

The `ecosystem.config.toml` file is critical. AI assistants frequently get this wrong.

### Correct format (Cosmos mode)
```toml
[[apps]]
name = "monod"
mode = "cosmos"
binary = "/usr/local/bin/monod"
args = ["start", "--home", "/home/monouser/.mono"]
cosmos_mode = "validator"        # validator | relay | sentry | seed | archive
rpc_endpoint = "http://127.0.0.1:26657"
chain_id = "mono_6940-1"
relay_until_synced = true        # validator only
restart = "on-crash"
max_restarts = 5
```

### Common AI mistakes

| Mistake | What happens |
|---------|-------------|
| Using `script` instead of `binary` | OxidePM treats it as Node.js, tries npm build |
| Missing `mode = "cosmos"` | No relay-until-synced, no double-sign detection |
| Wrong `cosmos_mode` | Safety checks for wrong role (e.g., validator checks on a relay) |
| Missing `chain_id` | Cosmos status checks fail silently |
| `relay_until_synced = true` on non-validator | Unnecessary key management on nodes without keys |

**Prevention**: Always use `monarch join` which generates the correct config automatically.

---

## Critical Safety Rules

### 1. Never manually edit ecosystem.config.toml with AI

Let `monarch join` or `monarch node configure` generate it. The config format is specific and AI assistants consistently get fields wrong, causing process startup failures or missing safety features.

### 2. Never bypass double-sign protection

If OxidePM blocks startup with "DOUBLE-SIGN RISK: priv_validator_key.json found", do NOT:
- Delete the key and restart
- Change `cosmos_mode` to bypass the check
- Disable the check in config

Instead: move the key to a secure backup location, or switch the node to validator mode if it should be a validator.

### 3. Never manually manage validator keys

OxidePM handles key file state (enabled/disabled) automatically through the relay-until-synced lifecycle. Manual renaming of `.json` ↔ `.disabled` files can cause:
- Double signing if the key is enabled before sync completes
- Jailing if the key is enabled while another validator is running
- State corruption if the key state doesn't match the process manager state

### 4. Let OxidePM handle restarts

OxidePM's `restart = "on-crash"` policy:
- Restarts on non-zero exit (crash)
- Does NOT restart on manual stop (`oxidepm stop`)
- Does NOT restart during upgrade halt
- Respects `max_restarts` limit

AI assistants should not implement their own restart logic with `watch`, `while true`, or cron jobs.

---

## Process States

| State | Meaning | AI action |
|-------|---------|-----------|
| `online` | Running normally | Monitor |
| `stopped` | Manually stopped | Safe to start if appropriate |
| `errored` | Crashed, not restarting | Check logs for root cause |
| `starting` | Starting up | Wait |
| `stopping` | Shutting down | Wait |
| `building` | Building from source | Wait |
| `upgrade_halted` | Chain upgrade detected | Use `monarch upgrade`, do NOT force restart |

---

## Notification Events

OxidePM can send Telegram notifications for these events:

| Event | Severity | AI should know |
|-------|----------|---------------|
| `ValidatorActivated` | Info | Node started signing blocks |
| `CatchingUp` | Warning | Node is syncing, may miss blocks |
| `MissedBlocks` | Warning | Validator missing blocks |
| `Jailed` | Critical | Validator jailed for downtime |
| `StaleNode` | Critical | No new blocks for extended period |
| `LowDiskSpace` | Warning | Disk usage high |

Notification messages intentionally exclude IPs, key paths, and other sensitive data.

---

## AI Failure Patterns with OxidePM

### Using generic process management instead of Cosmos mode
AI starts a Cosmos node without `mode = "cosmos"`, losing relay-until-synced, double-sign detection, and all Cosmos-specific health checks.

### Fighting the double-sign protection
AI encounters the "DOUBLE-SIGN RISK" error and tries to work around it by deleting keys, changing modes, or restarting without OxidePM — defeating the safety mechanism.

### Manual process management alongside OxidePM
AI uses `pkill`, `kill -9`, or raw process commands instead of `oxidepm stop`. This desynchronizes OxidePM's state from actual process state, leading to zombie processes or unexpected restarts.

### Editing config during runtime
AI modifies `ecosystem.config.toml` while the process is running. Changes don't take effect until restart, and the AI may not realize the running process uses the old config.

---

**Use Monarch CLI for node management. Use OxidePM directly only when you need process-level visibility (status, logs). Never let an AI write OxidePM configs manually.**
