# Codex Connect

Codex Connect is a local MCP backend designed for **ChatGPT as the primary operator** of the official Codex App Server.

> **Pre-release:** Codex Connect is a single-user, self-hosted backend under active development. This repository represents only the current canonical implementation; abandoned development-era paths are not retained.

```text
ChatGPT → official OpenAI tunnel-client → Codex Connect → Codex App Server
```

Codex App Server remains authoritative for Codex threads, turns, reviews, approvals, models, and execution semantics. Codex Connect does not mirror that RPC vocabulary into MCP. Instead it composes a deliberately selected App Server protocol subset into a small, goal-oriented tool surface while preserving official thread IDs, turn IDs, request IDs, and raw event traceability.

## Operator mental model

For deterministic host work:

```text
codexConnect.inspect → command.exec / apply_patch
```

For autonomous Codex work:

```text
codexConnect.work.start
        ↓
codexConnect.work.wait
        ↓
[pending typed action, if any]
        ↓
codexConnect.approval.respond / permissions.respond / elicitation.respond
        ↓
codexConnect.work.wait
```

Use `command.exec` when the exact command is already known. Use `codexConnect.work.start` when Codex should investigate, reason, edit, test, or iterate autonomously.

## Public MCP surface

The canonical MCP surface contains 19 tools:

| Area | Tools |
| --- | --- |
| Orientation | `codexConnect.status`, `codexConnect.usage` |
| Read-only host inspection | `codexConnect.inspect`, `view_image` |
| Deterministic mutation/execution | `apply_patch`, `command.exec` |
| Autonomous Codex work | `codexConnect.work.start`, `.read`, `.wait`, `.steer`, `.interrupt` |
| Operator/action loop | `codexConnect.pendingActions.list`, `codexConnect.approval.respond`, `codexConnect.permissions.respond`, `codexConnect.elicitation.respond`, `codexConnect.userInput.respond` |
| Review/discovery | `codexConnect.review`, `model.list`, `skills.list` |

The App Server is initialized with `experimentalApi: true` and advertises `extensions["openai/form"]` for the official structured collaboration paths Codex Connect exposes. Its dedicated App Server process also enables the pinned `default_mode_request_user_input`, `request_permissions_tool`, and `exec_permission_approvals` feature flags so those catalog responders can be exercised without relying on a user's global Codex configuration.

The configured host scope is a general filesystem workspace, not implicitly a Git repository. Version control is optional. Codex Connect and its agents must not initialize repositories, create branches, commits, or tags, or use Git as a workflow/checkpoint mechanism unless the user explicitly requests version-control work.

## Quick start

For a fresh machine, follow **[Getting Started](docs/getting-started.md)**.

If prerequisites are already installed:

```bash
cargo build --release -p codex-connect --locked
target/release/codex-connect setup
export PATH="$HOME/.local/bin:$PATH"
codex-connect doctor
codex-connect status
```

`setup` installs the binary into the content-addressed build store under `~/.local/lib/codex-connect/builds/`, points `~/.local/bin/codex-connect` at that build, installs the user service, and verifies backend health. After source changes, use `codex-connect deploy`; it builds and atomically activates the new artifact, removes older installed backend builds, and leaves the tunnel runtime untouched.

Configure the official tunnel client separately to connect its long-lived runtime to `http://127.0.0.1:8767/mcp`. The ChatGPT custom app/connector uses **no authentication**. The tunnel runtime owns its OpenAI control-plane credential and organization context. Codex Connect accepts MCP only on loopback, has no application-level authentication, and manages only its own backend service.

Routine backend commands are:

```bash
codex-connect status
codex-connect restart
codex-connect doctor
codex-connect logs --follow
codex-connect probe --codex-bin ~/.local/bin/codex --cwd ~/projects/example-project
```

After a computer restart, verify the backend first, then inspect/resume the existing tunnel runtime with `tunnel-client runtimes status codex-connect --json`. See [Operations](docs/operations.md#after-a-computer-restart).

## Documentation

- [Getting Started](docs/getting-started.md)
- [Documentation index](docs/README.md)
- [Architecture](docs/architecture/overview.md)
- [Operations](docs/operations.md)
- [Development](docs/development.md)
- [Security](SECURITY.md)
- [Contributing](CONTRIBUTING.md)

The backend is a high-trust host execution bridge. Read [SECURITY.md](SECURITY.md) before exposing or operating it outside its intended single-user environment.
