# AI-Assisted Process Management — OxidePM Safety Runbook

This document covers the safe use of AI assistants when managing processes through OxidePM.

---

## Why This Document Exists

OxidePM supervises long-running processes: web services, workers, databases, build watchers. Stopping, restarting, or deleting the wrong process, or truncating its logs, can cause downtime or destroy the evidence needed to debug an incident. AI assistants can compose and execute OxidePM commands quickly, but they do not always understand the side effects of those commands.

**Rule of thumb: let the AI read freely (status, logs, show), and require a human to confirm anything that stops, restarts, deletes, or truncates.**

---

## Command Risk Classification

### Safe (Read-Only)

```
oxidepm status [--more]         # Process list with CPU/memory (--more also shows the policy markers)
oxidepm show <name>             # Single process details
oxidepm logs <name>             # Process log output
oxidepm env list <name>         # Environment keys of an app (values are redacted)
oxidepm describe <name>         # Show the command that would run, without starting it
oxidepm ping                    # Daemon health check
oxidepm monit | top             # Interactive monitors (monit is the ui-tui adapter)
oxidepm --version               # Version info
```

### Caution (Write Operations)

| Command | Risk | Notes |
|---------|------|-------|
| `oxidepm start <target>` | Medium | Starts a process, or every app of a config file; verify the config is correct first. An app that is already up is refused (`E_APP_EXISTS`) and not changed |
| `oxidepm stop <selector>` | Medium | Stops the process; it stays stopped and is not auto-restarted. With several apps (`all`, a tag) dependents are stopped before their dependencies |
| `oxidepm restart <selector>` | Medium | Brief downtime while the process restarts |
| `oxidepm reload <selector>` | Medium | The same as `restart`: the process is stopped and started again, there is no zero-downtime reload |
| `oxidepm save` | Low | Writes the apps that should be running to `saved.json`; refuses to replace a non-empty file with an empty list unless `--force` |
| `oxidepm startup` | Low | Generates an autostart script (systemd/launchd) |
| `oxidepm resurrect` | Medium | Starts every app the daemon's registry says should be running and is not up (apps you stopped stay stopped), and re-adds apps from `saved.json` that the registry no longer has — verify the saved state first |
| `oxidepm check [target]` | Medium | The check adapter declares a caution-tier verb. `--fix` may install Node dependencies and create `.env`. `--set-env` edits `.env` |
| `oxidepm init [dir]` | Caution | The init adapter writes a new project config. Preview with `--dry-run` |
| `oxidepm git-start <url>` | Caution | The git-start adapter prepares a checkout. Starting the app and explicit dependency installs can run project code |
| `oxidepm deploy <host>` and `oxidepm diff <host>` | Caution | Both verbs come from the deploy adapter. Deploy pulls code and restarts remote processes. Diff inspects the remote checkout |
| `oxidepm update` | Medium | Replaces the installed oxidepm binaries and stops a running daemon |

Selectors can be a name, an id, `@tag`, or `all`. Treat `all` as a higher-risk variant of every command that accepts it.

### Dangerous

| Command | Why |
|---------|-----|
| `oxidepm kill` | Stops the daemon and every managed process |
| `oxidepm delete <selector>` | Stops the process and removes it from management (and from the database) |
| `oxidepm flush <selector>` | Truncates the active log files (stdout and stderr) of the process — the log contents are lost; rotated files are not touched |
| `oxidepm restore <file>` | Restores config and saved state from a backup tarball, overwriting current settings (the daemon must not be running) |
| `oxidepm policy set <selector> --protected=<bool> --ai-control=<bool>` | Changes who may stop, restart or reload an app. People only: an AI cannot protect, unprotect or open an app |
| `oxidepm show <name> --reveal` | Prints environment values and hook scripts that are otherwise `<redacted>` or `<set>`; the daemon logs each reveal. Do not let an AI run it, its output ends up in the conversation |

---

## Process States

| State | Meaning | AI action |
|-------|---------|-----------|
| `online` | Running normally | Monitor |
| `stopped` | Stopped manually with `oxidepm stop` | Safe to start if appropriate |
| `errored` | Exited unexpectedly (any exit that was not a manual stop) and is not being restarted, for example because the crash loop limit was reached, the restart policy says not to, a launch failed, or a dependency failed or timed out | Read the logs for the root cause before restarting |
| `starting` | Spawned; with a health check, until its first probe passes | Wait |
| `unhealthy` | Alive, but the health check failed `retries` times in a row; it is left running unless the check says `action = "restart"` | Read the logs and the check before restarting |
| `backoff` | Crashed; an automatic restart is scheduled (`status --json` has the time of the restart and the attempt) | Wait, or read the logs; a `stop` cancels the restart |
| `stopping` | Shutting down | Wait |
| `building` | Building from source | Wait |
| `waiting` | Accepted, held back by `depends_on` (or a startup delay); it fails closed (`errored`, "dependency ... not running after Ns", 60 s unless `depends_timeout` says otherwise) if the dependency does not come up | Look at the dependency with `status`; do not restart the dependent |

A cluster (`instances > 1`) is a parent app plus one app per instance (`web`, `web-0`, `web-1`). `delete web` removes all of them; `start web` after `stop web-1` starts only `web-1`; the instance count cannot change, and a single app cannot be started as a cluster or the reverse (delete and recreate; the start is refused with `E_INVALID_SPEC`). A new cluster that fails to launch an instance is deleted again, so a failed `start` leaves nothing to clean up.

---

## Config Safety

Config files (`oxidepm.config.toml`, `ecosystem.config.toml`, or the YAML/JSON equivalents) define what gets run, with which environment and restart policy. AI assistants often get field names and modes wrong.

### Example

```toml
[[apps]]
name = "worker"
mode = "node"
script = "worker.js"
cwd = "/srv/worker"
max_restarts = 5
restart_delay = 2000
restart = "on-crash"             # on-crash | always | never
```

### Common AI mistakes

| Mistake | What happens |
|---------|-------------|
| Wrong `mode` | The process is run with the wrong runner. Without `mode`, a `script` gets its mode from its extension and the file is refused if that is not possible; an app with neither `script` nor `bin` runs a command named like the app |
| Guessing field names | The file is refused and the unknown key is named, so nothing starts. Note that `restart` is the key for the restart mode (`restart_mode` is not a key) |
| Setting `restart = "always"` for a process that exits normally | The process is restarted over and over, with a growing delay, until more than `max_restarts` exits fall inside `crash_window` seconds; a run of `stable_uptime` seconds starts the delay over |
| Setting `max_restarts` very high | A crash loop can run for a long time (the default is 15 inside 600 seconds) |
| Editing the config while the process runs | The running process keeps the spec it was started with. Only `oxidepm start <file>` reads the file, and it refuses an app that is up; `restart` and `reload` reuse the old spec. Stop the app, then start the file again |
| `env set` / `env unset` | Both are refused; they never changed anything. Put `env` in the config file or pass `--env` at start |
| Legacy watch settings | Opt in per app with `[apps.adapters.watch]` and the enabled watch adapter |

Review the config diff, then use `oxidepm --dry-run start <config>` to validate it and preview the start. Use `oxidepm describe <target>` for a single target or `oxidepm show <name>` to inspect a registered app.

---

## Critical Safety Rules

### 1. Review AI-written configs before starting them

Read the generated config yourself. Confirm `mode`, `script`/`bin`, `cwd`, `env`, ports, and the restart policy match what you intend.

### 2. Confirm before destructive commands

Never let an AI run `kill`, `delete`, `flush`, or any command with the selector `all` without explicit human confirmation. `flush` permanently discards log output; `delete` drops the process definition.

### 3. Read logs before restarting

When a process is `errored` or restarting repeatedly, look at `oxidepm logs <name>` first. Restarting or flushing logs first removes the evidence of why it failed.

### 4. Let OxidePM handle restarts

OxidePM's `restart` policy:
- `on-crash` (default) restarts on a non-zero exit code or a death by a signal; an exit with code 0 is not restarted and leaves the app `errored`
- `always` restarts regardless of exit code
- `never` does not restart
- A manual `oxidepm stop` is never auto-restarted
- Restarts back off (the delay doubles up to `max_restart_delay`); more than `max_restarts` restarts inside `crash_window` seconds end the app `errored` as a crash loop, and it stays `errored` until a person starts it

AI assistants should not implement their own restart logic with `while true` loops, extra watchers, or cron jobs.

### 5. Keep secrets out of prompts and configs

`status`, `show`, `describe` and `oxidepm env list` do not show environment values: the daemon replaces every value with `<redacted>` (keys stay visible) and hook or health-check scripts with `<set>`. Only `oxidepm show <name> --reveal` prints them. Command arguments, URLs, logs, other command output and config files are not scrubbed. Do not paste tokens into AI conversations; use `env_file` or environment variables instead.

---

## Who Is Asking: Actors and the Agent Socket

The daemon decides who is asking from the connection a request arrives on. What the client writes about itself is only a claim, and a claim can make a request stricter, never looser.

| Where the request comes in | Class | Label |
|---|---|---|
| `daemon.sock`, no claim | human | `cli` |
| `daemon.sock`, claim `human:<label>` | human | the label |
| `daemon.sock`, claim `ai:<label>` | ai | the label |
| `daemon.sock`, any other claim (including `adapter:*` and `system:*`, which a client may never claim, and a label that is not `[A-Za-z0-9._-]` up to 32 characters) | ai | `invalid-claim` |
| `agent.sock`, whatever the claim | ai | the label of an `ai:<label>` claim, else `agent` |

The CLI sends `OXIDEPM_ACTOR` as its claim (`OXIDEPM_ACTOR=ai:example-agent oxidepm status`). Set it for every agent you run. `OXIDEPM_SOCKET` names the socket the CLI talks to; pointing an agent at `agent.sock` (`OXIDEPM_SOCKET=~/.oxidepm/agent.sock`) gives it class ai whatever it claims, so it does not depend on the agent being honest about itself. The daemon writes every request to its log with the actor, the class and the socket (`request <id> <type> actor=ai:example-agent class=ai via=agent pid=...`; reads at debug level), and the lifecycle events of an app are credited to the actor of the request that caused them, or to `system:crash`, `system:limit`, `system:cron`, `system:health` and `system:resurrect` when the daemon acted on its own.

`agent.sock` is created in the data directory with mode 0600; `agent_socket = false` in `daemon.toml` turns it off. It is tighter than the main socket: lines of at most 1 MiB, at most 32 connections at once (the 33rd is closed), a connection that is idle for 60 seconds is closed, and a write the client does not read is abandoned after 10 seconds. If it cannot be bound (a path that is too long, a file in the way) the daemon starts without it and says so in its log.

What this gives and does not give:

- It binds an AI that goes through the agent socket or that declares itself. It makes a confused or over-eager agent safe.
- It does not stop a hostile or careless process that runs as the same user and calls `oxidepm` without declaring anything: that process reaches the main socket and is a human as far as the kernel can tell. Code that runs as you is you.
- A class decides what a request may do (next section). A refused request is answered `E_POLICY_DENIED` (or `E_PROTECTED`), the message names the actor, the request and the apps, and nothing changes.

To contain an agent that runs in a container, run it as the same user inside the container (for example rootless Podman with `--userns=keep-id`) and bind-mount only `agent.sock` into it, then set `OXIDEPM_SOCKET` to the mounted path. The container then cannot reach `daemon.sock`.

---

## What Each Class May Ask For

Every request has a tier. A person (`human`, any label) may make every request. An `ai` actor, whether it declared itself or came in on `agent.sock`, is held to the tier:

| Tier | Requests | `ai` actor |
|---|---|---|
| safe | `status`, `show`, `describe`, `logs`, `ping`, `hello` | yes, on every app, protected ones too |
| caution | `stop`, `restart`, `reload` | only on an app marked `ai_control` and not `protected` |
| dangerous | `start` (and a config file: a spec is arbitrary code), `delete`, `flush`, `kill`, `save`, `resurrect`, `show --reveal`, `describe --reveal` | never |

Two markers belong to an app and are off by default: `ai_control` (`oxidepm start --ai-control`, or `ai_control = true` in the config file) lets AI actors stop, restart and reload it; `protected` (`--protected`, `protected = true`) makes it read-only for every non-human actor, whatever `ai_control` says. An `ai` actor must name its apps (a name or an id, at most 10): `all` and `@tag` selectors are refused for stop, restart and reload. The check is all or nothing: if one app of the request (an instance of a cluster counts) is not open to the actor, nothing runs and the error names the apps that are not. Whichever apps passed the check are the ones acted on; an app that gains a tag or is created while the request is being checked is not touched.

An `ai` actor can restart or reload the same app once every 10 seconds and five times in ten minutes (`E_RATE_LIMITED`), so a looping agent cannot keep an app down. `restart` also starts an app that is at rest, which is how an AI brings back what it may control.

Verbs that adapters provide follow the same rule. An `ai` actor may run a **safe** verb of an adapter that holds no more than the `read` level, in the working directory of an app it may control. A verb that needs the terminal (a session verb, such as an MCP server over stdio) is opened differently: a person may open any of them, an `ai:*` claim only a verb of an adapter whose manifest says `ai_driven`, and only while that adapter holds no more than `read`; the session such a claim opens may write only to the adapter's own state directory. Set `OXIDEPM_ACTOR=ai:<label>` for the program that launches the verb, as for any other command.

A person changes the markers of an app that is running, without a restart, with `oxidepm policy set <selector> --protected[=false] --ai-control[=false]` (`oxidepm status --more` shows them). For a cluster the cluster and every instance change in one transaction, and an instance created later takes the markers of its cluster; an instance can be set on its own, but it counts as protected when its cluster is, and as open to AI control only when both are. The markers are kept in the database (`state.db`, two columns) and in `saved.json`, so they survive a restart of the daemon, and `delete all` followed by `resurrect` brings an app back with the markers it had. Rolling the binary back to a release before the markers means moving `state.db` aside or putting back `state.db.pre-v2`, the copy the daemon made before it migrated the file (changes made since are lost).

The markers limit requests, not the daemon's own work: a crash restart, a `restart_cron` restart, a limit restart and the boot resurrect treat a protected app like any other. A person (any `human` label) is never limited by the markers. A start of an app that exists keeps its stored markers, whatever the spec or config file says, so running `oxidepm start <file>` again cannot clear `protected`.

---

## The Audit Log

Everything an AI or an adapter asks for is recorded, and so is every caution or dangerous request by anyone. The log is two JSON-lines files in `~/.oxidepm/audit/` (directory `0700`, files `0600`, created when the first line is written):

- `audit.jsonl`: every recorded request.
- `audit-security.jsonl`: the denials, the dangerous requests, and what a non-human actor changed. A chatty but permitted actor cannot rotate this file's evidence away.

Each request leaves a `decision` line (who asked, the operation and its tier, the selector and the apps it reached, `allow` or `deny` with the reason) and, if it ran, a `result` line (`ok`, the error code, how many apps changed or failed, the milliseconds). The two share a `req` number the daemon assigns; the id the client chose is kept only as `client_req`. A start also records the program, the number of arguments and a digest of them. `seq` counts up across rotation, so a gap shows a lost line.

What is recorded: every request of a non-human actor (repeated `ping`, `hello` and `status` are one line per actor and operation per minute with a count; `show`, `describe` and `logs` are one line each, with the app, because they are how arguments and logs are read), a person's caution and dangerous requests, and every denial, whoever asked.

What is never written: environment values, hook and probe scripts, the arguments themselves, log lines, the app spec, adapter configuration. The record is a fixed set of fields with no place for them, and free text is cut to a fixed length.

A non-human actor's caution or dangerous request is written to disk before it runs. If the log cannot be written (the directory is a file, the disk is full, the writer is stuck), the request is refused with `E_AUDIT_UNAVAILABLE` and nothing happens. A read still goes through and the lost line is counted. A person is never held up: a stop works while the log is broken, which is how a full-disk incident is handled; the lines that could not be written are counted. `oxidepm status --daemon` shows `audit: ok` or `degraded`, how many lines were lost, and a warning that says what is wrong.

A non-human actor is also rate limited: 50 requests a second (a burst of 100), and requests that change something, or are refused for being dangerous, 5 a minute (a burst of 10). Over the limit the answer is `E_RATE_LIMITED`; the refusals are counted into one line per actor per minute instead of one line each. An `ai` actor's allowance is shared by every `ai` actor on the same socket, so changing the label in the claim does not buy a new one.

`oxidepm audit` reads the files directly, so it works with the daemon down: `--tail N` (the newest N lines, 50 by default), `--actor ai:example-agent` (or just `ai`, or just `example-agent`), `--op restart`, `--since 30m` (or an RFC 3339 time), `--security` for the second file, and the global `--json` for one JSON object per line. A request that matches keeps its result line. Every string an actor could have written (labels, reasons, notes) is printed with control characters replaced, so a label cannot move the cursor or paint over the lines around it.

The files rotate at 10 MiB (5 MiB for the security file) and keep ten each. There is no tamper evidence beyond the file mode: a process with your rights can rewrite the files, as it can rewrite the database.

---

## Dry Run

Every request that changes something can be asked for as a rehearsal: `oxidepm --dry-run stop web`, or `"dry_run": true` in the envelope. The daemon runs the policy exactly as for the real request, records it in the audit log (the `decision` line and the `result` line both say `"dry_run": true`, because a rehearsal that lists the apps is itself a read), and answers with a plan instead of acting: the policy's decision, and an item for each app with what would happen, the step it happens in (stop and kill go dependents first) and, for a refusal, the error code. A rehearsal the policy refuses answers with the refusal as the plan (`E_POLICY_DENIED`, `E_PROTECTED`, `E_RATE_LIMITED`); a request that is invalid for a reason the daemon can see without side effects (a bad spec, a missing dependency, an unusable saved file) answers with the real error.

A plan is built from the registry's snapshots, the stored specs and the files. It sends nothing to an app, writes no row, `saved.json` or log, and spawns nothing, and a rehearsed restart does not use up the AI restart allowance. A read-only request ignores the flag. An AI actor can use a rehearsal to see what a stop or restart would reach before it asks for it; a rehearsal gives it nothing a `status` does not (a `start`, `delete`, `save` and the other dangerous or global requests are refused for it as plans, like the real ones).

---

## The Web API

The REST API and the WebSocket are the `ui-web` adapter (`oxidepm adapter import ui-web`). It listens on `127.0.0.1:9615` and needs its access key on every request, loopback included; it checks the `Host` and `Origin` headers; and it acts for the apps that list `ui-web` under `adapters` only, at the `lifecycle` level. Its `POST /api/processes` (which starts an arbitrary command) is refused unless the adapter was given the `admin` level, so an AI should not be given that level for it or the key. Everything it does is in the audit log as the actor `adapter:ui-web`. `/api/health` is the one route without a key and says only `{"ok":true}`. The `exporter-prometheus` adapter serves `/metrics` with read access only.

---

## Notification Events

The `notifier` adapter can send notifications (Telegram, Discord, Slack, signed webhooks) for these events of the daemon:

| Event | Severity | AI should know |
|-------|----------|---------------|
| `start` | Info | A process was started |
| `stop` | Info | A process was stopped |
| `crash` | Critical | A process crashed, or ended `errored` (launch failure, crash loop, failed dependency); the message includes the exit code, the signal or the error text |
| `restart` | Warning | A process was restarted (after a crash, by a limit, a cron schedule or by hand) |
| `memory_limit` | Warning | A process exceeded its memory limit and is being restarted |
| `crash_loop` | Critical | A process crashes again and again and is no longer restarted |
| `health_check` | Critical | A health check failed; the message includes the reason the daemon gives |

Notification messages contain process names, ids, error text, and health-check endpoints. Consider this before routing alerts to a shared channel.

---

## AI Failure Patterns with OxidePM

### Manual process management alongside OxidePM
AI uses `pkill`, `kill -9`, or raw process commands instead of `oxidepm stop`. This desynchronizes OxidePM's state from the real process state, leading to unexpected restarts or stale entries.

### Editing config during runtime
AI modifies the config file while the process is running. Changes do not take effect until the process is restarted, and the AI may not realize the running process still uses the old config.

### Flushing logs to "clean up"
AI runs `oxidepm flush` to reduce noise or disk usage. Log rotation is already automatic (configurable with `log_max_size`, `log_max_files`, `log_compress`), and flushing destroys the diagnostic history.

### Restarting to "fix" a crash loop
AI repeatedly restarts an `errored` process without reading the logs. Find the root cause first.

### Operating on `all`
AI uses the `all` selector when it meant a single process, stopping or deleting unrelated services.

---

**Let the AI observe freely; require human confirmation for anything that stops, restarts, deletes, or truncates, and review every AI-written config before it is started.**
