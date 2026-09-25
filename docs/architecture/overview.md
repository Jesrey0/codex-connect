# Architecture

Codex Connect is a compact MCP projection over a persistent host and the official Codex App Server. ChatGPT is the only supported action/control interface and the primary technical operator. The local console is read-only observability, not a second operator. Codex Connect owns host authority and the adaptation boundary; App Server remains authoritative for Codex primitives and state whenever it exposes them.

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

There is intentionally no second control UI. The local console may display worker state and transcripts, but all actions remain in ChatGPT. Other local CLI commands are trusted-host maintenance/development plumbing.

Host ingress owns the canonical public URL, TLS edge configuration, routing, and OAuth boundary. Codex CLI/App Server and host ingress are independently managed dependencies; their binaries, credentials, and state are not Codex Connect-owned.

Tools advertise OAuth scope `codex-connect:access`. Host ingress validates tokens and strips credentials before forwarding to the loopback backend.

## Component ownership

Keep implementation ownership as narrow as the plane model:

- `host` owns host path validation plus only the deterministic host mechanics for which the pinned App Server has no equivalent client primitive. The backend creates one validated `Host` authority and shares it with Relay and MCP dispatch.
- `app-server` owns the pinned Codex protocol transport, official request/response contracts, and every upstream primitive/state machine that Connect consumes.
- `relay` adapts and composes App Server/Host operations into ChatGPT-usable lifecycles. It owns execution/wait budgets and bounded recovery projections, not upstream state or public MCP schemas.
- `mcp` owns the public 13-tool ChatGPT catalog, input parsing/dispatch, transport policy, OpenAI/MCP descriptor projection, and client-facing guard budgets.
- `cli` is the composition root plus trusted-host installation, service, deployment, and diagnostic plumbing. It is not a product interaction surface.

Do not duplicate an authority object or mirror App Server-owned constants into the composition root. Runtime status should read invariants from the component that owns them.

### Upstream-first invariant

For every new capability, inspect the pinned App Server contract before designing a Connect abstraction. If App Server already exposes the primitive, route through it and preserve its IDs, lifecycle, state, errors, and notifications. Connect may normalize or combine those semantics for ChatGPT, but it must not create a second authoritative model.

Today that principle is visible in thread/turn/review lifecycle, model and skill discovery, usage, streaming commands, background terminals, approvals, permissions, elicitation, filesystem reads/directory listings/metadata, image-byte reads, and fuzzy file discovery. Connect validates host paths and response bounds around those calls, then projects operator-friendly results. Connect-native host mechanics remain only where the pinned client protocol has no equivalent operator primitive, notably literal content search and deterministic patch application.

## Authority and runtime defaults

The managed backend has no Codex Connect configuration file. It binds the fixed loopback endpoint `127.0.0.1:8767`, uses the service HOME as the navigation cwd, and resolves `codex` from the service PATH established by setup. App Server verifies the pinned release during startup. Per-call `cwd` values override the navigation cwd; absolute paths are accepted. None of these path defaults is an authorization boundary.

HostPlane uses the OS account's authority, and the dedicated App Server is launched with the process-local `sandbox_mode="danger-full-access"` override. The manual `serve` command retains explicit runtime flags for diagnostics; those overrides are not managed configuration.

For `codex.start(mode=work)`, omitted `access` means the canonical writable workspace sandbox with network access; `access="full"` means `danger-full-access`. Connect sends `approvalPolicy="never"` for work turns. That prevents mechanical approval stalls but does not enlarge the sandbox. Reviews are read-only.

Connect does not send `thread/start.developerInstructions`. Worker cognition is ordered by upstream `~/.codex/config.toml` `developer_instructions`, `~/.codex/AGENTS.md`, repository/directory `AGENTS.md`, and the delegated task. Connect owns authority, lifecycle, and operator orchestration; it does not add a competing instruction source.

## App Server reuse and state

App Server is the authority for official IDs and lifecycle. Connect adapts its methods rather than exposing a second session model:

Treat a Codex thread as a cache-bounded workstream, not a disposable invocation. A new workstream establishes cwd, model, reasoning effort, and access. Connect sends the canonical thread model and effort explicitly on every work turn; resumed turns cannot change workstream settings. Connect conservatively refuses resume outside OpenAI's documented GPT-5.6+ minimum 30-minute cache lifetime even though OpenAI may retain cache entries longer. When live model-usage telemetry is retained, that exact cache-touch time drives the cutoff; after restart/history loss, the latest completed-turn time is the fallback. Setting changes, unrelated work, and intentionally independent/adversarial review require a fresh thread.

Thread context is not runtime authority. Mutable repository, filesystem, Git, deployment, and external state must still be revalidated when current reality matters.

- `codex.start` composes fresh/resumed/forked thread work or read-only review. Forking copies persisted context into a new workstream without applying the resume cache-age gate.
- `codex.wait` synchronizes one delegated turn and returns terminal/action-required/input-required state.
- `codex.inspect` projects bounded semantic activity, raw relay notifications, or the canonical handoff result in bounded text chunks.
- `codex.query` reads Codex-owned discovery, persisted thread metadata/listings, and thread-owned background terminals.
- `codex.act` steers or interrupts, answers pending requests, manages persisted thread archival/deletion, and terminates thread-owned background terminals.
- `command.start/read/control` preserve the official streaming command lifecycle, including PTY stdin, resize, and termination.

Connect journals and caches are bounded observations; App Server owns lifecycle state. The relay retains only the projections needed for ChatGPT recovery, bounded inspection, current operator telemetry, and the read-only local console. Public MCP results expose operator-relevant data rather than a shadow App Server object graph. Loopback observer routes feed the console only and must remain mutation-free.

Use the protocol pinned by `config/codex-cli-pin`. App Server methods are mandatory when an equivalent primitive exists; Connect supplies only missing HostPlane semantics.

## Delegation ownership

```text
codex.start → codex.wait ─┬─ terminal
                         ├─ pending action/input → codex.act
                         └─ lease expiry → codex.inspect or another bounded wait
```

Workers own delegated scope until terminal state, required action/input, interruption, or user redirect. Start completion survives caller loss, with unclaimed handles delivered in `workerStarted` events on host calls. `status.workers` also exposes active and recent delegated handles so a fresh operator turn can rehydrate after frontend/caller interruption without duplicating work. `codex.wait` has a fixed server budget and wakes on terminal state or required action/input; expiry leaves the worker active. See [Operations](../operations.md#worker-lifecycle) for recovery.

Events drive live state. Every authoritative observation of a terminal turn passes through the same relay reconciliation path, which updates retained turn state, deduplicates terminal notification, and releases thread subscription ownership. Active delegated turns are never retention-eviction candidates; recent terminal observations are bounded separately. Reads hydrate existing turns, restore subscriptions, reconcile history loss or wait expiry, and project at most one canonical handoff message into `codex.wait`, capped at 10,240 characters. `selectionIncomplete` reports when the wait scan cannot establish the canonical choice; the item's `truncated` field reports text clipping only. This projection does not alter App Server thread history. `codex.inspect` with `detail: "result"` searches App Server items newest-first until a final answer, end of turn, or internal deadline. Its result is authoritative only when `resultPage.selectionComplete` is true. Paging adapts to aggregate transport limits, but a single item larger than the App Server transport limit leaves selection incomplete. Result lookup is independent of relay journal retention; `detail: "raw"` reads relay notifications, which may be lost. No periodic App Server read is used to observe progress.

The relay owns execution and wait budgets. MCP guards allow for finalization and response delivery. Transport failures and wait expiry do not establish worker failure.

## Public surface

The supported human interface is ChatGPT using the public MCP/plugin catalog. See the [public tool list](../../README.md#public-mcp-surface); live schemas are authoritative for tool inputs, outputs, annotations, OAuth metadata, and limits.

## State boundaries

Verify source, Git, deployed artifacts, live build identity, ChatGPT plugin discovery, and CI separately. Backend deployment does not commit/push, rescan the ChatGPT plugin, or restart ingress.
