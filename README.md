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

The remote path is ChatGPT → ngrok HTTPS → host ingress with OAuth → loopback Codex Connect. Codex CLI/App Server and host ingress are independently managed. Connect owns its backend service, deployment state/cache, and installed artifacts.

Tools advertise the `codex-connect:access` OAuth scope; host ingress validates tokens before forwarding to the backend.

## Authority and delegation

Deterministic host tools run with the OS account's authority. The managed backend uses the home directory as its navigation cwd; it is not an authorization boundary, and absolute host paths are valid. The dedicated App Server uses a process-local `danger-full-access` launch override for HostPlane.

`codex.start(mode=work)` defaults to a writable workspace sandbox with network access; `access="full"` grants unrestricted host access. Approval prompts are disabled within the selected sandbox. Reviews are read-only. Workers use the normal Codex config and AGENTS.md instruction sources.

Workers own their delegated scope until completion, required action/input, interruption, or user redirect. Continue only non-overlapping work, then use `codex.wait` to join. Timeout means the worker is still active; use `codex.inspect` for activity/history. See [Operations](docs/operations.md#worker-lifecycle) for pending actions and lost-call recovery.

Do not invoke the Codex CLI through HostPlane commands when a `codex.*` semantic tool exists. Git actions are operator workflow, not application workflow.

## Public MCP surface

The live public catalog contains exactly 14 tools:

`status`, `inspect`, `view_image`, `apply_patch`, `command.exec`, `command.start`, `command.read`, `command.control`, `codex.start`, `codex.wait`, `codex.inspect`, `codex.control`, `codex.action.respond`, and `codex.info`.

Use `command.exec` for short commands, `command.start/read/control` for persistent or interactive processes, and `codex.start` for delegated work/review. `status` is also the recovery anchor for retained persistent-command handles. Discover compact model, skill, and usage choices with `codex.info`. The live schemas define inputs and limits.

## Install and operate

Start with [Getting Started](docs/getting-started.md) to install the pinned Codex CLI/App Server and backend, configure authenticated HTTPS ingress, and verify calls from ChatGPT. Read [Security](SECURITY.md) first.

See [Operations](docs/operations.md) for lifecycle, recovery, and deployment. Source, Git, deployed artifacts, live builds, connector discovery, and CI must be verified separately.

For architecture, see [Architecture](docs/architecture/overview.md). For protocol governance and validation, see [Development](docs/development.md). Contributor rules are in [CONTRIBUTING.md](CONTRIBUTING.md); historical release notes are in [CHANGELOG.md](CHANGELOG.md).
