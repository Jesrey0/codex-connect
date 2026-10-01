# Codex Connect

Codex Connect is a pre-release, single-user, self-hosted bridge from ChatGPT to a persistent Linux host and the official Codex CLI/App Server. The single-user boundary is a trust boundary, not a single-conversation limit: one backend may serve multiple independent ChatGPT operators for the same trusted user concurrently. ChatGPT can inspect host files, run deterministic commands, edit files, and delegate autonomous work or review to Codex.

**ChatGPT is the only supported action/control interface.** Human-visible local tooling is allowed for read-only observability. The `codex-connect console` exists to watch workers, transcripts, quota, and pending state without mutating anything; setup/service/deployment/diagnostic CLI commands remain trusted-host maintenance plumbing. No local UI may steer workers, answer approvals, mutate thread state, or otherwise bypass ChatGPT for actions.

> Codex Connect is an independent project and is not affiliated with or endorsed by OpenAI.

## The operating model

ChatGPT is the primary technical operator and orchestrator. Keep these planes distinct:

| Plane | Owner | Use it for |
| --- | --- | --- |
| HostPlane | ChatGPT through Codex Connect | Host filesystem, processes, Git, deployment, and deterministic commands |
| WorkerPlane | ChatGPT through Codex Connect + Codex App Server | Delegated `codex.*` work and read-only review |
| PlatformPlane | ChatGPT | Web, user/project files, Work/browser, plugins/apps, and Scheduled Tasks |

HostPlane is authoritative for host work. WorkerPlane is authoritative for delegated Codex lifecycle and cognition. PlatformPlane does not acquire host authority, and host or worker tools do not acquire connected-account authority. Workers do not inherit ChatGPT conversation, files, native tools, plugins, or scheduled tasks; delegated tasks must carry their own context and acceptance criteria.

The remote path is ChatGPT → ngrok HTTPS → host ingress with OAuth → loopback Codex Connect. Codex CLI/App Server and host ingress are independently managed. Connect owns its backend service, deployment state/cache, and installed artifacts.

The backend is shared and does not infer operator, conversation, session, or project ownership. HostPlane defaults to the service user's home directory; each fresh WorkerPlane workstream selects its own cwd explicitly. Retained command and worker state is backend-global, non-destructive recovery state distinguished by factual cwd and authoritative IDs.

The implementation is upstream-first: when the pinned Codex App Server exposes a primitive, App Server owns that feature and Connect adapts it. Connect must not invent a second thread, turn, review, command, model, usage, approval, permission, elicitation, or background-terminal state machine. Connect-native mechanics are reserved for host capabilities that do not have an equivalent App Server client primitive.

Tools advertise the `codex-connect:access` OAuth scope; host ingress validates tokens before forwarding to the backend.

## Authority and delegation

Deterministic host tools run with the OS account's authority. The managed backend uses the home directory as its default cwd; it is not an authorization boundary, and absolute host paths are valid. The dedicated App Server uses a process-local `danger-full-access` launch override for HostPlane.

Fresh `codex.start` work and review require an explicit project `cwd` and model. Resume and fork require the canonical thread model, inherit its cwd, and reject cwd changes. `codex.start(mode=work)` defaults to a writable workspace sandbox with network access; `access="full"` grants unrestricted host access. Approval prompts are disabled within the selected sandbox. Reviews are read-only. Workers use the normal Codex config and AGENTS.md instruction sources.

Workers own their delegated scope until completion, required action/input, interruption, or user redirect. Delegation is selective: keep routine work on the operator critical path, and use workers when autonomy, independent review, or genuine parallelism materially helps. Continue useful non-overlapping work after a start, then use `codex.wait` only when the worker result is actually needed. Do not poll with repeated waits. Timeout means the worker is still active; use `codex.inspect` for a non-blocking activity/history check when one is necessary. See [Operations](docs/operations.md#worker-lifecycle) for pending actions and lost-call recovery.

Do not invoke the Codex CLI through HostPlane commands when a `codex.*` semantic tool exists. Git actions are operator workflow, not application workflow.

## Public MCP surface

The MCP catalog contains 14 model-visible tools:

`status`, `inspect`, `view_image`, `apply_patch`, `command.exec`, `command.start`, `command.read`, `command.control`, `codex.start`, `codex.wait`, `codex.inspect`, `codex.query`, `codex.act`, and `workers.open`. A fifteenth descriptor, `workers.snapshot`, is app-only for explicit panel Refresh.

Open **Workers** in a supporting ChatGPT conversation to browse current/recent workers by project, inspect canonical results, attach selected context, or ask ChatGPT to inspect pending state. The panel is observational; worker actions stay in ChatGPT. Its initial snapshot comes from the opener, and updates require Refresh. See the [panel contract](docs/development.md#conversation-worker-panel).

Use `command.exec` for short HostPlane commands, `command.start/read/control` for persistent or interactive host processes, and `codex.start` for delegated work/review. `codex.start` can also fork persisted thread context into a new workstream. Use `codex.query` for Codex-owned discovery and persisted thread/process state, and `codex.act` for Codex-owned lifecycle mutations. `status` is backend-global and non-destructive; its `defaultCwd` is the service user's home, while retained workers and commands expose their own cwd. Read `status.workers` to recover a lost start response. The live schemas are authoritative for inputs, outputs, annotations, and limits.

`command.start` returns the first retained observation with its handle; stdin write through `command.control` also returns output in the same call. Continue reading from the returned output cursor. See the [tool ergonomics review](docs/tool-ergonomics.md) for the full catalog assessment.

## Install and operate

Start with [Getting Started](docs/getting-started.md) to install the pinned Codex CLI/App Server and backend, configure authenticated HTTPS ingress, and verify calls from ChatGPT. Read [Security](SECURITY.md) first.

See [Operations](docs/operations.md) for lifecycle, recovery, and deployment. Source, Git, deployed artifacts, live builds, ChatGPT plugin discovery, and CI must be verified separately.

For architecture, see [Architecture](docs/architecture/overview.md). For protocol governance and validation, see [Development](docs/development.md). Contributor rules are in [CONTRIBUTING.md](CONTRIBUTING.md); historical release notes are in [CHANGELOG.md](CHANGELOG.md).
