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
python tests/protocol_integration.py
```

The schema check requires the project-pinned Codex CLI to be installed and available as
`codex`. The protocol integration suite requires Python with the `jsonschema` package.

## Pinned App Server contract

`config/codex-cli-pin` is the single Codex CLI contract pin. The project does not depend on Codex's internal Rust API; it validates its internal JSON-RPC adapter against schemas emitted by that pinned release.

`scripts/generate-app-server-tool-schemas.py`:

1. verifies the installed Codex release equals the pin,
2. runs `codex app-server generate-json-schema`,
3. verifies the request definitions used internally by the adapter, and
4. writes the self-contained subset to `config/app-server-tool-schemas.json`.

The artifact is an **internal protocol drift guard**, not the MCP catalog.

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

`experimentalApi` is enabled only because Codex Connect intentionally supports `item/tool/requestUserInput`. Treat that request as a named exception, not as permission to expose other experimental methods.

## Tool-selection calibration

The MCP tests contain deterministic golden examples such as:

- “find where Relay is defined” → `codexConnect.inspect`
- “run cargo test” → `command.exec`
- “investigate these failures and fix them” → `codexConnect.work.start`
- “wait for the coding agent” → `codexConnect.work.wait`
- “review uncommitted changes” → `codexConnect.review`
- unrelated prompts → no Codex Connect tool

Extend this fixture when adding or materially changing tool metadata.

For the architectural rationale, see [Architecture](architecture/overview.md).
