# Changelog

Notable project-level changes are recorded for tagged releases. Codex Connect is pre-release; support is defined by the current documentation and the pinned Codex CLI contract.

## Unreleased

### Changed

- The canonical ChatGPT path is now ngrok HTTPS → host ingress (routing + OAuth) → loopback Codex Connect.
- OpenAI Secure MCP Tunnel is retained only as an independently managed secondary fallback and is no longer a setup, health, deployment, timeout, or application-architecture dependency.
- Transport-specific tunnel configuration is no longer project-owned; client response deadlines remain independent from Codex Connect operation budgets.

## 0.1.0-alpha.1 — 2026-09-18

Initial public alpha of the single-user, self-hosted Codex Connect architecture.

### Included

- ChatGPT-facing MCP tools for host inspection and mutation, persistent commands, Codex work/review, typed pending actions, discovery, and usage telemetry.
- Official Codex App Server as the authority for threads, turns, commands, reviews, approvals, permissions, elicitation, and account semantics.
- Loopback-only MCP backend intended for OpenAI Secure MCP Tunnel.
- Linux/systemd setup, health/doctor/restart/log operations, content-addressed backend artifacts, and detached two-phase source deployment.
- Separate deterministic host authority and delegated Codex work authority.
- Pinned Codex CLI protocol validation with generated-schema drift checks and protocol integration coverage.
- `codex-connect uninstall` removes Codex Connect-owned backend state while preserving source, Codex CLI, and tunnel-client state.

### Support boundary

- Managed installation targets Linux with systemd user services.
- Installation is source-based; no prebuilt binary packages are distributed in this alpha.
- Codex Connect uses user-global configuration, state, cache, installed builds, and executable locations; the source checkout location is arbitrary.
- The default navigation cwd is `~`; Codex CLI/App Server and `tunnel-client` remain independently user-global.
- The product is single-user/self-hosted and does not expose the MCP backend directly to the public internet.
- ChatGPT Developer mode/custom MCP availability is account- and rollout-dependent; see [Getting Started](docs/getting-started.md).
