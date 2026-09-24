# Development

Read [Architecture](architecture/overview.md) before changing authority, lifecycle, App Server integration, or service management.

## Pinned protocol contract

The Codex CLI/App Server release is pinned in `config/codex-cli-pin`. `scripts/generate-app-server-tool-schemas.py` verifies the installed release, generates its experimental schema, and checks the protocol subset used by the adapter.

`config/app-server-tool-schemas.json` is an internal protocol drift guard, not the public MCP catalog. App Server owns official thread, turn, review, command, filesystem, approval, permission, elicitation, account, and notification semantics. Connect may project or adapt them, and may add a bridge only where the pinned App Server has no equivalent (currently content search and deterministic patch semantics).

## Public MCP governance

The public catalog and schemas are owned by `crates/mcp/src/catalog.rs`; parsing and dispatch are in `crates/mcp/src/lib.rs`. Update both with protocol tests when changing inputs or outputs.

Keep ownership one-way. `crates/host` owns host-path and Connect-native host mechanics. `crates/app-server` owns pinned upstream protocol/transport. `crates/relay` composes those primitives into lifecycle behavior. `crates/mcp` projects that behavior into the public tool/HTTP surface. The CLI is the composition root and local management UI; it should not duplicate Host authority, App Server invariants, or observer protocol logic.

Expose choices that control operator intent and project upstream protocol data into that vocabulary at the MCP boundary. Do not leak fields that merely repeat input, fixed constants, ranking internals, or account metadata that cannot drive an operator action. Keep execution limits and the worker wait budget in the relay; MCP guards must allow operations to finalize. `codex.wait` accepts only thread and turn IDs. `command.read.timeoutMs` supports immediate reads and bounded waits for interactive processes.

In the measured ChatGPT chat/Code Mode connector path, the highest returned tool duration was 57.597 seconds and calls near 57.60 seconds failed. This is an observed boundary, not a platform contract. Keep synchronous operation budgets near 50 seconds with local guards no higher than 55 seconds: `codex.wait` uses a fixed 40-second join plus 10-second finalization reserve, `command.exec` uses a fixed 40-second command plus a 10-second response allowance, and `command.read.timeoutMs` is the only exposed wait setting (0–50 seconds). Use `codex.start` or `command.start` handles for longer work.

Keep service tier, raw sandbox/approval policy, developer instructions, and cache controls internal. Preserve tool annotations, OAuth metadata, compact schemas, and PTY support. Test accepted inputs, outputs, and behavior without locking explanatory prose.

## Worker contract

Worker instructions come from Codex config, AGENTS.md, and the delegated task. Connect supplies authority and lifecycle controls; it does not inject developer instructions.

Work uses `approvalPolicy="never"` with the selected workspace or full-access sandbox. Disabling prompts does not widen the sandbox. New reviews start read-only.

Preserve thread identity only while the workstream remains cache-compatible. New workstreams establish cwd/model/effort/access and receive self-contained context. Resumed turns expose only thread identity plus the next objective/delta; Connect sends the canonical thread model and effort explicitly on every work turn and does not permit workstream setting changes. Connect uses a conservative 30-minute guaranteed-cache cutoff rather than assuming best-effort retention beyond OpenAI's minimum. Start fresh when settings must change, the cutoff is passed, or independence itself is useful, especially for adversarial review. Reuse never substitutes for checking mutable host/runtime state.

Preserve the [worker lifecycle and recovery contract](operations.md#worker-lifecycle). Starts complete independently of caller lifetime. Active turns remain retained until terminal and terminal state from events or authoritative reads must use the same reconciliation path. Events drive live state; reads hydrate existing state, reconcile history loss or wait expiry, and load one canonical terminal handoff capped at 10,240 characters. Keep persisted thread history intact; `codex.inspect` with `detail: "result"` searches App Server turn items newest-first with adaptive page sizing. Treat the result as authoritative only when `resultPage.selectionComplete` is true; a single item above the App Server transport limit leaves selection incomplete. Result text is paged in 10,240-character chunks, independent of relay journal retention. Raw detail is the relay notification journal and can be lost. Do not poll App Server for progress.

Each tool descriptor advertises OAuth scope `codex-connect:access`. Host ingress owns token validation, discovery challenges, and authorization metadata.

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

For documentation changes, verify local Markdown links and search for stale contract references. Validation does not include commit, push, deployment, or connector refresh.

## Dependency and change discipline

Keep durable product behavior separate from diagnostics and scratch tooling. Prefer existing abstractions and standard library code for small changes. Review dependency and compile impact. When semantics change, rename the concept across code, schemas, tests, docs, comments, and examples; do not retain aliases, fallback readers, migration shims, deprecated names, or dual paths without an explicit current requirement.

Prefer one concrete mechanism over speculative abstraction: share durable atomic-file writes, lock primitives, and validated authority objects instead of cloning their implementations. Split modules when they own a coherent state machine or boundary, not merely because a file is long. Do not add an interface with one implementation unless it creates a real testing or substitution boundary.
