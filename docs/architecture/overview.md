# Architecture

Codex Connect is a compact MCP projection over a persistent host and the official Codex App Server. ChatGPT is the only supported action/control interface and the primary technical operator. The local console is read-only observability, not a second operator. Codex Connect owns host authority and the adaptation boundary; App Server remains authoritative for Codex primitives and state whenever it exposes them.

## Architectural invariants

Treat these as the canonical review checklist. Detailed sections below explain the mechanisms; they do not weaken these rules.

1. **ChatGPT is the task owner and the only supported product action/control interface.** Trusted-host maintenance plumbing may diagnose, install, or deploy the backend, but it must not become a second operator surface. The console remains read-only, and worker/host mutation intent stays in ChatGPT.
2. **Authority stays in its plane.** PlatformPlane owns ChatGPT-native capabilities, HostPlane owns deterministic host/filesystem/process/Git/deployment reality, and WorkerPlane owns only delegated Codex lifecycle and cognition. No plane gains another plane's authority implicitly.
3. **Upstream primitives stay upstream.** Inspect the pinned App Server contract first. When it exposes a primitive, preserve its IDs, lifecycle, state, errors, and notifications instead of creating a competing Connect state machine or source of truth.
4. **The backend is shared, but not multi-tenant.** Independent ChatGPT operators for the same trusted user may act concurrently. Connect does not persist operator/session/project ownership, and backend-global recovery state is distinguished by cwd and authoritative resource IDs rather than caller identity.
5. **Every durable fact has one owner.** Components may adapt, validate, bound, or project another component's state, but they must not duplicate authority objects or mirror authoritative constants into a second owner.
6. **Workers own bounded delegated scope, not the overall task.** ChatGPT remains responsible for decomposition, integration, consequential host actions, and final verification. Parallel work must be non-overlapping; do not duplicate an active worker's investigation merely to create concurrency.
7. **Conversation or thread context is not runtime truth.** Revalidate mutable filesystem, Git, deployment, external, and live service state at the owning layer. Source changed, committed, pushed, prepared, deployed, live, plugin-discovered, and CI-green are distinct states and must not be inferred from one another.
8. **Reuse context deliberately.** Treat threads as durable workstreams with advisory cache hints: reuse a compatible thread when its context is useful, start fresh when settings change or independence matters, and do not spend compute recreating context already owned by an active compatible workstream.
9. **Long work is retained and event-driven.** Caller timeout, transport failure, or frontend detachment does not establish worker failure. Recover from retained handles and authoritative reads; do not poll App Server for progress. During long ChatGPT operations, human-visible progress is the liveness signal—SSE keepalives are not conversation progress.
10. **The network/security boundary stays outside the backend.** Codex Connect remains loopback-only; host ingress owns the public HTTPS route and OAuth enforcement. Codex CLI/App Server, Codex Connect, and host ingress retain independent lifecycles and state ownership.
11. **Pre-release changes are clean breaks.** There is no compatibility constituency yet: when semantics change, rename them coherently across code, schemas, tests, and docs. Do not add aliases, fallback readers, migration shims, dual paths, or product versioning without a present requirement.

## Three planes

```text
ChatGPT / PlatformPlane
        │ HTTPS / ngrok
        ▼
Host ingress (Caddy routing + OAuth)
        │ loopback MCP
        ▼
Codex Connect
  ├─ HostPlane: host.*, command.*, status; trusted-host deployment plumbing
  └─ WorkerPlane: codex.* → official Codex App Server
```

PlatformPlane is ChatGPT-native web/files/plugins/apps/Work/browser/Scheduled Tasks. It has no implicit host access. HostPlane is deterministic and authoritative for the OS account's filesystem, processes, Git, and deployment. WorkerPlane is delegated autonomous work/review; workers do not inherit ChatGPT conversation, native tools, files, or credentials.

There is intentionally no second control UI. The local console may display worker state and transcripts, but all actions remain in ChatGPT. Other local CLI commands are trusted-host maintenance/development plumbing.

Host ingress owns the canonical public URL, TLS edge configuration, routing, and OAuth boundary. Codex CLI/App Server and host ingress are independently managed dependencies; their binaries, credentials, and state are not Codex Connect-owned.

Tools advertise OAuth scope `codex-connect:access`. Host ingress validates tokens and strips credentials before forwarding to the loopback backend.

MCP Events use ingress-authenticated metadata for delivery routing and revocation,
not worker or ChatGPT conversation ownership. A short-lived opaque context is
resolved by ingress; subscriptions retain a separately authenticated opaque grant
context and check its current grant through the private loopback authority. The
fixed operator and existing OAuth provider remain the only identity model.

MCP Events let a subscribed ChatGPT chat be notified after a retained worker
finishes, even when the initiating assistant turn has already ended, without
manual polling. Event delivery never grants conversation ownership.

Only `codex.turn.terminal` is implemented, with exact canonical `threadId` and
`turnId` filters; extra or wildcard filters are rejected. Authenticated
`server/discover` projects the Events capability, and `events/list` describes
that sole definition. Both notifications and authoritative reads pass through
relay terminal reconciliation. A compact observer records a stable logical event
and matching outbox entries durably before callback I/O. The MCP Events service
owns bounded subscription/delivery state, webhook verification and delivery; it
is not a second App Server worker lifecycle. Restart rechecks grants before restoring
observation through the relay's existing thread-subscription owner. Multiple
callbacks can independently observe the same turn. Missed offline transitions
cannot be recovered through a replay cursor; explicit authoritative terminal
reads may project an existing terminal fact without creating a new logical ID.

## Component ownership

Keep implementation ownership as narrow as the plane model:

- `host` owns shared atomic private-file storage and OS lock primitives, host path validation plus only the deterministic host mechanics for which the pinned App Server has no equivalent client primitive. The backend creates one validated `Host` authority and shares it with Relay and MCP dispatch.
- `app-server` owns the pinned Codex protocol transport, official request/response contracts, and every upstream primitive/state machine that Connect consumes.
- `relay` adapts and composes App Server/Host operations into ChatGPT-usable lifecycles. It owns execution/wait budgets and bounded recovery projections, not upstream state or public MCP schemas.
- `mcp` owns the terminal Events subscription/outbox service, the public ChatGPT catalog (14 model-visible tools plus the app-only worker snapshot helper), input parsing/dispatch, transport policy, OpenAI/MCP descriptor projection, client-facing guard budgets, and the embedded Workers UI resource.
- `cli` is the composition root plus trusted-host installation, service, deployment, and diagnostic plumbing. It is not a product interaction surface.

Do not duplicate an authority object or mirror App Server-owned constants into the composition root. Runtime status should read invariants from the component that owns them.

### Upstream-first ownership

For every new capability, inspect the pinned App Server contract before designing a Connect abstraction. If App Server already exposes the primitive, route through it and preserve its IDs, lifecycle, state, errors, and notifications. Connect may normalize or combine those semantics for ChatGPT, but it must not create a second authoritative model.

Today that principle is visible in thread/turn/review lifecycle, model and skill discovery, usage, streaming commands, background terminals, approvals, permissions, elicitation, filesystem reads/directory listings/metadata, image-byte reads, and fuzzy file discovery. Connect validates host paths and response bounds around those calls, then projects operator-friendly results. Connect-native host mechanics remain only where the pinned client protocol has no equivalent operator primitive, notably literal content search and deterministic patch application.

## Authority and runtime defaults

The managed backend has no Codex Connect configuration file. It binds the fixed loopback endpoint `127.0.0.1:8767`, uses the service HOME as the default cwd, and resolves `codex` from the service PATH established by setup. App Server verifies the pinned release during startup. HostPlane calls may select another cwd; fresh `codex.start` work and review require an explicit cwd. Absolute paths are accepted. None of these path defaults is an authorization boundary.

One backend serves independent ChatGPT operators concurrently. Connect has no operator, session, or project ownership state: status and console are backend-global, and cwd identifies where a worker or retained command ran. Status reads never consume recovery handles. Usage remains available through the console and `codex.query`.

HostPlane uses the OS account's authority, and the dedicated App Server is launched with the process-local `sandbox_mode="danger-full-access"` override. The manual `serve` command retains explicit runtime flags for diagnostics; those overrides are not managed configuration.

For `codex.start(mode=work)`, omitted `access` means the canonical writable workspace sandbox with network access; `access="full"` means `danger-full-access`. Connect sends `approvalPolicy="never"` for work turns. That prevents mechanical approval stalls but does not enlarge the sandbox. Reviews are read-only.

Fresh workspace work may supply `writableRoots` as additional absolute write directories while keeping `cwd` as its primary working directory. Omitted roots and an empty list preserve the default scope. Full access, review, resume, and fork reject this field. App Server owns path enforcement and policy persistence; see [the pinned persistence limitation](../development.md#writable-root-persistence).

Connect does not send `thread/start.developerInstructions`. Worker cognition is ordered by upstream `~/.codex/config.toml` `developer_instructions`, `~/.codex/AGENTS.md`, repository/directory `AGENTS.md`, and the delegated task. Connect owns authority, lifecycle, and operator orchestration; it does not add a competing instruction source.

## App Server reuse and state

App Server is the authority for official IDs and lifecycle. Connect adapts its methods rather than exposing a second session model:

Treat a Codex thread as a durable workstream with advisory cache hints, not a disposable invocation. Fresh work and review establish an explicit cwd and model. Every `codex.start` supplies a model; resume and fork validate it against canonical thread metadata, inherit the canonical cwd, and reject cwd mutation. Work turns send the canonical model and effort to App Server. Cache age never gates resume or fork; the 30-minute connector reuse hint only informs the operator's choice while native persisted threads remain recoverable. Setting changes, unrelated work, and intentionally independent/adversarial review require a fresh thread.

Thread context is not runtime authority. Mutable repository, filesystem, Git, deployment, and external state must still be revalidated when current reality matters.

- `codex.start` composes fresh/resumed/forked thread work or read-only review. Forking copies persisted context into a new workstream; resume and fork share the same native permission checks and neither applies a cache-age gate.
- `codex.wait` synchronizes one delegated turn at an actual operator dependency boundary and returns terminal/action-required/input-required state; it is not a progress-polling primitive.
- `codex.inspect` projects bounded semantic activity, raw relay notifications, or the canonical handoff result in bounded text chunks.
- `codex.query` reads Codex-owned discovery, persisted thread metadata/listings, and thread-owned background terminals.
- `codex.act` steers or interrupts, answers pending requests, manages persisted thread archival/deletion, and terminates thread-owned background terminals.
- `command.start/read/control` preserve the official streaming command lifecycle, including PTY stdin, resize, and termination. Start and write compose that operation with a retained observation in one response; they create no additional command lifecycle.

Connect journals and caches are bounded observations; App Server owns lifecycle state. The relay retains only the projections needed for ChatGPT recovery, bounded inspection, current operator telemetry, and the read-only local console. Public MCP results expose operator-relevant data rather than a shadow App Server object graph. Loopback observer routes feed the console only and must remain mutation-free.

Use the protocol pinned by `config/codex-cli-pin`. App Server methods are mandatory when an equivalent primitive exists; Connect supplies only missing HostPlane semantics.

## Delegation ownership

```text
codex.start → useful non-overlapping operator work
           → synchronization boundary → codex.wait ─┬─ terminal
                                                    ├─ pending action/input → codex.act
                                                    └─ lease expiry → continue work or codex.inspect
```

Workers own delegated scope until terminal state, required action/input, interruption, or user redirect. Start completion survives caller loss because worker registration is independent of response delivery. The non-destructive backend-global `status.workers` projection exposes active and recent delegated handles with effective cwd so a fresh operator turn can rehydrate after frontend/caller interruption without duplicating work. `codex.wait` has a fixed server budget and wakes on terminal state or required action/input; expiry leaves the worker active and does not justify an immediate repeated wait. See [Operations](../operations.md#worker-lifecycle) for recovery.

Events drive live state. Every authoritative observation of a terminal turn passes through the same relay reconciliation path, which updates retained turn state, and releases thread subscription ownership. Active delegated turns are never retention-eviction candidates; recent terminal observations use bounded cwd-fair retention. Retained command sessions use the same principle for terminal handles. Reads hydrate existing turns, restore subscriptions, reconcile history loss or wait expiry, and project at most one canonical handoff message into `codex.wait`, capped at 10,240 characters. `selectionIncomplete` reports when the wait scan cannot establish the canonical choice; the item's `truncated` field reports text clipping only. This projection does not alter App Server thread history. `codex.inspect` with `detail: "result"` searches App Server items newest-first until a final answer, end of turn, or internal deadline. Its result is authoritative only when `resultPage.selectionComplete` is true. Paging adapts to aggregate transport limits, but a single item larger than the App Server transport limit leaves selection incomplete. Result lookup is independent of relay journal retention; `detail: "raw"` reads relay notifications, which may be lost. No periodic App Server read is used to observe progress.

The relay owns execution and wait budgets. MCP guards allow for finalization and response delivery. Transport failures and wait expiry do not establish worker failure.

## Public surface

The **Workers** conversation panel is a read-only MCP App in ChatGPT, opened by
`workers.open` or its thread entrypoint. It renders current/recent retained workers
by cwd, runtime readiness/build, activity, and informational pending requests.
The initial opener snapshot and explicit Refresh use the existing relay observer
projection; canonical selected results use `codex.inspect` and its authority/text
pagination contract. Context attachment and inspection requests travel through
the pinned MCP Apps host bridge. Worker mutation remains a ChatGPT model action.
No new service, ingress, authentication, hosted frontend, or periodic browser
polling is introduced. See [panel development](../development.md#conversation-worker-panel).

The supported human interface is ChatGPT using the public MCP/plugin catalog. See the [public tool list](../../README.md#public-mcp-surface); live schemas are authoritative for tool inputs, outputs, annotations, OAuth metadata, and limits.

## State boundaries

Verify source, Git, deployed artifacts, live build identity, ChatGPT plugin discovery, and CI separately. Backend deployment does not commit/push, rescan the ChatGPT plugin, or restart ingress.
