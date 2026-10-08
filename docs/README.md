# Documentation

Read these in order as needed:

- [Getting Started](getting-started.md) — install the pinned dependencies, backend, authenticated HTTPS ingress, and ChatGPT plugin connection, then prove the complete path.
- [Architecture](architecture/overview.md) — the canonical architectural invariants, shared single-user backend model, HostPlane/WorkerPlane/PlatformPlane ownership, upstream-first design, and public MCP contract.
- [Operations](operations.md) — ChatGPT tool lifecycles plus maintainer-only backend deployment and recovery.
- [Development](development.md) — pinned App Server ownership, OpenAI Plugin/MCP catalog rules, timeout budget, and validation.

- [Tool Ergonomics](tool-ergonomics.md) — complete catalog review, fewer remote calls, and preserved invariants.

Policy and history:

- [Security](../SECURITY.md)
- [Contributing](../CONTRIBUTING.md)
- [Changelog](../CHANGELOG.md)

Sibling operational sources (not copied into this backend): [host-ingress](https://github.com/Jesrey0/host-ingress) owns OAuth, ngrok, Caddy and the public origin; [OpenCode Connect](https://github.com/Jesrey0/opencode-connect) owns its separate backend and Windows computer/printing adapters. A Git update to any one repository does not deploy the others.
