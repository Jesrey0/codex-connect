# Changelog

All notable project-level changes are recorded here for tagged releases. Codex Connect remains pre-release software; compatibility is defined by the documented support boundary and the pinned Codex CLI contract.

## 0.1.0-alpha.1 — 2026-09-18

Initial public alpha of the single-user, self-hosted Codex Connect architecture.

### Included

- ChatGPT-facing MCP surface for scoped host inspection, deterministic command/file operations, persistent command sessions, Codex work/review workflows, typed pending actions, model/skill discovery, and Codex usage telemetry.
- Official Codex App Server as the authority for thread, turn, command, review, approval, permission, elicitation, and account semantics.
- Loopback-only MCP backend intended for OpenAI Secure MCP Tunnel.
- Linux/systemd managed setup, health/doctor/restart/log operations, content-addressed backend artifacts, and detached two-phase source deployment.
- Explicit sandbox ownership: host commands inherit upstream Codex policy only when no override is supplied; autonomous Codex work requires an explicit policy.
- Pinned Codex CLI protocol contract with generated-schema drift checks and end-to-end protocol integration coverage.
- Reversible `codex-connect uninstall` for Codex Connect-owned backend state while preserving source, Codex CLI, and tunnel-client state.

### Support boundary

- Managed installation currently targets Linux with systemd user services.
- Installation is source-based; no prebuilt binary packages are distributed in this alpha.
- The product is single-user/self-hosted and intentionally does not expose the MCP backend directly to the public internet.
- ChatGPT Developer mode/custom MCP availability is account- and rollout-dependent; see `docs/getting-started.md` for the verified project scope.
