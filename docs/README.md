# Documentation

Read these in order as needed:

- [Getting Started](getting-started.md) — install the pinned dependencies, backend, authenticated HTTPS ingress, and ChatGPT plugin connection, then prove the complete path.
- [Architecture](architecture/overview.md) — the canonical architectural invariants, shared single-user backend model, HostPlane/WorkerPlane/PlatformPlane ownership, upstream-first design, and public MCP contract.
- [Operations](operations.md) — ChatGPT tool lifecycles plus maintainer-only backend deployment and recovery.
- [Development](development.md) — pinned App Server ownership, OpenAI Plugin/MCP catalog rules, timeout budget, and validation.

Policy and history:

- [Security](../SECURITY.md)
- [Contributing](../CONTRIBUTING.md)
- [Changelog](../CHANGELOG.md)

Feature proposals:

- [MCP Events operator follow-up](proposals/mcp-events-operator-follow-up.md) — proposed asynchronous worker notifications that trigger instructed follow-up in a subscribed ChatGPT chat; account validation pending.
