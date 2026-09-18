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
| Thread/turn ownership, resume/read/pagination, start/steer/interrupt, review, and terminal reconciliation | App Server thread/turn lifecycle contract | protocol integration tests and the complete contract-surface smoke test |
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

`experimentalApi` is enabled because Codex Connect intentionally supports `item/tool/requestUserInput`. The dedicated App Server launch also enables the pinned `default_mode_request_user_input`, `request_permissions_tool`, and `exec_permission_approvals` flags, while initialize advertises the `openai/form` extension. These are explicit integration requirements, not blanket permission to expose unrelated experimental methods.

## App Server reuse gate

Before implementing or retaining a public MCP behavior, generate or inspect the protocol schema from the pinned Codex CLI and search for an equivalent App Server method. Prefer, in order:

1. a narrow projection of the official method;
2. a scope/bounds/transport adapter around the official method;
3. a Connect-only bridge only when the pinned App Server lacks the semantic operation.

A bridge needs an explicit architectural justification and should be reconsidered whenever the Codex pin changes. Do not keep parallel implementations for convenience alone. In particular, file/directory name discovery belongs to App Server `fuzzyFileSearch`; `inspect.searchContent` remains Connect-owned only because the current pin has no workspace-content-search RPC.

## Tool-selection calibration

### Sandbox ownership invariant

Host `command.exec` / `command.start` must preserve an omitted `sandboxPolicy` all the way to App Server. **Absence is semantic:** Codex Connect sends no synthetic policy, so App Server uses the effective upstream Codex configuration from `$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`). Do not replace omission with an equivalent-looking `workspaceWrite` object, because doing so would duplicate upstream defaults and create configuration drift.

`codex.work.start` has the opposite contract: it must reject omission and always send the operator-supplied `sandboxPolicy` on `turn/start`. Do not add Codex Connect configuration fields for `sandbox_mode` or `sandbox_workspace_write.network_access`.

The MCP tests contain deterministic golden examples such as:

- “find where Relay is defined” → one batched `inspect` call
- “show status, recent commits, and changed files” → one composed `command.exec` call
- “run cargo test” → `command.exec`
- “start the dev server and keep it running” → `command.start`
- “read the new output from the dev server” → `command.read`
- “send input to the debugger” → `command.write`
- “resize the debugger terminal” → `command.resize`
- “stop the running dev server” → `command.terminate`
- “investigate these failures and fix them” → `codex.work.start`
- “wait for the coding agent” → `codex.work.wait`
- “review uncommitted changes” → `codex.review`
- unrelated prompts → no Codex Connect tool

Extend this fixture when adding or materially changing tool metadata.

Operator-facing output schemas should expose state that Connect already knows rather than forcing the caller to reverse-engineer bounds. In particular, streaming command reads expose `hasMoreOutput` and `drained`, buffered commands expose byte-count/cap telemetry without claiming definitive upstream truncation, and batched inspection keeps concrete result schemas for every operation type.

For the architectural rationale, see [Architecture](architecture/overview.md).
