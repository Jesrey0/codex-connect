# Contributing

Preserve the authority and lifecycle boundaries in [Architecture](docs/architecture/overview.md).

## Prerequisites

Use Linux with systemd user services, Rust 1.88 or newer, Python 3 with `jsonschema`, and the Codex CLI release in `config/codex-cli-pin`.

Read [Development](docs/development.md), [Architecture](docs/architecture/overview.md), and [Security](SECURITY.md) before changing public MCP behavior, authority, deployment, or service management.

## Validation

Run the [repository validation gate](docs/development.md#validation).

## Public-tool governance

Public tools need distinct ChatGPT/operator value, explicit safety annotations, compact schemas, and behavior tests. Follow the current OpenAI Plugin/MCP guidance for tool names, titles, descriptions, input/output schemas, per-tool `securitySchemes`, and annotations.

App Server primacy is an invariant, not a preference: if the pinned App Server exposes the primitive, use it and keep App Server authoritative for its state. Do not build a parallel Connect implementation for convenience. Preserve PTY support. Do not expose raw sandbox policy, service tier, developer instructions, approval policy, or other server-owned mechanics. Do not invoke Codex CLI through host commands when a `codex.*` semantic operation exists.

## Operating rules

- Keep Codex CLI/App Server and host ingress independently owned; public routing and OAuth belong to host ingress.
- Keep ChatGPT as the only supported action/control interface. A local human-visible console is allowed only for read-only observability. Do not add worker steering, approval responses, mutations, or lifecycle controls outside ChatGPT.
- Keep the managed backend's home-directory cwd as navigation, never authorization.
- Treat worker threads as cache-bounded workstreams. New threads establish cwd/model/effort/access and need self-contained context. Resumed turns keep those settings fixed and carry only the current objective/delta, changed facts, and acceptance criteria while revalidating mutable state when necessary. Start fresh outside the conservative 30-minute guaranteed-cache cutoff, when settings must change, work is unrelated, or independent review is intentional. Preserve delegated scope ownership until terminal/action/interrupt/redirect; a `codex.wait` timeout is not permission to take over.
- Keep live state event-driven; App Server remains authoritative. Reads may hydrate or reconcile state at explicit boundaries but must not periodically observe progress. Do not reintroduce timer-based worker, observer, transcript, or quota polling.
- Engineer synchronous ChatGPT-facing calls against the empirically observed ~55 second outer result window. Keep useful synchronous work near 50 seconds or below; move longer work behind retained worker/command handles. App Server, host, worker-join, command, and client-response budgets still follow their own semantics.
- Git actions are operator workflow, not application workflow.
- Remove superseded code, terminology, and compatibility paths unless a current consumer needs them.

Report source changes, test results, and unverified state separately. Commit, push, deployment, ChatGPT plugin rescan, and external communication require explicit authorization.
