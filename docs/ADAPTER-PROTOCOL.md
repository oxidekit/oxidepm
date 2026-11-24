# Adapter protocol (version 1)

How `oxidepmd` talks to an adapter: an executable it starts and supervises. This describes what the daemon does today. The types are in the `oxidepm-adapter-proto` crate; the frames in `crates/oxidepm-adapter-proto/tests/golden/` are the reference examples, and the host tests, the test adapter and the crate's own tests all read those same files. Changing a golden file is a protocol change and needs a new protocol number.

## Transport and framing

- The adapter's stdin carries daemon to adapter, its stdout adapter to daemon. Both are UTF-8, one JSON object per line, ended by `\n`. A frame never contains a raw newline.
- Each object is a JSON-RPC 2.0 message with these restrictions:
  - `"jsonrpc":"2.0"` is required. No batches (an array is a violation).
  - `id` is an unsigned 64-bit integer. Each side numbers its own requests from 1 and never reuses a number.
  - `params` and `result` are objects; a missing `params` means `{}`.
  - `method` is 1 to 64 characters of letters, digits, `.`, `_` and `-`.
  - A response has `result` or `error`, never both. An error is `{code: integer, message: string, data?}`.
  - A line that is empty, contains a NUL byte or is not valid UTF-8 is not a frame.
- Unknown fields are ignored. An unknown request method is answered with error `-32601`; an unknown notification is ignored.
- stdout carries protocol and nothing else. stderr is the adapter's log: lines are cut at 8 KiB, rate limited to 1 MiB per second, kept in the adapter's own rotated file (1 MiB, 5 files) and never mixed into the daemon's log.
- The adapter is started with a cleared environment: a short base (`PATH`, `LANG`, `HOME` set to its state directory), the names its manifest lists in `env_pass`, and `OXIDEPM_ADAPTER_PROTOCOLS`, the comma separated protocol numbers the daemon speaks. A process that finds that variable missing was not started by the daemon.

- A manifest may list names from `env_pass` in `connect_env_socket`. On each launch,
  the daemon resolves any set values to absolute pathname sockets owned by its user and
  outside its data directory. The Linux launcher verifies the opened socket and applies
  a pathname connect rule on Landlock ABI 9 or later. Dry-run tool calls receive no such
  rules. An unset variable adds no rule.

## Handshake

The daemon speaks first; the adapter stays silent until it receives `initialize`. A frame sent before that is a violation.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocols":[1],"host":{"name":"oxidepmd","version":"0.4.0"},"adapter":{"name":"notifier","version":"0.1.0"},"kind":"service","grant":{"level":"read","scope":"all"},"paths":{"state":"/home/u/.oxidepm/adapters/state/notifier"},"limits":{"max_line":262144,"max_in_flight":32,"event_queue":1024}}}
{"jsonrpc":"2.0","id":1,"result":{"protocol":1,"name":"notifier","version":"0.1.0"}}
```

- The adapter must answer within 10 seconds, with the highest protocol it shares with `protocols`, and its own `name` and `version`.
- The daemon compares `name` and `version` with what it has on record. A difference means the executable is not what was installed: the adapter is killed and marked failed (`tampered`) and does not start again.
- A protocol that is not in the offered list, or an error `-32022` (optionally with `data.supported`), marks the adapter failed (`incompatible`).
- After a good reply the daemon sends the notification `initialized`. A `service` can then receive events, and the daemon can send requests to either kind.
- `kind` is `service` (resident, receives events) or `tool` (started on demand, exits when idle).
- A silent or broken handshake ends the process; it counts as a crash, and the supervisor restarts it with a growing delay until it gives up.

## Messages

Daemon to adapter, requests (the adapter answers with `result` or `error`):

| Method | Params | Result | Daemon today |
|---|---|---|---|
| `initialize` | see above | `{protocol, name, version}` | sent |
| `ping` | `{}` | `{}` | sent every 15 s to a service |
| `shutdown` | `{}` | `{}` | sent when the adapter is stopped |
| `runtime.prepare` | `PrepareParams` | `PrepareResult` | asks the runtime adapter for build steps |
| `runtime.plan` | `PlanParams` | `PlanResult` | asks the runtime adapter for a launch plan after any build steps run |
| `command.invoke` | `InvokeParams` | `InvokeResult` | runs an adapter verb on a service or a tool |

Daemon to adapter, notifications (no answer):

| Method | Params |
|---|---|
| `initialized` | `{}` |
| `event` | `{seq, kind, family, at, app?, data, options?, dropped_before?}` (below) |
| `cancel` | `{id}`: the request with that id was abandoned; a late answer will be dropped |

Adapter to daemon:

| Method | Kind | Daemon today |
|---|---|---|
| `host.request`, `host.audit_note` | request | answered by the host API (below) |
| `cancel {id}` | notification | accepted and ignored |
| `invoke.output {id, stream, text}` | notification | forwards output to the caller while a relayed verb runs |

The `provider.*` and `gate.*` method names are reserved.

## Session verbs

A verb whose manifest entry says `"mode": "session"` needs the terminal (a monitor) or the pipes of the program that started `oxidepm` (an MCP server over stdio). Only a `tool` adapter can provide one. The adapter's stdin and stdout are not the protocol channel for such a verb; everything else is unchanged.

1. `oxidepm <verb>` asks the daemon for a session. The daemon applies the policy and answers with a socket and a single-use token. The CLI connects, sends its own standard input and output as open descriptors with its first line, and waits.
2. The daemon starts the adapter under the same launcher and profile as any other call, with the executable it checked against the digest recorded at import. The adapter's **stdin and stdout are the descriptors the CLI handed over** (a terminal, or pipes); stderr is still its log and never reaches the terminal.
3. The adapter finds `OXIDEPM_SESSION_SOCKET` (a path under 100 bytes) and `OXIDEPM_SESSION_TOKEN` in its environment. It connects to the socket and sends one line within 60 seconds: `{"session":"adapter","token":"<token>"}`. Nothing else may be sent first. The token works once; the socket is removed as soon as the adapter is connected.
4. From the second line on it is the protocol described above, over that connection instead of stdin and stdout: the daemon sends `initialize`, then `initialized`, then `command.invoke` with `tty` set when the CLI's stdin is a terminal. The adapter's `host.request` calls are made as `adapter:<name>` and recorded in the audit log with the channel `adapter_session`.
5. The answer to `command.invoke` ends the session; its `exit_code` is the exit code of `oxidepm <verb>`. The session also ends when the CLI goes away (the adapter's process group is killed), when the adapter is disabled or the daemon stops, and after 12 hours at most. At most 4 sessions exist at once, pending ones included.

The adapter runs in the daemon's session, not the terminal's, so it has no controlling terminal: it cannot open `/dev/tty`, and a Ctrl-C typed at a terminal that is not in raw mode reaches the CLI, which ends the session. A terminal in raw mode delivers Ctrl-C as a key. When the window changes size the CLI tells the daemon, which sends `SIGWINCH` to the adapter's process group; the size is read with `TIOCGWINSZ` on descriptor 1. The CLI puts back the terminal settings it found when the session ends, whatever the adapter left.

An `ai:*` claim may open a session only for a verb of an adapter whose manifest says `ai_driven`, and only while that adapter holds no more than the `read` level; what such a session writes is limited to the adapter's state directory. A person may open any session verb.

## The host API

An adapter reaches the daemon through `host.request` and nothing else. `params.request` is the object the CLI would send (`{"type":"stop","selector":{"by":"name","value":"web"}}`); `params.dry_run` asks for a plan instead of the action (a request that only reads ignores it); `params.end_user` is an optional label (at most 64 characters, control characters removed) for the adapter's own end user. The answer's `result.response` is the daemon's response, exactly as the CLI would get it.

```json
{"jsonrpc":"2.0","id":1,"method":"host.request","params":{"request":{"type":"stop","selector":{"by":"name","value":"web"}},"dry_run":true}}
{"jsonrpc":"2.0","id":1,"result":{"response":{"type":"error","code":"E_POLICY_DENIED","message":"denied"}}}
```

- The daemon decides who is asking. The request is handled as the adapter, named by the connection, under the grant the user accepted when the adapter was enabled. Nothing the adapter writes can change that: an `actor`, `reveal`, `dry_run` or any other field inside the request object, or extra fields in `params`, is not read. Only `request`, `dry_run` and `end_user` are taken from `params`.
- The request goes through the same policy, audit log and dry-run handling as a request from the CLI. A refusal is a normal response (`type: "error"`, `code: "E_POLICY_DENIED"`, `"E_PROTECTED"`, `"E_RATE_LIMITED"`, `"E_SHUTTING_DOWN"`, ...), not a protocol error. The denied requests are written to the audit log, with the adapter's name.
- What the grant allows: `read` makes safe requests (`status`, `show`, `describe`, `logs`, `daemon_status`); `lifecycle` also stops, restarts and reloads apps; `admin` also starts apps and does the dangerous and global requests (`delete`, `flush`, `save`, `resurrect`). All of them only on apps that are not protected, and, if the grant's scope is `opted-in`, only on apps that list the adapter under `adapters`. `kill`, `set_policy` and revealing environment values are a person's, whatever the grant.
- The `end_user` label is written to the audit log as an unverified label. It is never used in a decision.
- `logs` for an adapter are scrubbed of the app's environment values (those of 6 or more characters, an exact-match filter) and bounded: at most 10 000 lines and 1 MiB (the newest lines are kept, the oldest cut). An answer that would not fit in a frame is replaced by `E_REQUEST_TOO_LARGE`.
- Limits: 32 requests in flight (the grant may ask for up to 128; the next one is answered with `E_ADAPTER_BUSY`, `-32029`); 50 requests a second, burst 100, and 5 requests that change something a minute, burst 10 (`E_RATE_LIMITED`). An adapter answered with `E_RATE_LIMITED` 20 times within 10 seconds commits a violation.
- A `params` that is not a request answers error `-32602`; that is not a violation.
- `host.audit_note` takes `{"text": "..."}` and writes a line to the audit log, tagged with the adapter's name and cut at 256 characters. It counts against the same rate, and a note is dropped, not waited for, when the log's queue is full.

```json
{"jsonrpc":"2.0","id":2,"method":"host.audit_note","params":{"text":"sent a message"}}
```

A process in an adapter's process tree that opens the daemon's main socket itself (for example the CLI, started by the adapter) is handled as that adapter, under its grant, whatever it claims. The daemon looks at the peer once, when the connection is accepted, and only while an adapter process is alive. A peer that cannot be looked at then (it is gone, or its process chain is too long) is treated as an AI, not as a person. An adapter that moves its helper out of its process tree is not found by this; the sandbox and the grant are the protection against that.

## Events

Events go to a running `service` that listed the family in its manifest and was granted it. There are two families.

`lifecycle`: something happened to an app. `seq` is the daemon-wide sequence number of the event. `kind` is one of `started`, `restarted`, `stopped`, `crashed`, `errored`, `unhealthy`, `recovered`, `backoff`, `crash_loop`, `cron_fired`, `memory_limit`, `uptime_limit`. `app` is `{id, name, tags}`. `data` holds only these keys, and only those the event has: `cause`, `actor`, `pid`, `exit_code`, `exit_signal`, `restarts`, `memory_mb`, `limit_mb`, `reason`. `reason` is a sentence made by the daemon with the app's environment values removed and cut at 1024 characters. `options` is the app's `adapters.<adapter name>` table for this adapter only (at most 4 KiB); no other adapter's options are ever sent. The event never carries the app's command, arguments, working directory, environment, hooks or log lines.

`adapter`: something happened to an adapter (`started`, `exited`, `killed`, `failed`). It has no `app`, has its own `seq` counter, and `data` holds `adapter`, `version` and, for `failed`, `reason` (a short name). An adapter is not told about itself.

Delivery is best effort and never slows the daemon. Each service has a queue of 1024 events plus 256 critical ones (`crashed`, `errored`, `unhealthy`, `crash_loop`, `memory_limit`; `killed` and `failed` for the adapter family), at most 4 MiB. When it is full the oldest normal event is dropped (the oldest critical one when the critical part is full). A service that is not running does not queue: its events are dropped. The drops are counted, and the next event delivered carries `dropped_before: n`. A gap in `seq` shows where. Events queued when the process ended are dropped, not sent to the next process. When the adapter is stopped on request, queued events are written (for up to a second) before `shutdown`.

An event line is at most 16 KiB. If one would be longer the daemon removes `options`, then `reason`, then the tags; if it still does not fit it is dropped and counted.

## Limits

All are enforced by the daemon and none depends on the adapter's cooperation.

| What | Limit | When exceeded |
|---|---|---|
| Line, adapter to daemon | 256 KiB | skipped without being stored; a violation |
| A frame the daemon would send | 256 KiB | a request is refused before it is sent (`E_ADAPTER_INVALID`); an answer to an adapter's request that is too large is replaced by error `E_REQUEST_TOO_LARGE` |
| `event` line | 16 KiB | trimmed, see above |
| Requests in flight, each direction | 32 (up to 128 for `host.request` when the grant asks) | the new request fails at once (`-32029`, `E_ADAPTER_BUSY`) |
| Frames from the adapter | 500 per second sustained, burst 1000; 8 MiB per second | the daemon stops reading, so the adapter blocks on its own pipe; throttled for 10 s without a break is a violation |
| Write to the adapter's stdin | no progress for 5 s | the connection is closed and the adapter killed |
| Strikes | 3 in 60 s | the adapter is killed and counts as a crash |
| Remembered expired request ids | 1024, for 5 minutes | see below |

A violation (a strike) is: a line that is not a valid frame, an oversize line, a frame before `initialize`, a response to an id that was never issued or was already answered, a reply that does not have the shape of its method, or being throttled too long. An adapter request with bad params is not a violation; it gets an error. A line that ends the stream half way, EOF, or a read error closes the connection.

## Timeouts and cancellation

- Every wait for an adapter is a timer in the daemon. The adapter's cooperation is not needed.
- `initialize`: 10 s. `ping`: 5 s to answer; three misses in a row kill a service (a tool is not pinged). Other request deadlines are chosen by the caller of the method.
- When a deadline passes, or the caller goes away, the daemon forgets the request at once, sends `cancel {id}` and remembers the id. A reply with that id (a late answer, or error `-32024` from an adapter that obeyed the cancel) is dropped and counted, without a strike. An id that was never issued is a strike.
- Shutdown: the daemon sends `shutdown` and waits up to 2 s, closes stdin (the adapter must also exit when its stdin closes), waits until 2 s are over, sends SIGTERM to the adapter's process group, waits until 5 s are over in all, then SIGKILL. The total is fixed.
- When the connection ends for any reason, every request in flight fails with `E_ADAPTER_CRASHED`.

## Errors

An error response is `{code, message, data?}`; `data.code` is the stable name. The numeric codes are the JSON-RPC ones (`-32700` parse, `-32600` invalid request, `-32601` method not found, `-32602` invalid params, `-32603` internal) and these of OxidePM: `-32022` unsupported protocol (`E_ADAPTER_PROTOCOL`), `-32023` permission denied (`E_POLICY_DENIED`), `-32024` cancelled, `-32025` timeout (`E_ADAPTER_TIMEOUT`), `-32026` shutting down, `-32027` dry run unsupported (`E_DRY_RUN_UNSUPPORTED`), `-32028` rate limited (`E_RATE_LIMITED`), `-32029` busy (`E_ADAPTER_BUSY`). The rest of the stable names are in `oxidepm_adapter_proto::codes`.

## What fails closed and what fails open

Closed (the operation does not happen):

- The executable's SHA-256 differs from the one recorded at install: it is not started (`E_ADAPTER_TAMPERED`).
- The handshake names another adapter or version, or no common protocol: the adapter is killed and marked failed.
- A request to an adapter that is missing, failed or not ready fails with a named error; there is no silent fallback.
- A frame that is not valid, too large or out of turn is never acted on.
- If the OS cannot enforce a required sandbox restriction, enable fails unless the user
  accepts `--allow-unenforced`. Adapter execution and the OS sandbox are Linux-only.
  The daemon also checks protocol calls against the grant.

Open (the daemon carries on):

- Events to a service are dropped, and counted, when its queue is full or it is down.
- An adapter that crashes, hangs, floods or breaks the protocol is killed and restarted with backoff; the apps the daemon supervises are not touched.
- Unknown fields, unknown notifications and stderr floods are ignored or dropped.

The daemon never stops, restarts, signals or changes an app because an adapter failed.
