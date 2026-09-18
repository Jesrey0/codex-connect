# Contributing

Codex Connect is a deliberately narrow bridge between ChatGPT, the official OpenAI Secure MCP Tunnel, a single durable host scope, and the official Codex App Server API. Contributions should preserve those ownership boundaries rather than add parallel lifecycle managers or alternate protocol paths.

## Development prerequisites

Use Linux with a working Rust toolchain (Rust 1.85 or newer), Python 3, and the exact Codex CLI release in `config/codex-cli-pin`. The protocol integration suite also requires the Python `jsonschema` package.

For architecture and protocol invariants, read [docs/development.md](docs/development.md) and [docs/architecture/overview.md](docs/architecture/overview.md) before changing public MCP behavior, sandbox semantics, App Server integration, deployment, or service management.

## Validation

Run the complete repository gate before submitting a change:

```bash
./scripts/generate-app-server-tool-schemas.py --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo check --locked
cargo build --locked -p codex-connect
python3 tests/protocol_integration.py
```

The schema check intentionally fails if your installed Codex CLI does not match `config/codex-cli-pin`. Do not regenerate or loosen the protocol contract merely to accommodate a different local Codex release.

## Scope of changes

- Prefer official App Server semantics over Connect-owned reimplementations.
- Keep the public MCP catalog small, goal-oriented, explicitly annotated, and covered by deterministic selection tests.
- Preserve the loopback-only backend and single-user self-hosted trust model.
- Do not add tunnel supervision, tunnel credentials, active-project state, hidden profiles, or abandoned historical compatibility paths.
- Do not introduce Git branches, commits, tags, or repository initialization as application workflow; version control is an operator concern.
- Keep required dependencies and compilation cost proportionate to the capability being added.

## Issues and security reports

Normal bugs and feature requests can use GitHub Issues. Security vulnerabilities must follow [SECURITY.md](SECURITY.md); do not include secrets, exploit details, tunnel identifiers, account identifiers, or sensitive local paths in a public issue.
