# Development

Read [Architecture](architecture/overview.md) before changing authority, lifecycle, App Server integration, or service management.

## Pinned protocol contract

The Codex CLI/App Server release is pinned in `config/codex-cli-pin`. `scripts/generate-app-server-tool-schemas.py` verifies the installed release, generates its experimental schema, and checks the protocol subset used by the adapter.

`config/app-server-tool-schemas.json` is an internal protocol drift guard, not the public MCP catalog. **App Server primacy is the default design rule:** before adding a Connect feature, inspect the pinned App Server contract. If App Server exposes the primitive, use it and leave its lifecycle/state authoritative upstream. Connect may validate, compose, bound, or project that primitive for ChatGPT, but it must not create a competing state machine or duplicate source of truth.

That rule currently covers Codex threads/turns, review, model and skill discovery, usage, streaming commands, background terminals, approvals, permissions, elicitation, and App Server notifications. Connect-native HostPlane mechanics exist only where the pinned App Server has no equivalent client primitive for the operator use case. Examples include deterministic host patching and literal content search; fuzzy file discovery already delegates to App Server.

## Public MCP governance

The public catalog and schemas are owned by `crates/mcp/src/catalog.rs`; parsing and dispatch are in `crates/mcp/src/lib.rs`. Update both with protocol tests when changing inputs or outputs. `status` is backend-global and non-destructive; its `defaultCwd` describes HostPlane defaults, while retained worker and command handles carry effective cwd. `codex.start` requires model on every invocation and cwd on fresh work/review. Its input is an explicit union of fresh workspace/full work, resumed work, forked work, fresh review, and resumed review. Inherited-setting overrides are absent from resumed/forked variants; relay validation remains authoritative.

Public host tools use `host.inspect`, `host.apply_patch`, and `host.view_image`;
commands use `command.*` and workers use `codex.*`. Bare host names have no aliases.
Keep descriptions to selection guidance and essential lifecycle rules, with precise
constraints in schema fields and references. `codex.inspect.nextCall` and command
observation `nextCall` values are additive MCP projections over existing IDs,
cursors, and offsets, never another lifecycle or pagination store. Test direct
invocation against the consumed catalog, independently of explanatory wording.

The public endpoint supports only MCP `2026-07-28`: advertise that version in `server/discover` and require modern per-request metadata. There is no legacy `initialize` path or compatibility mode in the public endpoint. The integration client exercises this stateless contract. The separate Codex App Server handshake is internal to the pinned upstream protocol.

Keep ownership one-way. `crates/host` owns host-path validation and only those deterministic host mechanics that are genuinely Connect-native. `crates/app-server` owns the pinned upstream protocol/transport. `crates/relay` adapts and composes upstream/host primitives without becoming a second authority. `crates/mcp` projects that behavior into the ChatGPT-facing tool/HTTP surface. The CLI is the composition root plus maintainer-only installation/service/deployment/diagnostic plumbing; it is not a second human product interface.

### OpenAI Plugin/MCP descriptor contract

Use the current OpenAI Plugin documentation as the ChatGPT-specific contract and the MCP specification as the protocol contract. Do not preserve stale Apps/connector-era metadata merely because an older ChatGPT build once accepted it.

- Define focused tools around recognizable user goals rather than mirroring an internal API.
- Keep `name`, `title`, `description`, `inputSchema`, and `outputSchema` accurate and compact. Output schemas must describe the structured result actually returned.
- Set `readOnlyHint`, `destructiveHint`, and `openWorldHint` explicitly according to behavior. `idempotentHint` is optional and should be truthful when present.
- Declare OAuth with root-level per-tool `securitySchemes`. Do not emit the optional `_meta.securitySchemes` compatibility mirror.
- Use `_meta` only for documented MCP Apps/OpenAI extensions: invocation status, tool visibility, the Workers opener association and thread entrypoint, and resource display/CSP metadata. Keep OAuth at descriptor root.
- Keep server instructions short and cross-tool. Tool-specific selection guidance belongs in each tool description.

Current references:

- [OpenAI: Define tools](https://developers.openai.com/plugins/plan/tools)
- [OpenAI: Build an MCP server](https://developers.openai.com/plugins/build/mcp-server)
- [OpenAI: Plugin reference](https://developers.openai.com/plugins/reference)
- [OpenAI: Authentication](https://developers.openai.com/plugins/build/auth)
- [MCP 2026-07-28 protocol/version guidance](https://ts.sdk.modelcontextprotocol.io/v2/protocol-versions)

Expose choices that control operator intent and project upstream protocol data into that vocabulary at the MCP boundary. Do not leak fields that merely repeat input, fixed constants, ranking internals, or account metadata that cannot drive an operator action. Keep execution limits and the worker wait budget in the relay; MCP guards must allow operations to finalize. `codex.wait` accepts only thread and turn IDs and is a synchronization primitive, never a progress-polling mechanism. `command.read.timeoutMs` supports immediate reads and bounded waits for interactive processes.

### ChatGPT synchronous-result budget

Treat **~55 seconds as the engineering ceiling** for one synchronous ChatGPT → Code Mode → connector call on the empirically tested path. This is an observed frontend/code-runner boundary, not a documented OpenAI platform guarantee, so keep meaningful headroom instead of encoding the longest single successful sample as a contract.

Keep local MCP guards at or below 48 seconds to leave delivery headroom under the observed caller ceiling. The current implementation uses a fixed 33-second `codex.wait` join plus a 10-second finalization reserve, a fixed 33-second `command.exec` request plus a 10-second response allowance. `command.read` accepts an observation wait of `0..=43_000` ms. Start and stdin write may observe the same retained command for `yieldTimeMs=0..=10_000` ms (default 1 second) before returning. These are observation waits, never execution limits. Long work belongs behind retained `codex.start` or `command.start` handles. SSE keepalives, once a stream is open, can protect transport liveness but do not extend the ChatGPT result budget or count as conversation progress. In the pinned RMCP SDK, quiet calls do not open a response stream before the handler emits its first message.

Keep service tier, raw sandbox/approval policy, developer instructions, and cache controls internal. Preserve tool annotations, OAuth metadata, compact schemas, and PTY support. Test accepted inputs, outputs, and behavior without locking explanatory prose.

## Human-interface boundary

ChatGPT is the only supported action/control interface. All mutation intent enters through the ChatGPT MCP/plugin surface. Human-visible local observability is allowed only when it is read-only and cannot steer or mutate workers, host state, or App Server state.

The local `codex-connect console` is an explicitly supported read-only visibility surface for workers, transcripts, pending state, and usage. Its loopback observer routes exist only to feed that console. They must remain observational: no steering, approval responses, archive/delete, command control, or other mutation may be added there. Other local CLI commands are trusted-host maintenance and development plumbing: setup, service state, logs, diagnostics/probes, deployment, and internal entrypoints.

## Conversation worker panel

The Rust MCP runtime owns `workers.open`, the app-only `workers.snapshot` helper,
and the enumerated HTML resource (`text/html;profile=mcp-app`). The opener accepts
`{}` and advertises a thread entrypoint titled **Workers**. Only the opener has
`_meta.ui.resourceUri`; Refresh must not reopen the view. Resource metadata and
browser capabilities both declare fullscreen only. In supporting ChatGPT hosts
this is the conversation side panel; placement remains host-controlled.

Snapshots reuse Relay's retained observer projection and existing pending-request
projection. They are backend-global, grouped/filterable by effective cwd in the
view, and timestamped. No conversation/project ownership or upstream worker state
is added. Selection reads canonical results through `codex.inspect(detail=result)`;
`selectionComplete` controls authority independently of `hasMoreText` and
`nextTextOffset`. Late replies cannot replace another selected worker. Pending
requests are informational. Attach context uses `ui/update-model-context`; asking
ChatGPT to inspect uses `ui/message`. All mutation tools are model-only.

The browser source and locked dependencies live in `crates/mcp/ui`. Its bridge uses
`@modelcontextprotocol/ext-apps` **1.7.5**, with `@openai/mcp-extensions` **0.1.0**
([node-v0.1.0 source contract](https://github.com/openai/mcp-extensions/blob/4ff03c033e170d623f5101e885ce94dac959a1d9/docs/spec.md)).
Follow the [MCP Apps specification](https://github.com/modelcontextprotocol/ext-apps/blob/v1.7.5/specification/draft/apps.mdx)
and [official UI guide](https://developers.openai.com/plugins/build/chatgpt-ui).
Do not introduce another bridge or a browser connection to loopback ingress.

`workers.html` is the source-controlled compiled asset embedded by Rust; Cargo
builds and existing deployment builds need no Node toolchain. Rebuild it with
`npm run build` from `crates/mcp/ui` after browser changes. The resource URI hashes
the actual HTML, so rebuilt assets cannot reuse a stale host cache key. The bundle
has no runtime external assets/network access; resource CSP has empty domain
allowlists and HTML CSP permits only hashed inline script/style. Initial rendering
uses the delivered launch result with no duplicate fetch. Refresh and result
pagination are explicit; there are no background polling loops.

For browser work use Node **24.16 or newer** (native TypeScript test execution),
then run `npm ci` and `npm run check` in `crates/mcp/ui` before the Rust gate.
This checks types, embedded asset freshness, and real SDK bridge behavior using
in-memory host transport. Run focused protocol validation with
`python3 tests/protocol_integration.py OperatorProtocolTests.test_workers_resource_discovery_and_launch_contract OperatorProtocolTests.test_workers_snapshot_reuses_retained_projection_and_informational_pending_requests`
after building the CLI. A JSON resource/tool result proves protocol behavior,
not actual rendering or placement. Verify light/dark narrow/wide layouts, keyboard
selection, empty/stale/error/unsupported states, and initial delivery in a browser;
verify placement and context actions separately in the intended ChatGPT host.

## Worker contract

Worker instructions come from Codex config, AGENTS.md, and the delegated task. Connect supplies authority and lifecycle controls; it does not inject developer instructions.

Work uses `approvalPolicy="never"` with the selected workspace or full-access sandbox. Disabling prompts does not widen the sandbox. New reviews start read-only.

Preserve thread identity only while the workstream remains cache-compatible. Fresh work and review require explicit cwd and model. Every resume or fork supplies the canonical thread model, inherits cwd, and rejects cwd mutation. Work turns send canonical model and effort to App Server; effort and access remain inherited. Connect uses a conservative 30-minute guaranteed-cache cutoff rather than assuming best-effort retention beyond OpenAI's documented minimum for GPT-5.6 and later. Start fresh when settings must change, the cutoff is passed, or independence itself is useful, especially for adversarial review. Reuse never substitutes for checking mutable host/runtime state. See [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching).

Preserve the [worker lifecycle and recovery contract](operations.md#worker-lifecycle). Starts complete independently of caller lifetime. Active turns remain retained until terminal and terminal state from events or authoritative reads must use the same reconciliation path. Events drive live state; reads hydrate existing state, reconcile history loss or wait expiry, and load one canonical terminal handoff capped at 10,240 characters. Keep persisted thread history intact; `codex.inspect` with `detail: "result"` searches App Server turn items newest-first with adaptive page sizing. Treat the result as authoritative only when `resultPage.selectionComplete` is true; a single item above the App Server transport limit leaves selection incomplete. Result text is paged in 10,240-character chunks, independent of relay journal retention. Raw detail is the relay notification journal and can be lost. Do not poll App Server for progress.

Each tool descriptor advertises OAuth scope `codex-connect:access`. Host ingress owns token validation, discovery challenges, and authorization metadata.

### Writable-root persistence

Connect forwards fresh workspace `writableRoots` unchanged through `turn/start.sandboxPolicy`, with network access enabled, and sends no sandbox override for resumed or forked work. Fixture lifecycle tests verify that Connect leaves the inherited policy alone; they do not prove upstream persistence or OS enforcement.

The [App Server documentation](https://learn.chatgpt.com/docs/app-server) describes turn settings as defaults for later turns. Pinned 0.159.0 source, however, installs a raw sandbox override as an unnamed permission snapshot (`core/src/session/session.rs` and `protocol/src/permission_profile_snapshot.rs`). Cold resume and fork restore only the active permission-profile ID (`app-server/src/request_processors/persisted_resume_settings.rs` and `thread_processor.rs`), not the persisted concrete policy. Loaded-thread defaults are distinct from cold restoration. Additional roots therefore cannot be guaranteed across cold reload or fork from this interface alone. This is an integration blocker for durable root retention, not evidence that wire forwarding failed. No Connect-owned policy store or raw rollout parser is introduced here.

## MCP Events feature validation

Before the full gate, run `cargo test --locked -p codex-connect-mcp events`.
The fixture tests cover the actual RMCP HTTP custom dispatch/discovery boundary,
relay notification/read reconciliation, private authorization failures, callback
verification/signatures, filters, refresh/rotation/cancellation, queue limits,
retry exhaustion and restart recovery. The private ingress contract is owned and
tested separately by host-ingress with `./scripts/check source`; it keeps OAuth,
keys and grant state there. Do not use live ChatGPT subscriptions as source tests.
The atomic storage and OS lock primitives formerly in the CLI are shared through
`codex-connect-host::storage`; keep one owner when adding consumers.

Also run `cargo test --locked -p codex-connect-relay terminal_watch` for attachment
ordering, cancellation cleanup and monotonic terminal preservation. Events
integration tests release natural completion only after subscription, reject
follow-up turn reads on that path, and exercise recovery from a real oversized
transport notification without operator reads. These tests use the fixture peer;
they do not establish live ChatGPT callback acceptance or wake behavior.

## Validation

Run the repository gate with the pinned CLI installed and Python `jsonschema` available:

```bash
./scripts/generate-app-server-tool-schemas.py --check
cargo fmt --all -- --check
git diff --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --locked -p codex-connect
python3 tests/protocol_integration.py
```

The protocol tests validate the exact catalog, App Server request/response schemas, server requests, streaming commands, worker authority mapping, and end-to-end lifecycle. Do not weaken a test or schema to accommodate an unpinned CLI.

For documentation changes, verify local Markdown links and search for stale contract references. Validation does not include commit, push, deployment, or a ChatGPT plugin rescan.

## Dependency and change discipline

Keep durable product behavior separate from diagnostics and scratch tooling. Prefer existing abstractions and standard library code for small changes. Review dependency and compile impact. When semantics change, rename the concept across code, schemas, tests, docs, comments, and examples; do not retain aliases, fallback readers, migration shims, deprecated names, or dual paths without an explicit current requirement.

Prefer one concrete mechanism over speculative abstraction: share durable atomic-file writes, lock primitives, and validated authority objects instead of cloning their implementations. Split modules when they own a coherent state machine or boundary, not merely because a file is long. Do not add an interface with one implementation unless it creates a real testing or substitution boundary.

Codex Connect is pre-release and has no compatibility constituency to preserve. Prefer clean breaks over versioned compatibility layers. The workspace package version remains `0.0.0`; protocol-required implementation-version metadata is not a product release scheme.
