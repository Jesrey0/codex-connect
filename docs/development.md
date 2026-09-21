# Development

## Validation

Run the complete validation gate before submitting changes:

```bash
./scripts/generate-app-server-tool-schemas.py --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo check --locked
cargo build --locked -p codex-connect
python3 tests/protocol_integration.py
```

The schema check requires the project-pinned Codex CLI to be installed and available as
`codex`. The protocol integration suite requires Python with the `jsonschema` package.

## Pinned App Server contract

`config/codex-cli-pin` is the single Codex CLI contract pin. The project does not depend on Codex's internal Rust API; it validates its internal JSON-RPC adapter against schemas emitted by that pinned release.

`scripts/generate-app-server-tool-schemas.py`:

1. verifies the installed Codex release equals the pin,
2. runs `codex app-server generate-json-schema --experimental`, because the adapter
   explicitly negotiates `experimentalApi = true`,
3. verifies the request definitions used internally by the adapter, and
4. writes the self-contained subset to `config/app-server-tool-schemas.json`.

The artifact is an **internal protocol drift guard**, not the MCP catalog.
Its method, server-request, and notification sets must match the protocol paths the
adapter actually consumes semantically. Opaque journal-only notifications do not belong
in this closure.

The protocol integration fixture validates every selected App Server request and response
against this artifact and asserts that the complete selected method/server-request/
semantic-notification set is exercised. This is the contract-coverage gate; ordinary Rust
line coverage is useful separately but is not a substitute for protocol conformance.

### Contract coverage ledger

| Dependency | Authority | Enforcement |
| --- | --- | --- |
| Selected request/response field shapes, enums, requiredness, and experimental fields | JSON Schema emitted by the pinned Codex CLI | schema regeneration `--check`, schema tests, schema-valid fake App Server |
| `initialize` → `initialized`, negotiated capabilities, and required process feature flags | App Server initialization/experimental API contract | fake-peer handshake assertions plus app-server launch/capability tests |
| Thread/turn ownership, subscribe/resume/unsubscribe, read/pagination, start/steer/interrupt, review, and terminal reconciliation | App Server thread/turn lifecycle contract | protocol integration tests and the complete contract-surface smoke test |
| Command/file approvals, permissions, MCP elicitation, and request-user-input responses | App Server server-request/approval contract | exact selected server-request schemas, typed action unit tests, protocol integration responses |
| Connection-scoped streaming command control and `command/exec/outputDelta` | App Server streaming command contract | persistent command integration tests plus semantic-notification schema validation |
| Public ChatGPT-facing tool names and input/output projection | Codex Connect architecture, not the raw App Server catalog | exact MCP catalog tests plus Draft 2020-12 validation of every integration tool call/result |

When changing `config/codex-cli-pin`, review the generated schema diff and the official App
Server documentation sections for initialization, events/lifecycle, approvals/server
requests, streaming command behavior, and experimental API opt-in. Add a schema contract
only when Codex Connect semantically depends on it. Notifications retained solely as opaque
journal data remain intentionally outside the typed closure. Any newly selected contract
must also be traversed by the complete contract-surface integration test before the pin bump
is accepted.

## Dependency and compile budget

Compile cost is part of the operator experience. Keep required validation and release paths
lean rather than adding instrumentation or utility crates by default:

- prefer the standard library and existing workspace dependencies for small utilities;
- disable dependency default features when the operator contract needs only a smaller set;
- review `cargo tree -d --workspace` when adding or upgrading dependencies;
- do not add line/branch-coverage tooling to the required gate merely to produce a percentage;
- keep deployment Cargo output as a reusable build cache only. Content-addressed installed
  binaries and deployment records remain the release authority.

The deployment release target is intentionally stable across operations so Cargo can reuse
dependency artifacts. A deployment-wide build lock serializes release builds that share this
cache; changing operation IDs must not force a cold dependency rebuild.

## Public MCP ownership

The public catalog lives in `crates/mcp`. It is intentionally small and goal-oriented. Do not expose a new raw App Server method merely because it exists in generated schemas.

A new public tool must answer a distinct ChatGPT operator goal, have a precise safety boundary, and include:

- a description that distinguishes it from the nearest alternative,
- explicit `readOnly`, `destructive`, `openWorld`, and `idempotent` annotations,
- compact input/output schemas,
- deterministic catalog/tool-selection tests.

The catalog test is exact: the advertised tool names must equal the canonical public surface.

## Event journal

`crates/relay/src/event_journal.rs` is a bounded observational projection of official App Server notifications. It may help ChatGPT wait efficiently, but it must never become authoritative state. Official thread/turn reads and IDs remain authoritative.

When adding event handling:

- retain the raw official method and params,
- preserve monotonic cursors,
- keep retention and per-event sizes bounded,
- tolerate broadcast lag without inventing missing state.

## Server requests

Server-request response shapes must come from the pinned generated schemas. Public responders should normalize only the operator decision and translate it to the official shape. Never reintroduce a generic `result: any` public responder.

`experimentalApi` is enabled because Codex Connect intentionally supports `item/tool/requestUserInput`. Initialize advertises the canonical `openai/form` App Server extension and routes elicitation through the existing typed `codex.action.respond` surface; do not use the legacy `mcpServerOpenaiFormElicitation` compatibility flag. The dedicated App Server launch sets the host-plane `sandbox_mode="danger-full-access"` override and enables the pinned `default_mode_request_user_input`, `request_permissions_tool`, and `exec_permission_approvals` flags. These are explicit integration requirements, not blanket permission to expose unrelated experimental methods.

## App Server reuse gate

Before implementing or retaining a public MCP behavior, generate or inspect the protocol schema from the pinned Codex CLI and search for an equivalent App Server method. Prefer, in order:

1. a narrow projection of the official method;
2. a path/bounds/transport adapter around the official method;
3. a Connect-only bridge only when the pinned App Server lacks the semantic operation.

A bridge needs an explicit architectural justification and should be reconsidered whenever the Codex pin changes. Do not keep parallel implementations for convenience alone. In particular, file/directory name discovery belongs to App Server `fuzzyFileSearch`; `inspect.searchContent` remains Connect-owned only because the current pin has no workspace-content-search RPC.

## Tool-selection calibration

### Authority ownership invariant

Host `command.exec` / `command.start` expose no `sandboxPolicy`. Codex Connect owns one process-local App Server launch override, `sandbox_mode="danger-full-access"`, for the deterministic primary-operator plane. Host paths are not fenced to the configured default workspace; that directory is only the base for relative paths. Do not move this host-plane authority into `~/.codex/config.toml`, because the user-global Codex configuration must remain generic.

`codex.start(mode=work)` exposes operator intent rather than the pinned App Server sandbox object. Omitted `access` maps to the canonical `WorkspaceWrite` policy with network access enabled and no extra writable roots; `access="full"` maps to `DangerFullAccess`. Work turns always send upstream `approvalPolicy="never"`, which removes mechanical approval stalls without changing the selected sandbox boundary. Do not re-expose writable roots, network/temp flags, approval policy, per-turn service tier, or operator-supplied developer instructions as MCP knobs. New work threads receive the matching coarse sandbox mode plus the server-owned `WORKSPACE_POLICY`; resumed threads do not receive thread-level sandbox or developer-instruction overrides, while each new work turn still receives the selected canonical turn policy. New `mode=review` threads are explicitly read-only; an optional review `model` is carried by `thread/start.model`, while `review/start` remains byte-for-byte upstream.

Codex Connect is the primary control plane for ChatGPT. When a Codex semantic operation is public under `codex.*`, do not invoke the Codex CLI through host command tools as an alternate control plane. Delegated Codex workers do not inherit the calling ChatGPT conversation; every work task must therefore be self-contained. Prefer direct inspection, deterministic commands, and exact patches when they are sufficient, and delegate only when autonomous iteration or parallel reasoning materially improves the critical path or quality. Once a worker starts, it owns the assigned scope until terminal, blocked for operator action, or explicitly interrupted. Continue only non-overlapping operator work; an expired `codex.wait` lease means the worker is still active, not stalled, and must not trigger duplicate implementation. If useful non-overlapping work remains, do it. If the worker legitimately owns the remaining critical path for a long interval and no useful parallel work remains, prefer a ChatGPT Scheduled Task/monitoring handoff when the current product surface supports it and can access Codex Connect, rather than burning the conversation on repeated joins. The scheduled run must re-establish state through `codex.wait`/`status`; scheduling is observation orchestration, never a worker interrupt or ownership transfer. Host-plane result schemas may carry optional `workerEvents` for terminal turns or action-required states; those events are compact semantic interrupts, not a progress feed. `codex.wait` owns interactive synchronization, while `codex.inspect` owns explicit worker activity/history inspection.

ChatGPT-native capabilities are a separate PlatformPlane from both HostPlane and WorkerPlane. Use web/research for current public information, Files for user/project artifacts, Scheduled Tasks for deferred monitoring, Work/browser for supported web workflows, and installed plugins/apps for connected external systems when those surfaces are authoritative and available. Do not route host filesystem/process work through a connected app merely because an app can act externally, and do not scrape or shell around an external service when an authorized native app/plugin is the owning source. PlatformPlane permissions never widen HostPlane OS authority; HostPlane access never implies connected-account authority. A Codex worker does not inherit PlatformPlane tools, files, app/plugin sessions, or scheduled-task state unless required context is explicitly supplied in its self-contained task.

A deferred worker-monitor task must itself be self-contained. Include the Codex Connect identity plus the exact `threadId` and `turnId`, what state to verify, and what conditions justify notifying the user. Do not make the scheduled run depend on an uploaded file or Project file being implicitly available: ChatGPT Scheduled Tasks do not carry those files into a project-created task. If the Scheduled Tasks surface cannot access the Codex Connect connector in that run, keep coordination in the current foreground turn rather than pretending a handoff succeeded.

**Non-regression rule:** do not reintroduce a control flow where `codex.start` is followed by one bounded wait and then direct implementation of the same delegated task while the turn is still active. Any future change to worker orchestration must preserve exclusive ownership of a delegated scope until terminal/action-blocked/explicit-interrupt/user-redirect, preserve the distinction between worker/process lifetime and bounded join/read lifetime, and keep raw journal payloads off the normal wait path. Tool timeouts are semantic operator choices, not copies of tunnel poll/deadline settings. The public synchronous target is 40 seconds with a 45-second server guard: `codex.wait` defaults to a 20-second quiet lease and caps at 30 seconds while reserving up to 10 seconds for reconciliation/output hydration inside a 40-second total wait-operation budget, `command.read` defaults to 20 seconds and caps at 40 seconds, and `command.exec` defaults to 30 seconds with a 35-second child maximum plus 5 seconds of App Server response allowance. Tunnel-client independently owns any per-command outer response deadline supplied by the control plane; do not add a profile-level timeout merely to mirror these Connect-side budgets.

The MCP tests contain deterministic golden examples such as:

- “find where Relay is defined” → one batched `inspect` call
- “show status, recent commits, and changed files” → one composed `command.exec` call
- “run cargo test” → `command.exec`
- “start the dev server and keep it running” → `command.start`
- “read the new output from the dev server” → `command.read`
- “send input to the debugger” → `command.control(action=write)`
- “resize the debugger terminal” → `command.control(action=resize)`
- “stop the running dev server” → `command.control(action=terminate)`
- “investigate these failures and fix them” → `codex.start(mode=work)`
- “wait for the coding agent” → `codex.wait`
- “show what the coding agent has been doing” → `codex.inspect`
- “review uncommitted changes” → `codex.start(mode=review)`
- “answer the coding agent's question” → `codex.action.respond`
- “show models, skills, and usage” → one batched `codex.info` call
- unrelated prompts → no Codex Connect tool

Extend this fixture when adding or materially changing tool metadata.

Operator-facing output schemas should expose state that Connect already knows rather than forcing the caller to reverse-engineer bounds. In particular, streaming command reads expose `hasMoreOutput` and `drained`, buffered commands expose byte-count/cap telemetry without claiming definitive upstream truncation, and batched inspection keeps concrete result schemas for every operation type.

For the architectural rationale, see [Architecture](architecture/overview.md).
