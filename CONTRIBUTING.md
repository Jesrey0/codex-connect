# Contributing

Preserve the authority and lifecycle boundaries in [Architecture](docs/architecture/overview.md).

## Prerequisites

Use Linux with systemd user services, Rust 1.88 or newer, Python 3 with `jsonschema`, and the Codex CLI release in `config/codex-cli-pin`.

Read [Development](docs/development.md), [Architecture](docs/architecture/overview.md), and [Security](SECURITY.md) before changing public MCP behavior, authority, deployment, or service management.

## Validation

Run the [repository validation gate](docs/development.md#validation).

## Public-tool governance

Public tools need distinct operator value, explicit safety annotations, compact schemas, and behavior tests. Prefer App Server semantics over a new Connect abstraction.

Preserve PTY support. Do not expose raw sandbox policy, service tier, developer instructions, approval policy, or other server-owned mechanics. Do not invoke Codex CLI through host commands when a `codex.*` semantic operation exists.

## Operating rules

- Keep Codex CLI/App Server and host ingress independently owned; public routing and OAuth belong to host ingress.
- Keep `default_cwd` as navigation, never authorization.
- Keep worker tasks self-contained and preserve delegated scope ownership until terminal/action/interrupt/redirect. A `codex.wait` timeout is not permission to take over.
- Keep live state event-driven; App Server remains authoritative. Reads may hydrate or reconcile state at explicit boundaries but must not periodically observe progress. Do not reintroduce timer-based worker, observer, transcript, or quota polling.
- Keep timeout classes independent. App Server, host, worker-join, command, and client-response budgets follow their own semantics. Caller loss must not orphan durable worker starts, and a timed-out mutating host operation must not continue silently without cooperative cancellation/rollback.
- Git actions are operator workflow, not application workflow.
- Remove superseded code, terminology, and compatibility paths unless a current consumer needs them.

Report source changes, test results, and unverified state separately. Commit, push, deployment, connector refresh, and external communication require explicit authorization.
