# Contributing

Codex Connect is a narrow, canonical-only pre-release bridge. Contributions must preserve the ownership boundaries in [Architecture](docs/architecture/overview.md): ChatGPT is the primary operator, HostPlane owns deterministic host work, WorkerPlane owns delegated Codex work, and PlatformPlane is ChatGPT-native.

## Prerequisites

Use Linux with systemd user services, Rust 1.88 or newer, Python 3, the exact Codex CLI/App Server pin in `config/codex-cli-pin` (currently 0.155.1), and Python `jsonschema` for protocol integration.

Read [Development](docs/development.md), [Architecture](docs/architecture/overview.md), and [Security](SECURITY.md) before changing public MCP behavior, authority, deployment, or service management.

## Validation

Run the repository gate:

```bash
./scripts/generate-app-server-tool-schemas.py --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo check --locked
cargo build --locked -p codex-connect
python3 tests/protocol_integration.py
```

The schema check must use the pinned Codex CLI. Do not loosen or regenerate the protocol contract for a different local release. For documentation-only changes, also verify Markdown links and search for retired terminology.

## Public-tool governance

The live catalog is exactly 14 intent-shaped tools. Prefer the official App Server semantic operation; add a Connect adapter or bridge only when the pinned App Server lacks the needed operation. New tools require distinct operator value, explicit safety annotations, compact schemas, and deterministic catalog/selection tests.

Preserve PTY support. Do not expose raw sandbox policy, service tier, developer instructions, approval policy, or other server-owned mechanics. Do not invoke Codex CLI through host commands when a `codex.*` semantic operation exists.

## Operating rules

- Keep Codex CLI/App Server and `tunnel-client` independently owned; do not add tunnel supervision.
- Keep `default_cwd` as navigation, never authorization.
- Keep worker tasks self-contained and preserve delegated scope ownership until terminal/action/interrupt/redirect. A `codex.wait` timeout is not permission to take over.
- Keep the event journal and console observational and event-driven; App Server remains authoritative. Do not reintroduce timer-based observer, transcript, or quota polling.
- Git actions are operator workflow, not application workflow.
- Remove superseded terminology and compatibility baggage end-to-end; this pre-release repository represents the current canonical implementation only.

For reporting, separate SourceChanged, Committed, Pushed, Deployed, Live, and CIGreen. Do not commit, push, deploy, or communicate externally as part of ordinary implementation without explicit authorization.
