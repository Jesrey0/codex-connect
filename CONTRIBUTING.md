# Contributing

Preserve the canonical [architectural invariants](docs/architecture/overview.md#architectural-invariants). Treat them as the review checklist; the rest of Architecture and Development provide the detailed contracts behind them.

## Prerequisites

Use Linux with systemd user services, Rust 1.88 or newer, Python 3 with `jsonschema`, and the Codex CLI release in `config/codex-cli-pin`.

Read [Development](docs/development.md), [Architecture](docs/architecture/overview.md), and [Security](SECURITY.md) before changing public MCP behavior, authority, deployment, or service management.

## Validation

Run the [repository validation gate](docs/development.md#validation).

## Public-tool governance

Public tools need distinct ChatGPT/operator value, explicit safety annotations, compact schemas, and behavior tests. Follow the current OpenAI Plugin/MCP guidance for tool names, titles, descriptions, input/output schemas, per-tool `securitySchemes`, and annotations.

App Server primacy is an invariant, not a preference: if the pinned App Server exposes the primitive, use it and keep App Server authoritative for its state. Do not build a parallel Connect implementation for convenience. Preserve PTY support. Do not expose raw sandbox policy, service tier, developer instructions, approval policy, or other server-owned mechanics. Do not invoke Codex CLI through host commands when a `codex.*` semantic operation exists.

## Operating rules

- Keep the managed backend's home-directory cwd as navigation, never authorization.
- Engineer synchronous ChatGPT-facing calls against the empirically observed ~55 second outer result window. Keep local tool guards at or below 48 seconds; move longer work behind retained worker/command handles. App Server, host, worker-join, command, and client-response budgets still follow their own semantics.
- Git actions are operator workflow, not application workflow.

Report source changes, test results, and unverified state separately. Commit, push, deployment, ChatGPT plugin rescan, and external communication require explicit authorization.
