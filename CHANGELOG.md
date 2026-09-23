# Changelog

Notable project-level changes remain under `Unreleased` until Codex Connect has an actual release. Support is defined by the current documentation and the pinned Codex CLI contract.

## Unreleased

### Changed

- The canonical ChatGPT path is now ngrok HTTPS → host ingress (routing + OAuth) → loopback Codex Connect.
- OpenAI Secure MCP Tunnel is no longer part of the current supported architecture; ngrok plus host ingress is the single remote path.
- Transport-specific tunnel configuration is no longer project-owned; client response deadlines remain independent from Codex Connect operation budgets.
- The ChatGPT-facing MCP surface now projects operator intent rather than upstream transport detail, with compact discovery, normalized pending actions, persistent-command recovery through `status`, bounded terminal output, and one terminal-turn reconciliation path.
