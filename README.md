# Codex Connect

Codex Connect connects ChatGPT to your host workspace and Codex CLI. Inspect files, run commands, edit code, and delegate autonomous engineering work to Codex through the official Codex App Server.

> **Pre-release:** Codex Connect is a single-user, self-hosted integration under active development. This repository represents only the current canonical implementation; abandoned development-era paths are not retained.

> **Unofficial project:** Codex Connect is an independent open-source project. It is not affiliated with, endorsed by, or distributed by OpenAI.

## Who this is for

Codex Connect is developer infrastructure for people who deliberately want a high-trust bridge from their own ChatGPT account to their own persistent development host. It is aimed at Linux workstations, development servers, homelabs, and similar single-user environments where the operator is comfortable reviewing source, running a local service, and managing developer credentials.

The managed setup currently targets **Linux with systemd user services**. It requires an authenticated Codex CLI, OpenAI Secure MCP Tunnel, and a ChatGPT account where Developer mode/custom MCP app creation is available. The repository builds from source; prebuilt packages are not currently distributed.

The default workspace and workspace-local installation layout use `~/projects`. Codex Connect keeps its backend on loopback and does not expose port `8767` directly to the internet.

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
command.read ↔ command.control
               ├─ write
               ├─ resize   (PTY only)
               └─ terminate
```

For autonomous Codex work:

```text
codex.start(mode=work|review)
        ↓
codex.wait
        ↓
[pending approval / permissions / userInput, if any]
        ↓
codex.action.respond
        ↓
codex.wait
```

Use `codex.inspect` when you need to see what a delegated worker has been doing without changing synchronization state. Its default semantic view is compact and cursor-based; `detail=raw` is an explicit forensic escape hatch for original App Server notifications. `codex.wait` is deliberately synchronization-only and never returns the raw event journal.

Use `inspect` for structured read-only host exploration, and batch independent inspection operations in one call. Use `command.exec` when the answer is naturally produced by one bounded deterministic host command; compose related repository/tool reads into that single command rather than chaining tiny calls. Use `command.start` when the host command is still deterministic but must remain running or interactive. Use `codex.start(mode=work)` only when delegated autonomous reasoning or iteration materially improves the critical path or quality; delegation is an optimization, not the default.

**Control-plane invariant:** ChatGPT operates the host and Codex through Codex Connect. When a Codex semantic operation is available through `codex.*`, host command tools must not be used to invoke the Codex CLI as an alternate control plane. Codex workers are isolated from the calling ChatGPT conversation: every delegated work task must include its own relevant context, constraints, paths, decisions, and acceptance criteria. Prefer batching and deterministic local work over unnecessary delegated turns because model usage is a constrained resource. Delegation transfers ownership of the assigned scope until the worker becomes terminal, blocks for operator action, or is explicitly interrupted. ChatGPT may continue non-overlapping critical-path work while the worker runs, but must not redo the delegated task merely because a `codex.wait` lease expires. A wait timeout means the worker is still active. Continue useful non-overlapping work when available; if a legitimately long-running worker owns the remaining critical path and there is nothing useful left to parallelize, prefer a ChatGPT Scheduled Task/monitoring handoff when that feature is available and can access the connector instead of spinning repeated waits. The scheduled run must re-establish authoritative state through Codex Connect and does not inherit worker ownership or HostPlane authority. Host-plane tool responses opportunistically surface compact `workerEvents` only when delegated work becomes terminal or requires operator action. `codex.wait` owns interactive synchronization and `codex.inspect` owns worker observation/forensics.

**Delegation non-regression invariant:** `codex.wait` is a bounded observation lease, never an ownership deadline. `WaitTimeout != WorkerFailure`, `WaitTimeout != WorkerStall`, and `WaitTimeout != PermissionToTakeOver`. A delegated scope has exactly one active owner: the worker until terminal/action-blocked/explicit-interrupt/user-redirect. The operator may parallelize only non-overlapping work.

**Authority-plane invariant:** deterministic host tools are the primary HostPlane. Codex Connect launches its dedicated App Server with a process-local `sandbox_mode="danger-full-access"` override, and `command.exec` / `command.start` expose no public sandbox knob. The configured cwd is only the base for relative paths; absolute host paths are valid and OS permissions remain authoritative. Delegated `codex.start(mode=work)` is a separate WorkerPlane with an intent-shaped authority selector: omitted `access` means the canonical writable workspace sandbox with network access, while `access="full"` explicitly selects `dangerFullAccess`. Codex Connect fixes delegated work to `approvalPolicy="never"` so mechanical approvals do not stall interactive or Scheduled Task coordination; that policy does not widen the selected sandbox. Low-level sandbox fields and per-turn service tier are server-owned rather than public MCP knobs. Worker behavior is owned by upstream Codex configuration plus the normal AGENTS.md instruction chain; Connect deliberately omits a thread-level developer-instruction override so it cannot shadow those sources. ChatGPT-native capabilities form a third PlatformPlane: web/research, user/project files, Scheduled Tasks, Work/browser, and installed plugins/apps should be used when they own the relevant external information or action, but they do not gain host filesystem/process authority from Codex Connect and HostPlane does not gain connected-account authority from them. Workers do not inherit ChatGPT-native tools, app/plugin connections, files, conversation state, or task scheduling unless relevant content is explicitly supplied. New official review threads are always started read-only and may select a model.

**Timeout ownership invariant:** Codex Connect timeouts describe operation semantics; they do not duplicate tunnel transport deadlines. Tunnel-client independently enforces any per-command response deadline supplied by the control plane. The ChatGPT-facing synchronous path targets completion within 40 seconds and Codex Connect guards each public MCP call at 45 seconds. `codex.wait` remains an operator-facing 20-second quiet join with a 30-second maximum, and `command.read` remains an operator-facing output/exit lease with a 20-second default and 40-second maximum because those values affect coordination behavior. By contrast, `command.exec` uses server-owned execution limits: a 30-second child timeout, a 64 KiB per-stream output cap, and a 5-second App Server response allowance; those mechanics are intentionally absent from the public MCP input schema. Persistent, interactive, or longer-running commands use `command.start` plus bounded `command.read`, including PTY sessions. A per-command `cwd` selects only the child process working directory; it is not a security boundary. Conservative truncation flags report when buffered output reaches the server-owned cap because the pinned App Server exposes no definitive truncation flag. Keep reading a persistent command with its cursor after terminal state until `drained:true` when retained final output matters; `historyLost:true` means earlier output is unrecoverable. `command.control(action=terminate)` is a stop request, not a graceful-cleanup guarantee.

## Public MCP surface

The canonical MCP surface contains 14 tools:

| Area | Tools |
| --- | --- |
| Host orientation | `status` (readiness, build identity, navigation cwd, and worker-default provenance) |
| Read-only host inspection | `inspect`, `view_image` |
| Deterministic mutation/execution | `apply_patch`, `command.exec`, `command.start`, `command.read`, `command.control` |
| Codex work/review | `codex.start`, `codex.wait`, `codex.inspect`, `codex.control` |
| Codex collaboration | `codex.action.respond` |
| Codex discovery/account | `codex.info` |

`codex.*` is the public namespace for interacting with the Codex CLI/App Server agent domain. `codex-connect` and `@codexConnect` remain the implementation and connector/product identities of the bridge; host/operator tools remain un-namespaced or under `command.*`.

The App Server is initialized with `experimentalApi: true` and the canonical `openai/form` client extension. Form/URL elicitation is resolved through the existing `codex.action.respond` collaboration tool rather than a separate public responder. The dedicated App Server process also enables the pinned `default_mode_request_user_input`, `request_permissions_tool`, and `exec_permission_approvals` feature flags so the public collaboration paths can be exercised without relying on a user's global Codex configuration.

The configured default workspace is a navigation base, not an authorization boundary, and it is not implicitly a Git repository. Version control is optional. Codex Connect and its agents must not initialize repositories, create branches, commits, or tags, or use Git as a workflow/checkpoint mechanism unless the user explicitly requests version-control work.

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
codex-connect console
codex-connect restart
codex-connect doctor
codex-connect logs --follow
codex-connect probe --codex-bin "$(command -v codex)" --cwd ~/projects/example-project
codex-connect uninstall
```

`codex-connect console` opens the local read-only observability console backed by the running Codex Connect backend. It is intentionally human-oriented rather than a tool-event trace: the dashboard shows quota, active and recent workers, delegated-task previews, model/reasoning metadata, operator-action requests, and genuine observer errors. Use ↑/↓ (or `j`/`k`) to select a worker and Enter/→ to open it. The worker view foregrounds the delegated task, user/agent conversation, live agent output, and an animated `THINKING` state without exposing reasoning text. Command execution, file-change, search, and MCP-tool details are omitted from the normal transcript; use `codex.inspect` when forensic activity detail is actually needed. Pending permissions, user input, and elicitations are rendered as read-only action cards with their relevant context; resolution still happens through the ChatGPT/Codex Connect operator path. In a worker view, `g` jumps to the start, `G` resumes live tail-following, ↑/↓ scroll, and Esc/←/`b` returns to the dashboard.

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
