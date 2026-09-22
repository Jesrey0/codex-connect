# Architecture

Codex Connect is a compact MCP projection over a persistent host and the official Codex App Server. ChatGPT is the primary technical operator; Codex Connect owns host authority and delegated lifecycle, while App Server owns Codex threads, turns, reviews, requests, and execution semantics.

## Three planes

```text
ChatGPT / PlatformPlane
        │ HTTPS / ngrok
        ▼
Host ingress (Caddy routing + OAuth)
        │ loopback MCP
        ▼
Codex Connect
  ├─ HostPlane: inspect, patch, image, commands, status, deployment
  └─ WorkerPlane: codex.* → official Codex App Server
```

PlatformPlane is ChatGPT-native web/files/plugins/apps/Work/browser/Scheduled Tasks. It has no implicit host access. HostPlane is deterministic and authoritative for the OS account's filesystem, processes, Git, and deployment. WorkerPlane is delegated autonomous work/review; workers do not inherit ChatGPT conversation, native tools, files, or credentials.

Host ingress owns the canonical public URL, TLS edge configuration, routing, and OAuth boundary. Codex CLI/App Server and host ingress are independently managed dependencies; their binaries, credentials, and state are not Codex Connect-owned.

OpenAI Secure MCP Tunnel is outside the canonical architecture. An operator may retain it as a separately managed secondary fallback, but its process, profile, credentials, reconnect behavior, and response deadlines are not Codex Connect state or design invariants. Normal installation, readiness, deployment, recovery, and performance expectations are defined against host ingress.

## Authority and configuration

Host paths are resolved from an optional `cwd` or the configured navigation `default_cwd`; absolute paths are accepted. Neither is an authorization boundary. HostPlane uses the OS account's authority, and the dedicated App Server is launched with the process-local `sandbox_mode="danger-full-access"` override.

For `codex.start(mode=work)`, omitted `access` means the canonical writable workspace sandbox with network access; `access="full"` means `danger-full-access`. Connect sends `approvalPolicy="never"` for work turns. That prevents mechanical approval stalls but does not enlarge the sandbox. Reviews are read-only.

Connect does not send `thread/start.developerInstructions`. Worker cognition is ordered by upstream `~/.codex/config.toml` `developer_instructions`, `~/.codex/AGENTS.md`, repository/directory `AGENTS.md`, and the delegated task. Connect owns authority, lifecycle, and operator orchestration; it does not add a competing instruction source.

## App Server reuse and state

App Server is the authority for official IDs and lifecycle. Connect adapts its methods rather than exposing a second session model:

- `codex.start` composes thread/turn work or read-only review.
- `codex.wait` synchronizes one delegated turn and returns terminal/action-required/input-required state.
- `codex.inspect` projects bounded semantic activity, or raw retained notifications when explicitly requested.
- `codex.control` steers or interrupts; `codex.action.respond` answers approvals, permissions, user input, and elicitation.
- `command.start/read/control` preserve the official streaming command lifecycle, including PTY stdin, resize, and termination.

Connect-owned event journals, reducers, pending-request registries, and observer caches are bounded projections, not authoritative Codex state. The human `codex-connect console` is read-only and human-oriented; it hydrates once and then advances through cursor-based observer waits driven by App Server events. Message deltas update the live projection directly; completed human-visible messages advance a transcript revision that triggers one authoritative transcript rehydrate. Quota refresh is asynchronous, single-flight/coalesced, and cannot fail the worker projection. The local UI clock performs no observer or quota reads. Use `codex.inspect` for forensic activity.

Before adding a capability, inspect the generated schema for pinned Codex CLI/App Server `0.155.1`. Use an official semantic method when one exists; keep a Connect bridge only when the pin has no equivalent (for example content search or deterministic patch semantics).

## Delegation ownership

```text
codex.start → codex.wait ─┬─ terminal
                         ├─ pending action/input → codex.action.respond
                         └─ lease expiry → codex.inspect or another bounded wait
```

The worker owns its delegated scope from start submission until terminal, semantic block/action, interrupt, or user redirect. Start completion itself is relay-owned: if the initiating MCP caller disappears, Connect continues thread/resume/start and turn/review start, registers the resulting worker, and retains an unclaimed handle as a one-shot `workerStarted` host event. App Server lifecycle notifications drive live worker state. `codex.wait` performs no periodic status read: it hydrates a turn that predates the current relay, resumes that thread's official notification subscription when needed, reconciles explicit history loss, and performs one final authoritative check when the lease expires. The ChatGPT-facing default lease is 80 seconds, calibrated below the observed ~100-second tool-runner ceiling; the public hard maximum remains five minutes for other callers/explicit experiments. Client response deadlines remain independent. A `codex.wait` timeout is only lease expiry: it is not worker failure, stall evidence, or permission to take over. The operator may continue non-overlapping work.

The general runtime invariant is: **events drive live state; reads hydrate or reconcile state; deadlines bound operations; retries recover unavailable event sources or transient persistence races.** No periodic App Server read may exist merely to observe progress. Ingress availability is verified independently from backend readiness.

Timeout policy follows operation class rather than one global number. Quick control/inspection paths use a 45-second local guard. `codex.start` has a 90-second caller guard but durable relay-owned completion. `command.exec` uses a 60-second child timeout with a 75-second MCP guard; longer deterministic jobs move to `command.start`. `command.read` is an event-driven 60-second default / 80-second maximum lease with five seconds of guard headroom. `codex.wait` is an event-driven 80-second default with 15 seconds of guard headroom and a 300-second server maximum. Ordinary App Server RPCs stay at 30 seconds because local RPC failure detection is independent from client-response deadlines.

## Public surface

The canonical live catalog has 14 tools: `status`, `inspect`, `view_image`, `apply_patch`, `command.exec`, `command.start`, `command.read`, `command.control`, `codex.start`, `codex.wait`, `codex.inspect`, `codex.control`, `codex.action.respond`, and `codex.info`.

The public contract is intent-shaped. `command.exec` accepts only `command`, optional `cwd`, and optional `env`; its timeout/output limits are server-owned. `codex.start` work accepts `task`, optional `cwd`, `threadId`, `model`, `effort`, and `access`; review accepts its target plus optional `cwd`, `threadId`, and `model`. `serviceTier`, raw `sandboxPolicy`, developer instructions, approval policy, and skill-cache forcing are server-owned/hidden. PTY lifecycle remains public through `command.start/read/control`.

## Version-state boundaries

`SourceChanged != Committed != Pushed != Deployed != Live != CIGreen`. Each state is owned and verified separately. Git actions are operator workflow, not application workflow. Backend deployment changes Codex Connect only; it does not refresh the ChatGPT connector or restart independently owned ingress services.
