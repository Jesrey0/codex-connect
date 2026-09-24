# Operations

Use [Getting Started](getting-started.md) for a fresh installation. This guide covers the steady-state owners, backend lifecycle, deployment, and recovery.

## Ownership

Codex Connect owns its backend service, deployment state/cache, and installed content-addressed artifacts. Codex CLI/App Server and host ingress have independent user-global lifecycles. Host ingress is the sole owner of public routing and OAuth.

The backend listens on loopback at `127.0.0.1:8767/mcp`. Public calls use `${NGROK_URL}/codex-connect/mcp` through ngrok and the OAuth-protected host ingress. Never expose the backend port directly.

## MCP streaming

The `tools/list` response stays JSON so the OpenAI `securitySchemes` projection can add its
root-level descriptor field without buffering or rewriting SSE. For 2025-06-18 and 2025-11-25
requests, ordinary stateless tool calls open a request-scoped SSE response immediately, with
comments every 15 seconds during a quiet call.

ChatGPT's observed `2026-07-28` requests take a different path in the pinned RMCP SDK:
it waits for the first handler message before opening the response stream. A quiet tool call
therefore receives neither SSE headers nor comments until its result is ready. Even when
comments are sent, the MCP client ignores them; they are not ChatGPT conversation progress.
The 2026 transport does not support `Last-Event-ID` resumability. Keep synchronous tools
inside the caller's measured result window and use retained worker or command handles for
long work. Origin validation remains enabled for the ChatGPT origins.

## Backend lifecycle

Initial setup is documented in [Getting Started](getting-started.md). Routine commands are:

```bash
codex-connect status
codex-connect doctor
codex-connect restart
codex-connect logs --follow
codex-connect console
codex-connect probe --codex-bin "$(command -v codex)" --cwd ~/src/example-project
```

`status` reports readiness, live build identity, navigation cwd, Codex defaults, retained persistent-command handles, and active/recent worker handles for operator rehydration. `doctor` provides local diagnostics. `console` follows worker activity, quota, pending actions, and transcripts without changing worker state.

## Host commands

Use `command.exec` for short, non-interactive commands and `command.start` for
long-running or interactive work. Commands take argv; invoke a shell explicitly
for pipes, redirects, or expansion. Set `tty=true` only when a terminal is needed.

After `command.start`, retain `processId`; the first `command.read` starts at cursor `0`, then
passes each returned cursor to the next read. If the start response is lost, `status.commands`
lists retained `processId`, state, and TTY mode for recovery. `timeoutMs=0` reads immediately; otherwise the read waits for output
or exit, up to 50 seconds per call. Timeout does not terminate the process. Continue until `drained=true` for
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

Retain `threadId` and `turnId`. Continue only non-overlapping operator work, then
call `codex.wait` with those IDs. It uses a fixed server wait and returns:

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
- `timeout`: the worker is active. Continue waiting or inspect activity; timeout
  does not establish a stall or authorize taking over its scope.

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

Host ingress owns `host-ngrok`, `host-ingress` (Caddy), and `host-oauth` services, their startup ordering, public URL, credentials, and reconnect behavior. Codex Connect readiness describes the local backend; it does not assert public reachability or a valid ChatGPT connection. Backend restart/deployment/uninstall do not manage ingress. Rediscover/refresh the ChatGPT app separately when its tool catalog changes.

## Restart recovery

After reboot, verify backend and ingress separately:

```bash
codex-connect status
codex-connect doctor
# from the host-ingress checkout
CHECK_PUBLIC=1 ./scripts/check
```

Restart only the failed service. User-systemd services are enabled independently; ngrok wants Caddy, and Caddy wants the OAuth service. OAuth grants/refresh state survive restarts. MCP clients initialize a new session after backend restart. Verify a real authenticated call after recovery rather than rerunning setup.

## Deployment

Deployment is a two-phase, backend-only workflow:

```bash
codex-connect deploy prepare
codex-connect deploy status <operation-id>
codex-connect deploy activate <operation-id>
codex-connect deploy status <operation-id>
```

`prepare` queues a detached release build and records a durable operation. Wait for `prepared`; `activate` queues the backend restart; the final `status` verifies the exact prepared artifact is live. The deployment build cache at `~/.cache/codex-connect/deploy/build` is a compiler cache, not runtime authority. Deployment does not restart ingress or refresh the connector.

Deployment records and managed unit files use the same durable atomic-write primitive. Deployment build, activation, and per-operation transitions use one file-lock mechanism with separate lock keys; these locks serialize local state changes but do not create a second runtime authority. Activation preflights the managed operator link before touching the backend, stages its replacement, and commits the service, operator link, and deployment record under the activation lock; a failed commit restores the previous link and service state, reporting any partial rollback explicitly.

Verify source, Git, prepared artifact, live build identity, connector discovery, and CI separately. Deployment does not commit or push.

## Runtime defaults and uninstall

The managed backend intentionally has no Codex Connect configuration file. Its loopback endpoint is `127.0.0.1:8767`, its navigation cwd comes from the service HOME, and `codex` is resolved from the service PATH and verified against the pinned release at App Server startup. Project-specific paths belong in tool-call `cwd` values rather than persistent backend state.

Worker defaults may be reported by `status` from the normal Codex global config; worker instruction sources remain the Codex config and AGENTS.md chain.

```bash
codex-connect uninstall
```

Uninstall removes Codex Connect's service, state/cache, operator symlink, and installed artifacts. It leaves the source tree, Codex CLI/App Server, and host ingress state untouched.

For security implications, see [Security](../SECURITY.md).
