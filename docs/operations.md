# Operations

Use [Getting Started](getting-started.md) for a fresh installation. This guide covers the steady-state owners, backend lifecycle, deployment, and recovery.

## Ownership

Codex Connect owns its backend service, deployment state/cache, and installed content-addressed artifacts. Codex CLI/App Server and host ingress have independent user-global lifecycles. Host ingress is the sole owner of public routing and OAuth.

The backend listens on loopback at `127.0.0.1:8767/mcp`. Public calls use `${NGROK_URL}/codex-connect/mcp` through ngrok and the OAuth-protected host ingress. Never expose the backend port directly.

## MCP streaming

The public MCP endpoint accepts only `2026-07-28` requests with per-request metadata;
`server/discover` advertises that version alone. Each request is independent; the backend
does not use an `initialize` handshake or protocol-level sessions. The `tools/list` response
stays JSON so the OpenAI `securitySchemes` projection can add its root-level descriptor field.

The pinned RMCP SDK waits for the first handler message before opening a response stream.
A quiet tool call therefore receives neither SSE headers nor comments until its result is
ready. SSE comments are not ChatGPT conversation progress and cannot extend the measured
caller result window. The transport does not support `Last-Event-ID` resumability. Keep
local tool guards at or below 48 seconds and use retained worker or command handles
for longer work. Origin validation remains enabled for the ChatGPT origins.

## Backend lifecycle

Initial setup is documented in [Getting Started](getting-started.md). The following are
trusted-host maintenance commands, not a second product interface:

```bash
codex-connect status
codex-connect doctor
codex-connect restart
codex-connect logs --follow
codex-connect console
codex-connect probe --codex-bin "$(command -v codex)" --cwd ~/src/example-project
```

`codex-connect status` reports service and runtime identity; `doctor` provides local diagnostics. The MCP `status` tool also reports configuration read from the running App Server for the navigation cwd and retained command/worker handles for operator recovery. Null configuration values leave selection to App Server when a workstream starts; `configError` makes a failed read explicit without hiding recovery handles. `console` is a read-only visibility surface for workers, transcripts, pending state, and usage; it does not steer or mutate anything. All worker/host actions remain in ChatGPT through the MCP/plugin surface.

### Console navigation

The console uses the terminal's foreground/background theme. Task names lead the worker
list; the selected worker's detail area distinguishes cumulative thread tokens from
the latest request's input, context window, and cached input. Quota percentages explicitly
show usage consumed, with the window labels reported by App Server.

| View | Keys | Effect |
| --- | --- | --- |
| Workers | Up/Down or j/k; Enter | Select a worker; open its transcript |
| Workers | a | Toggle all workers / active workers |
| Workers | Home/End or g/G; Page Up/Down | First/last worker; move by a visible page |
| Transcript | Up/Down or j/k; Page Up/Down | Scroll and pause following |
| Transcript | Home/g; End/G | Start of retained text; follow latest output |
| Transcript | Escape, Left, b, or q | Return to workers |
| Either | ?; Escape while help is open | Show/close keyboard help |
| Either | Ctrl-C; q from workers | Exit and restore the terminal |

Resizing preserves selection by thread/turn identity. Small windows use a compact view.
Read failures retain the last successful data and visibly mark it stale; reconnecting
refreshes an open transcript even if backend revision counters restarted. UI ticks update
ages and animation locally. Observer notifications and transcript revisions drive backend
reads; the console does not poll App Server for progress. Pasted text is ignored and
pending actions remain informational: resolve them through ChatGPT.

## Host commands

Use `command.exec` for short, non-interactive commands and `command.start` for
long-running or interactive work. Commands take argv; invoke a shell explicitly
for pipes, redirects, or expansion. Set `tty=true` only when a terminal is needed.

After `command.start`, retain `processId`; the first `command.read` starts at cursor `0`, then
passes each returned cursor to the next read. If the start response is lost, `status.commands`
lists retained `processId`, state, and TTY mode for recovery. `timeoutMs=0` reads immediately; otherwise the read waits for output
or exit, up to 43 seconds per call. That ceiling leaves headroom below the measured ChatGPT
outer result window. Timeout does not terminate the process. Continue until `drained=true` for
final output; `historyLost=true` means older output was evicted. Termination can be
forceful: confirm exit with `command.read`. Process handles belong to the backend's
App Server connection and do not survive its restart.

## Worker lifecycle

Treat a Codex thread as a cache-bounded workstream. A new work thread receives a
self-contained task and may select cwd, model, reasoning effort, and access. Those settings
become workstream state. A resumed work turn supplies only `threadId` plus the next
objective/delta; Connect explicitly resends the canonical thread model and effort while
keeping cwd/access fixed. Review follows the same shape: cwd/model are creation-time choices,
while a resumed review supplies only its thread and target.

Connect conservatively accepts resume only inside OpenAI's minimum 30-minute prompt-cache
guarantee, even though the cache may survive longer. Live model-usage telemetry drives that
cutoff when available; after restart/history loss, latest completed-turn time is the fallback.
Outside that cutoff, or when cwd/model/effort/access must change, start a fresh thread with
self-contained context. Revalidate mutable repository, Git, runtime, and external state even
inside a reused thread.

Use `codex.query` to discover model IDs and supported effort. `codex.start` returns the
effective model/effort reported for the workstream. `codex.wait` and semantic inspection
surface compact context/cache telemetry: total/context-window tokens, latest cache-hit
percentage, the minimum cache-guarantee deadline, and whether that guarantee is active.

When durable thread context is still useful but normal resume is outside that cache window,
`codex.start` can fork the persisted thread into a new workstream. A fork may optionally stop
at a specific completed source turn. Forking preserves conversation history; it does not
promise prompt-cache reuse.

Retain `threadId` and `turnId`. Continue useful non-overlapping operator work, then
call `codex.wait` with those IDs only at a real synchronization boundary, when the
worker result or required action is needed. Do not use repeated waits as a polling loop;
use `codex.inspect` for a non-blocking activity check when one is actually useful. The
wait uses a fixed server budget and returns:

- `terminal`: read the turn status, canonical handoff, and error. `turn.output` stays an
  array and contains at most one message, chosen in this order: a `final_answer`
  `agentMessage`, an `exitedReviewMode` item, then the newest `agentMessage`. Handoff
  text is capped at 10,240 characters. `truncated` reports text clipping;
  `selectionIncomplete` separately reports when the bounded scan could not establish
  whether a handoff exists or whether a higher-priority item exists. For either case,
  use `codex.inspect` with `detail: "result"` and `textOffset`. It searches persisted
  turn items newest-first until it finds a final answer, exhausts the turn, or reaches
  its internal time budget. The result is authoritative only when
  `resultPage.selectionComplete` is true; false means it has only the best candidate
  found so far. Text is returned in bounded 10,240-character chunks; follow
  `resultPage.nextTextOffset` until `hasMoreText` is false. An item that itself exceeds
  the App Server transport limit cannot be recovered by this mode, and leaves selection
  incomplete. Result lookup works independently of relay journal retention and leaves
  thread history intact.
- `actionRequired` or `inputRequired`: respond with `codex.act`, matching
  the pending kind, requestId, and requested answers or decisions; then wait again.
- `timeout`: the worker is active. Continue other useful non-overlapping work or inspect
  activity when needed; do not immediately re-enter a wait loop. Timeout does not
  establish a stall or authorize taking over its scope.

Use `codex.inspect` semantic/raw details and `afterCursor` for activity and retained journal
notifications; use `detail: "result"` and `textOffset` to retrieve the persisted handoff.
Treat it as authoritative only when `resultPage.selectionComplete` is true. Raw
notifications can be lost with the relay journal. Use `codex.act` to steer
or interrupt, and `codex.wait` to confirm terminal state after interruption.

`codex.query` also reads persisted thread metadata/listings and App Server background terminals.
`codex.act` archives, unarchives, or deletes persisted threads and can terminate a background
terminal owned by a Codex thread. These background terminals are distinct from HostPlane
`command.start` sessions and remain App Server-owned.

If the ChatGPT caller/frontend is interrupted while the backend remains healthy, call
`status` in the next turn before starting replacement work. Its `workers` projection lists
active workers first and newest retained terminal workers after them. Reattach with the
reported `threadId`/`turnId` and reconcile through `codex.wait` or `codex.inspect`.

A lost `codex.start` response does not cancel creation. Do not retry immediately.
HostPlane responses can carry a `workerStarted` recovery receipt with the missing
thread/turn IDs. Receipts are reconciled from the relay's bounded retained-worker
state and replay until `codex.wait`, `codex.inspect`, or another known-turn control
operation claims them. `status.workers` is the deterministic recovery projection once
the worker is registered, so consult it before considering a retry. `workerEvents` can also report completion or
required action; those notifications take delivery priority over start receipts.
`historyLost` means older notification or recovery state was evicted. After a
backend restart, use retained thread/turn IDs to reconcile through `codex.wait`.

## Ingress lifecycle

Verify ingress independently:

```bash
# from the host-ingress checkout
./scripts/status
CHECK_PUBLIC=1 ./scripts/check
```

Host ingress owns `host-ngrok`, `host-ingress` (Caddy), and `host-oauth` services, their startup ordering, public URL, credentials, and reconnect behavior. Codex Connect readiness describes the local backend; it does not assert public reachability or a valid ChatGPT connection. Backend restart/deployment/uninstall do not manage ingress. Rescan/refresh the ChatGPT plugin separately when its tool catalog changes.

## Restart recovery

After reboot, verify backend and ingress separately:

```bash
codex-connect status
codex-connect doctor
# from the host-ingress checkout
CHECK_PUBLIC=1 ./scripts/check
```

Restart only the failed service. User-systemd services are enabled independently; ngrok wants Caddy, and Caddy wants the OAuth service. OAuth grants/refresh state survive restarts. MCP requests carry their own metadata; verify a real authenticated call after backend recovery rather than rerunning setup.

## Deployment

Deployment is a two-phase, backend-only workflow:

```bash
codex-connect deploy prepare
codex-connect deploy status <operation-id>
codex-connect deploy activate <operation-id>
codex-connect deploy status <operation-id>
```

`prepare` queues a detached release build and records a durable operation. Wait for `prepared`; `activate` queues the backend restart; the final `status` verifies the exact prepared artifact is live. The deployment build cache at `~/.cache/codex-connect/deploy/build` is a compiler cache, not runtime authority. Deployment does not restart ingress or rescan the ChatGPT plugin.

Deployment records and managed unit files use the same durable atomic-write primitive. Deployment build, activation, and per-operation transitions use one file-lock mechanism with separate lock keys; these locks serialize local state changes but do not create a second runtime authority. Activation preflights the managed operator link before touching the backend, stages its replacement, and commits the service, operator link, and deployment record under the activation lock; a failed commit restores the previous link and service state, reporting any partial rollback explicitly.

Verify source, Git, prepared artifact, live build identity, ChatGPT plugin discovery, and CI separately. Deployment does not commit or push.

## Runtime defaults and uninstall

The managed backend intentionally has no Codex Connect configuration file. Its loopback endpoint is `127.0.0.1:8767`, its navigation cwd comes from the service HOME, and `codex` is resolved from the service PATH and verified against the pinned release at App Server startup. Project-specific paths belong in tool-call `cwd` values rather than persistent backend state.

`status.codex.config` reads the running App Server's `config/read` response for the navigation cwd. It does not parse `config.toml` or guess a model when the upstream value is null. `codex.start` reports the model and effort assigned to the actual workstream. Worker instruction sources remain the Codex config and AGENTS.md chain.

```bash
codex-connect uninstall
```

Uninstall removes Codex Connect's service, state/cache, operator symlink, and installed artifacts. It leaves the source tree, Codex CLI/App Server, and host ingress state untouched.

For security implications, see [Security](../SECURITY.md).
