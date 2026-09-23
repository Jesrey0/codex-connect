# Architecture

Codex Connect is a compact MCP projection over a persistent host and the official Codex App Server. ChatGPT is the primary technical operator; Codex Connect owns host authority and delegated lifecycle, while App Server owns Codex threads, turns, reviews, requests, and execution semantics.

## Three planes

```text
ChatGPT / PlatformPlane
        │ HTTPS / ngrok
        ▼
Host ingress (Caddy routing + OAuth)
        │ loopback MCP
        ▼
Codex Connect
  ├─ HostPlane: inspect, patch, image, commands, status, deployment
  └─ WorkerPlane: codex.* → official Codex App Server
```

PlatformPlane is ChatGPT-native web/files/plugins/apps/Work/browser/Scheduled Tasks. It has no implicit host access. HostPlane is deterministic and authoritative for the OS account's filesystem, processes, Git, and deployment. WorkerPlane is delegated autonomous work/review; workers do not inherit ChatGPT conversation, native tools, files, or credentials.

Host ingress owns the canonical public URL, TLS edge configuration, routing, and OAuth boundary. Codex CLI/App Server and host ingress are independently managed dependencies; their binaries, credentials, and state are not Codex Connect-owned.

Tools advertise OAuth scope `codex-connect:access`. Host ingress validates tokens and strips credentials before forwarding to the loopback backend.

## Component ownership

Keep implementation ownership as narrow as the plane model:

- `host` owns host path resolution plus Connect-only search, image, and patch mechanics. The backend creates one validated `Host` authority and shares it with Relay and MCP dispatch.
- `app-server` owns the pinned Codex protocol transport and official request/response contracts.
- `relay` composes App Server operations into worker, command-session, observer, and action lifecycles. It owns execution/wait budgets and bounded live projections, not host authorization or public MCP schemas.
- `mcp` owns the public 14-tool catalog, input parsing/dispatch, HTTP observer/runtime routes, and client-facing guard budgets.
- `cli` is the composition and local-operations layer. Its console uses a small local backend client; deployment/service management does not own observer protocol behavior.

Do not duplicate an authority object or mirror App Server-owned constants into the composition root. Runtime status should read invariants from the component that owns them.

## Authority and runtime defaults

The managed backend has no Codex Connect configuration file. It binds the fixed loopback endpoint `127.0.0.1:8767`, uses the service HOME as the navigation cwd, and resolves `codex` from the service PATH established by setup. App Server verifies the pinned release during startup. Per-call `cwd` values override the navigation cwd; absolute paths are accepted. None of these path defaults is an authorization boundary.

HostPlane uses the OS account's authority, and the dedicated App Server is launched with the process-local `sandbox_mode="danger-full-access"` override. The manual `serve` command retains explicit runtime flags for diagnostics; those overrides are not managed configuration.

For `codex.start(mode=work)`, omitted `access` means the canonical writable workspace sandbox with network access; `access="full"` means `danger-full-access`. Connect sends `approvalPolicy="never"` for work turns. That prevents mechanical approval stalls but does not enlarge the sandbox. Reviews are read-only.

Connect does not send `thread/start.developerInstructions`. Worker cognition is ordered by upstream `~/.codex/config.toml` `developer_instructions`, `~/.codex/AGENTS.md`, repository/directory `AGENTS.md`, and the delegated task. Connect owns authority, lifecycle, and operator orchestration; it does not add a competing instruction source.

## App Server reuse and state

App Server is the authority for official IDs and lifecycle. Connect adapts its methods rather than exposing a second session model:

Treat a Codex thread as a cache-bounded workstream, not a disposable invocation. A new workstream establishes cwd, model, reasoning effort, and access. Connect sends the canonical thread model and effort explicitly on every work turn; resumed turns cannot change workstream settings. Connect conservatively refuses resume outside OpenAI's documented GPT-5.6+ minimum 30-minute cache lifetime even though OpenAI may retain cache entries longer. When live model-usage telemetry is retained, that exact cache-touch time drives the cutoff; after restart/history loss, the latest completed-turn time is the fallback. Setting changes, unrelated work, and intentionally independent/adversarial review require a fresh thread.

Thread context is not runtime authority. Mutable repository, filesystem, Git, deployment, and external state must still be revalidated when current reality matters.

- `codex.start` composes thread/turn work or read-only review.
- `codex.wait` synchronizes one delegated turn and returns terminal/action-required/input-required state.
- `codex.inspect` projects bounded semantic activity, or raw retained notifications when explicitly requested.
- `codex.control` steers or interrupts; `codex.action.respond` answers approvals, permissions, user input, and elicitation.
- `command.start/read/control` preserve the official streaming command lifecycle, including PTY stdin, resize, and termination.

Connect journals and caches are bounded observations; App Server owns lifecycle state. The relay retains detailed cache/context telemetry for observation, while the public MCP projection exposes only operator-relevant totals, cache-hit percentage, and guaranteed-reuse state/deadline. Raw inspection remains available for deeper App Server telemetry. The read-only console follows observer events, refreshes transcripts asynchronously with bounded retry, preserves the last good view through transient read failures, and marks cached quota data with the age of its last successful refresh when updates fail. Quota refresh is independent of worker observation; the UI clock only redraws.

Use the protocol pinned by `config/codex-cli-pin`. Prefer an App Server method when available; Connect supplies content search and deterministic patch semantics.

## Delegation ownership

```text
codex.start → codex.wait ─┬─ terminal
                         ├─ pending action/input → codex.action.respond
                         └─ lease expiry → codex.inspect or another bounded wait
```

Workers own delegated scope until terminal state, required action/input, interruption, or user redirect. Start completion survives caller loss, with unclaimed handles delivered in `workerStarted` events on host calls. `codex.wait` has a fixed server budget and wakes on terminal state or required action/input; expiry leaves the worker active. See [Operations](../operations.md#worker-lifecycle) for recovery.

Events drive live state. Every authoritative observation of a terminal turn passes through the same relay reconciliation path, which updates the observer projection, deduplicates terminal notification, and releases thread subscription ownership. Active delegated turns are never retention-eviction candidates; recent terminal observations are bounded separately. Reads hydrate existing turns, restore subscriptions, reconcile history loss or wait expiry, and load bounded terminal output. No periodic App Server read is used to observe progress.

The relay owns execution and wait budgets. MCP guards allow for finalization and response delivery. Transport failures and wait expiry do not establish worker failure.

## Public surface

See the [public tool list](../../README.md#public-mcp-surface) and live schemas for the operator interface.

## Version-state boundaries

Verify source, Git, deployed artifacts, live build identity, connector discovery, and CI separately. Backend deployment does not commit/push, refresh the connector, or restart ingress.
