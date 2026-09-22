# Codex Connect

Codex Connect is a pre-release, single-user, self-hosted bridge from ChatGPT to a persistent Linux host and the official Codex CLI/App Server. ChatGPT can inspect host files, run deterministic commands, edit files, and delegate autonomous work or review to Codex.

> Codex Connect is an independent project and is not affiliated with or endorsed by OpenAI.

## The operating model

ChatGPT is the primary technical operator and orchestrator. Keep these planes distinct:

| Plane | Owner | Use it for |
| --- | --- | --- |
| HostPlane | Codex Connect | Host filesystem, processes, Git, deployment, and deterministic commands |
| WorkerPlane | Codex Connect + Codex App Server | Delegated `codex.*` work and read-only review |
| PlatformPlane | ChatGPT | Web, user/project files, Work/browser, plugins/apps, and Scheduled Tasks |

HostPlane is authoritative for host work. WorkerPlane is authoritative for delegated Codex lifecycle and cognition. PlatformPlane does not acquire host authority, and host or worker tools do not acquire connected-account authority. Workers do not inherit ChatGPT conversation, files, native tools, plugins, or scheduled tasks; delegated tasks must carry their own context and acceptance criteria.

The canonical remote path is ChatGPT → ngrok HTTPS → host ingress with OAuth → loopback Codex Connect. Codex CLI/App Server and host ingress have independent user-global lifecycles. Codex Connect owns its backend, configuration/state/cache, and installed artifacts. OpenAI Secure MCP Tunnel may be retained only as an independently managed secondary fallback; it is not part of normal setup, health, deployment, or timeout semantics.

Every public tool advertises the `codex-connect:access` OAuth requirement in its descriptor metadata. Host ingress remains the sole token-validation authority: it verifies the Bearer token and strips credentials before proxying the request, so Codex Connect does not duplicate OAuth state or token parsing.

## Authority and delegation

Deterministic host tools run with the OS account's authority. The configured `default_cwd` is navigation only, not an authorization boundary; absolute host paths are valid. The dedicated App Server uses a process-local `danger-full-access` launch override for HostPlane.

`codex.start(mode=work)` is WorkerPlane. Omitted `access` selects the canonical writable workspace sandbox with network access; `access="full"` selects `danger-full-access`. Work turns use `approvalPolicy="never"`, which removes mechanical approval stalls without widening the selected sandbox. Reviews are read-only. Connect does not send `thread/start.developerInstructions`: worker cognition comes from `~/.codex/config.toml` `developer_instructions`, `~/.codex/AGENTS.md`, repository/directory `AGENTS.md`, and then the delegated task. Connect owns authority and lifecycle.

Delegated scope ownership persists until terminal, semantic block/action, interrupt, or user redirect. `codex.wait` is a bounded synchronization lease driven by App Server lifecycle notifications; reads hydrate pre-existing state, reconcile explicit history loss, or perform one final lease-expiry check. The ChatGPT-facing default is 80 seconds, leaving finalization margin below the observed ~100-second tool-runner ceiling; the server hard maximum remains five minutes for other callers and explicit experiments. Client response deadlines remain independent. A timeout is not failure, a stall diagnosis, or permission to take over. A dropped `codex.start` caller also does not cancel upstream worker creation: Connect owns start completion and can recover the handle through a one-shot `workerStarted` host event. Use `codex.inspect` for activity/history; the human `console` is a read-only event-driven projection, not an event trace. Its local UI clock never polls App Server state. PTY support remains available through `command.start/read/control`.

Do not invoke the Codex CLI through HostPlane commands when a `codex.*` semantic tool exists. Git actions are operator workflow, not application workflow.

## Public MCP surface

The live public catalog contains exactly 14 tools:

`status`, `inspect`, `view_image`, `apply_patch`, `command.exec`, `command.start`, `command.read`, `command.control`, `codex.start`, `codex.wait`, `codex.inspect`, `codex.control`, `codex.action.respond`, and `codex.info`.

The intent-shaped inputs are deliberately compact:

- `command.exec`: required `command`; optional `cwd` and `env`. The server-owned child timeout is 60 seconds with response allowance below the frontend ceiling; longer jobs belong on `command.start`.
- `command.start/read/control`: persistent command lifecycle with stdin, PTY resize, termination, cursors, and event-driven reads; `command.read` defaults to 60 seconds and permits 80 seconds.
- `codex.start(mode=work)`: `task`, optional `cwd`, `threadId`, `model`, `effort`, and `access`.
- `codex.start(mode=review)`: review `target`, optional `cwd`, `threadId`, and `model`; reviews are read-only.

`serviceTier`, developer instructions, raw sandbox policy, approval policy, and other low-level controls are server-owned/hidden. Model, skill, and usage discovery is batched through `codex.info`.

## Install and operate

Start with [Getting Started](docs/getting-started.md). It installs the pinned Codex CLI/App Server `0.155.1` and Codex Connect, configures authenticated ngrok HTTPS through host ingress, and proves the complete ChatGPT path. Read [Security](SECURITY.md) first: this is a high-trust loopback host bridge whose public authentication is enforced at ingress.

For steady-state lifecycle and recovery, see [Operations](docs/operations.md). Backend deployment and connector refresh are separate: `SourceChanged != Committed != Pushed != Deployed != Live != CIGreen`. Verify each state at its owner. Backend deployment never refreshes the ChatGPT connector or supervises ingress.

For architecture, see [Architecture](docs/architecture/overview.md). For protocol governance and validation, see [Development](docs/development.md). Contributor rules are in [CONTRIBUTING.md](CONTRIBUTING.md); historical release notes are in [CHANGELOG.md](CHANGELOG.md).
