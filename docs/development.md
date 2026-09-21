# Development

This repository is canonical-only pre-release code. Read [Architecture](architecture/overview.md) before changing authority, delegation, public tools, App Server integration, deployment, or service management.

## Pinned protocol contract

The current Codex CLI/App Server pin is `0.155.1`, recorded in `config/codex-cli-pin`. The project does not depend on Codex's internal Rust API. Instead, `scripts/generate-app-server-tool-schemas.py` verifies the installed release, generates the experimental App Server schema, and checks the self-contained protocol subset used by the adapter.

`config/app-server-tool-schemas.json` is an internal protocol drift guard, not the public MCP catalog. App Server owns official thread, turn, review, command, filesystem, approval, permission, elicitation, account, and notification semantics. Connect may project or adapt them, and may add a bridge only where the pinned App Server has no equivalent (currently content search and deterministic patch semantics).

## Public MCP governance

The exact public catalog is the 14-tool surface tested in `crates/mcp`: `status`, `inspect`, `view_image`, `apply_patch`, `command.exec`, `command.start`, `command.read`, `command.control`, `codex.start`, `codex.wait`, `codex.inspect`, `codex.control`, `codex.action.respond`, and `codex.info`.

Public inputs are intent-shaped. `command.exec` exposes only `command`, optional `cwd`, and optional `env`; execution timeout and output limits are server-owned. Its public path uses a 60-second child budget plus bounded response allowance; persistent/interactive work belongs on `command.start`. Persistent commands retain stdin, PTY resize, termination, cursor lifecycle, and event-driven reads (60-second default, 80-second maximum). Work delegation exposes `task`, optional `cwd`, `threadId`, `model`, `effort`, and `access`; review exposes its target plus optional `cwd`, `threadId`, and `model`.

Do not expose `serviceTier`, raw `sandboxPolicy`, approval policy, developer instructions, writable-root/network/temp switches, skill-cache forcing, or other server-owned controls. Do not invoke the Codex CLI through HostPlane commands when a `codex.*` semantic tool exists. Preserve the intent-shaped catalog, tool annotations, compact schemas, selection tests, and PTY support.

## Worker contract

Connect sends no `thread/start.developerInstructions`. Worker cognition comes from `~/.codex/config.toml` `developer_instructions`, `~/.codex/AGENTS.md`, repository/directory `AGENTS.md`, and the delegated task. Connect owns authority, lifecycle, and operator-facing orchestration.

For work, omitted `access` maps to the writable workspace sandbox with network access; `access="full"` maps to `danger-full-access`. Work turns send `approvalPolicy="never"`; this avoids mechanical approval stalls and does not widen the sandbox. New reviews are read-only.

Delegation is exclusive scope ownership until terminal, semantic action/input block, interrupt, or user redirect. A `codex.wait` timeout is only a bounded lease expiry, never failure, stall evidence, or takeover permission. The ChatGPT-facing default is 80 seconds because live validation found an external tool-runner cutoff around 100 seconds; the hard server ceiling remains five minutes for other callers/explicit experiments. `tunnel-client` remains authoritative for any shorter per-command outer deadline. `codex.start` is relay-owned once submitted: thread/resume/start plus turn/review start are completed independently of caller lifetime, and an unclaimed handle is retained as a one-shot `workerStarted` host event. Live state is notification-driven: reads establish pre-existing state, reconcile explicit history loss, hydrate terminal output, or perform one final lease-expiry check; they do not periodically observe progress. `codex.wait` synchronizes; `codex.inspect` owns activity/history observation; the console is a human-oriented read-only projection. App Server state remains authoritative; Connect's journals/reducers/caches are bounded observations.

Timeouts are classified by semantics, not by the tunnel polling cadence: quick/control tools keep a 45-second local guard; `codex.start` gets a 90-second caller guard while its owned upstream start can finish afterward; `command.exec` uses a 60-second child budget and 75-second MCP guard; `command.read` uses 60/80-second default/maximum leases with 5 seconds of MCP headroom; `codex.wait` defaults to 80 seconds with 15 seconds of local headroom and keeps a 300-second hard lease maximum. Ordinary App Server RPCs remain at 30 seconds, and management/health deadlines remain independently fail-fast.

## Validation

Run the repository gate with the pinned CLI installed and Python `jsonschema` available:

```bash
./scripts/generate-app-server-tool-schemas.py --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo check --locked
cargo build --locked -p codex-connect
python3 tests/protocol_integration.py
```

The protocol tests validate the exact catalog, App Server request/response schemas, server requests, streaming commands, worker authority mapping, and end-to-end lifecycle. Do not weaken a test or schema to accommodate an unpinned CLI.

For documentation changes, also check local Markdown links and search all maintained docs for retired terms. Keep the states distinct: `SourceChanged != Committed != Pushed != Deployed != Live != CIGreen`. Do not commit, push, deploy, or refresh the connector as part of documentation validation.

## Dependency and change discipline

Keep durable product behavior separate from diagnostics and scratch tooling. Prefer existing abstractions and standard library code for small changes. Review dependency and compile impact. When semantics change, rename the concept across code, schemas, tests, docs, comments, and examples; do not retain aliases, fallback readers, migration shims, deprecated names, or dual paths without an explicit current requirement.
