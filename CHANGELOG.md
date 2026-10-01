# Changelog

Notable project-level changes remain under `Unreleased` until Codex Connect has an actual release. Support is defined by the current documentation and the pinned Codex CLI contract.

## Unreleased

### Changed

- Account-plugin sources and 0x0perator skills moved out of this backend repository to the neutral user-root `~/0xoperator/` source tree; Codex Connect now owns backend/runtime code only.
- Persistent command start and stdin write return a retained output/exit observation in the same call, with a bounded yield that never stops execution.
- `codex.start` now advertises explicit fresh/resumed/forked input variants that match the existing inherited-setting rules.

- The canonical ChatGPT path is now ngrok HTTPS → host ingress (routing + OAuth) → loopback Codex Connect.
- OpenAI Secure MCP Tunnel is no longer part of the current supported architecture; ngrok plus host ingress is the single remote path.
- Transport-specific tunnel configuration is no longer project-owned; client response deadlines remain independent from Codex Connect operation budgets.
- The ChatGPT-facing MCP surface now projects operator intent rather than upstream transport detail, with compact discovery, normalized pending actions, persistent-command recovery through `status`, bounded terminal output, and one terminal-turn reconciliation path.
- ChatGPT is the only supported action/control interface. The local console/observer surface remains intentionally read-only for worker visibility; all mutations stay in ChatGPT.
- Codex App Server primacy is explicit: Connect reuses upstream primitives and state whenever available, adding Connect-native HostPlane mechanics only for genuine protocol gaps.
- ChatGPT-facing synchronous work is budgeted against the empirically observed ~55-second result window, with longer work moved behind retained worker or command handles.
- Public tool descriptors track the current OpenAI Plugin/MCP contract, including compact schemas, behavioral annotations, and per-tool OAuth `securitySchemes`.
