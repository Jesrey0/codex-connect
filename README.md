# Codex Connect

Codex Connect connects ChatGPT to your host workspace and Codex CLI. Inspect files, run commands, edit code, and delegate autonomous engineering work to Codex through the official Codex App Server.

> **Pre-release:** Codex Connect is a single-user, self-hosted integration under active development. This repository represents only the current canonical implementation; abandoned development-era paths are not retained.

> **Unofficial project:** Codex Connect is an independent open-source project. It is not affiliated with, endorsed by, or distributed by OpenAI.

## Who this is for

Codex Connect is developer infrastructure for people who deliberately want a high-trust bridge from their own ChatGPT account to their own persistent development host. It is aimed at Linux workstations, development servers, homelabs, and similar single-user environments where the operator is comfortable reviewing source, running a local service, and managing developer credentials.

The managed setup currently targets **Linux with systemd user services**. It requires an authenticated Codex CLI, OpenAI Secure MCP Tunnel, and a ChatGPT account where Developer mode/custom MCP app creation is available. The repository builds from source; prebuilt packages are not currently distributed.

The default host scope and workspace-local installation layout use `~/projects`. Codex Connect keeps its backend on loopback and does not expose port `8767` directly to the internet.

### Dependency ownership boundary

Codex Connect is downstream of two independently managed upstream dependencies. Their installation and owned state are deliberately **not** part of the Codex Connect workspace layout:

| Component | Ownership | Canonical location model |
| --- | --- | --- |
| Codex CLI / App Server | User-global, Codex-owned | Install outside `~/projects`; Codex state/config remains under the normal `~/.codex` home unless the user explicitly changes it. |
| OpenAI `tunnel-client` | User-global, tunnel-client-owned | Install outside `~/projects`; profiles and native runtime state use user-global XDG locations such as `~/.config/tunnel-client` and `~/.local/state/tunnel-client`. |
| Codex Connect | Workspace-local, Codex Connect-owned | Source under the workspace; managed config/state/artifacts live under `~/projects/.config/codex-connect` and `~/projects/.local/...`. |

**Ownership invariant:** Codex Connect may call or reference Codex CLI/App Server and Secure MCP Tunnel, but it must not install, relocate, duplicate, upgrade, delete, or supervise either upstream dependency or its owned configuration/state.

## Security model

This is a **high-trust host execution bridge**, not a read-only data connector. Depending on the selected sandbox and permissions, ChatGPT can cause file mutations, command execution, persistent processes, and autonomous Codex work on the host. Read [SECURITY.md](SECURITY.md) before installation and do not run the backend under an OS account whose privileges exceed what you intend ChatGPT to exercise.

```text
ChatGPT → official OpenAI tunnel-client → Codex Connect → Codex App Server
```

Codex App Server remains authoritative for Codex threads, turns, reviews, approvals, models, and execution semantics. Codex Connect does not mirror that RPC vocabulary into MCP. Instead it composes a deliberately selected App Server protocol subset into a small, goal-oriented tool surface while preserving official thread IDs, turn IDs, request IDs, and raw event traceability.

**Reuse invariant:** before adding a Connect capability, inspect the generated schema from the pinned Codex CLI. If App Server already owns the semantic operation, Connect projects or adapts it; Connect-only bridges exist only for semantics the pin does not provide.

## Operator mental model

For deterministic host work:

```text
inspect → command.exec / command.start / apply_patch
```

For persistent or interactive deterministic commands:

```text
command.start
    ↓
command.read ↔ command.write
       ↘ command.resize   (PTY only)
        ↘ command.terminate
```

For autonomous Codex work:

```text
codex.work.start
        ↓
codex.work.wait
        ↓
[pending typed action, if any]
        ↓
codex.approval.respond / codex.permissions.respond /
codex.elicitation.respond / codex.userInput.respond
        ↓
codex.work.wait
```

Use `inspect` for structured read-only host exploration, and batch independent inspection operations in one call. Use `command.exec` when the answer is naturally produced by one bounded deterministic host command; compose related repository/tool reads into that single command rather than chaining tiny calls. Use `command.start` when the host command is still deterministic but must remain running or interactive. Use `codex.work.start` when the Codex CLI/agent should investigate, reason, edit, test, or iterate autonomously.

**Host sandbox invariant:** when `command.exec` or `command.start` omits `sandboxPolicy`, Codex Connect sends **no synthetic policy**. The field remains absent on the App Server request, so App Server uses the effective upstream Codex configuration from `$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`). Codex Connect must not mirror those defaults locally. Callers may still supply an explicit per-command override, including `dangerFullAccess`. By contrast, `codex.work.start` requires an explicit `sandboxPolicy` on every delegated turn so operator-selected authority is never inherited implicitly.

**Command edge semantics:** `workspaceWrite.writableRoots` adds writable roots; it is not an exclusive allowlist and does not narrow App Server's base workspace. Codex Connect launches its dedicated App Server with `scopeRoot` as the working directory, so host `command.exec` / `command.start` with `workspaceWrite` leave `scopeRoot` writable even when `writableRoots` is empty. A per-command `cwd` only selects the process working directory; it does **not** confine write authority to that directory. `command.exec.timeoutMs` is a process deadline rather than an end-to-end MCP latency ceiling, and a buffered stdout/stderr stream exactly equal to `outputBytesCap` may have been truncated because the pinned App Server response has no truncation flag. For persistent commands, keep calling `command.read` with the returned cursor even after `state:"exited"`/`"failed"` until the cursor stops advancing and both output streams are empty; honor `historyLost:true` as unrecoverable earlier output. `command.terminate` is a stop request, not a graceful-cleanup guarantee. Relative scoped paths reject `..` components lexically; use an absolute in-scope path when intentionally reaching outside a request-local `cwd`.

## Public MCP surface

The canonical MCP surface contains 24 tools:

| Area | Tools |
| --- | --- |
| Host orientation | `status` (health, build identity, global Codex config provenance, App Server launch context) |
| Read-only host inspection | `inspect`, `view_image` |
| Deterministic mutation/execution | `apply_patch`, `command.exec`, `command.start`, `command.read`, `command.write`, `command.resize`, `command.terminate` |
| Codex work | `codex.work.start`, `codex.work.read`, `codex.work.wait`, `codex.work.steer`, `codex.work.interrupt` |
| Codex operator/action loop | `codex.pendingActions.list`, `codex.approval.respond`, `codex.permissions.respond`, `codex.elicitation.respond`, `codex.userInput.respond` |
| Codex review/discovery/usage | `codex.review`, `codex.model.list`, `codex.skills.list`, `codex.usage` |

`codex.*` is the public namespace for interacting with the Codex CLI/App Server agent domain. `codex-connect` and `@codexConnect` remain the implementation and connector/product identities of the bridge; host/operator tools remain un-namespaced or under `command.*`.

The App Server is initialized with `experimentalApi: true` and advertises `extensions["openai/form"]` for the official structured collaboration paths Codex Connect exposes. Its dedicated App Server process also enables the pinned `default_mode_request_user_input`, `request_permissions_tool`, and `exec_permission_approvals` feature flags so those catalog responders can be exercised without relying on a user's global Codex configuration.

The configured host scope is a general filesystem workspace, not implicitly a Git repository. Version control is optional. Codex Connect and its agents must not initialize repositories, create branches, commits, or tags, or use Git as a workflow/checkpoint mechanism unless the user explicitly requests version-control work.

## Quick start

For a fresh machine, follow **[Getting Started](docs/getting-started.md)**.

If prerequisites are already installed:

```bash
bootstrap_target=target/codex-connect-bootstrap
rm -rf "$bootstrap_target"
CARGO_TARGET_DIR="$bootstrap_target" cargo build --release -p codex-connect --locked
"$bootstrap_target/release/codex-connect" setup
rm -rf "$bootstrap_target"
export PATH="$HOME/projects/.local/bin:$PATH"
codex-connect doctor
codex-connect status
```

`setup` installs the bootstrap binary into the workspace-local content-addressed build store under `~/projects/.local/lib/codex-connect/builds/`, points `~/projects/.local/bin/codex-connect` at that build, keeps configuration/state under `~/projects/.config` and `~/projects/.local/state`, installs the user service, and verifies backend health. The one-time `target/codex-connect-bootstrap/` build directory is disposable and should be removed after setup; it is not a runtime installation. After source changes, use the explicit deployment workflow: `codex-connect deploy prepare`, poll `codex-connect deploy status <operation-id>` until it reports `prepared`, then run `codex-connect deploy activate <operation-id>` and verify the same operation after reconnect. Deployment builds use the persistent `target/codex-connect-deploy/build/` compiler cache, but the content-addressed workspace-local build store remains the only runtime artifact authority. Both the potentially long release build and the disruptive backend activation run as detached systemd jobs, so foreground operator commands remain short and deterministic. The tunnel runtime remains untouched.

Configure the user-global official tunnel client separately to connect its long-lived runtime to `http://127.0.0.1:8767/mcp`. Its binary, profiles, credentials, and native runtime state remain outside `~/projects`. The ChatGPT custom app/connector uses **no authentication**. The tunnel runtime owns its OpenAI control-plane credential and organization context. Codex Connect accepts MCP only on loopback, has no application-level authentication, and manages only its own backend service.

Routine backend commands are:

```bash
codex-connect status
codex-connect restart
codex-connect doctor
codex-connect logs --follow
codex-connect probe --codex-bin "$(command -v codex)" --cwd ~/projects/example-project
codex-connect uninstall
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
- [Changelog](CHANGELOG.md)

The backend is a high-trust host execution bridge. Read [SECURITY.md](SECURITY.md) before exposing or operating it outside its intended single-user environment.
