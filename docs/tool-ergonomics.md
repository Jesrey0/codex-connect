# Tool ergonomics review

## Operator grouping and continuation

From source baseline `4e757abf6e1c8021883f4b80d31b2fc58863f173`, the deterministic
host tools use `host.inspect`, `host.apply_patch`, and `host.view_image`. Command,
Codex, status, and Workers names stay unchanged. The old bare names are removed.
Descriptions give purpose, selection guidance, and essential lifecycle rules;
schema fields and the operations guide carry precise constraints. The consumed
`codex.start` descriptor already exposes six explicit authority alternatives,
so its union and exact parser remain intact.

`codex.inspect` returns `nextCall={tool,arguments}` for available semantic/raw
journal pages or terminal result text pages, and `null` otherwise. Commands expose
the same shape on `command.read` and inside start/write `output`. Operators can
invoke these reads directly without rebuilding cursor arguments. They use canonical
IDs and cursors, preserve independent readers and history loss, and leave result
`selectionComplete` independent of text continuation. Active result candidates
cannot continue at nonzero text offsets. Command continuations stop only when
`drained=true` when an observation is available; failed observations return
`output=null`/`readError` and require handle recovery without replaying acknowledged
input. Completion alone establishes neither successful work nor output presence.
No new App Server or relay lifecycle, panel behavior, transport, authorization, or
budget mechanism is introduced; the relay retention fix below only preserves an
existing history-loss boundary monotonically.

The pagination protocol test fills the retained journal and exposed an existing
history-loss defect: eviction could overwrite a newer transport gap with an older
cursor. The journal now keeps that boundary monotonic, so semantic/raw continuation
reports missing history correctly after oversized notifications.

The local source gate does not verify deployment or refreshed ChatGPT discovery.
The primary operator owns plugin skill changes, immutable artifact deployment,
and package publication.

## Earlier call-count review

Reviewed the complete catalog on 2026-10-01: 14 model-visible tools and the one
app-only helper. The clean source baseline is commit
`26bf302f3fb8f273e021d3bbdd6d6c4ce2112a12`, created before modifications.

## Design target

The operator's measured read-only CSV sample was about 240 ms through local
execution versus 3,336 ms through Connect (five calls per route). That is an
end-to-end comparison, not an isolated ngrok benchmark. Treating the remote
round trip as the dominant cost makes avoiding a call more useful than shaving
milliseconds from local parsing. No tunnel, ingress, or OAuth change is needed.

Evaluate tools on call count, clarity of required inputs, compact actionable
results, retained recovery, bounded responses, and faithful upstream semantics.
Copy the direct tool pattern of starting/writing and observing in one call;
retain Connect's explicit execution intent and authority boundaries.

## Complete catalog assessment

| Tool | Assessment and decision |
| --- | --- |
| `status` | Keep a single compact orientation/recovery call. Recovery is backend-global and non-destructive; cwd is context, never ownership. No duplicate account or deployment state. |
| `host.inspect` | Already batches independent file reads, directory listings, metadata, literal content search, and fuzzy discovery. Use one batch before iterative calls. Keep per-operation errors and aggregate bounds; continue using App Server for exposed filesystem/fuzzy primitives. |
| `host.view_image` | Already returns native image content with high/original detail. Keep host paths explicit and image bounds intact. A new download/upload route would cross a different authority boundary. |
| `host.apply_patch` | Keep one deterministic diff operation with existing cancellation/rollback handling. No worker required for a known patch. Do not add a duplicate file-writing tool or mutation batch with ambiguous partial retries. |
| `command.exec` | Keep the concise short-command path and fixed execution/output bounds. It already returns exit code, stdout/stderr, truncation indicators, and elapsed time in one call. Use explicit argv/shell intent for a coherent command. |
| `command.start` | **Changed:** returns the existing retained handle plus the first output/exit observation. Optional `yieldTimeMs` defaults to 1,000 and permits 0–10,000. Yield expiration leaves execution running. A lost response is recovered through `status.commands`. |
| `command.read` | Keep independent cursor reads, bounded output, explicit history loss, and `hasMoreOutput`/`drained`. Describe cursors from start/control as well as reads. Never interpret terminal state alone as fully drained output. |
| `command.control` | **Changed for write:** stdin write/close plus retained observation in one call. `afterCursor` avoids replaying earlier output; `yieldTimeMs` has start's semantics. Validate observation arguments before mutation. Preserve acknowledged input if observation later fails. PTY resize/termination remain explicit actions. |
| `codex.start` | **Changed:** explicit disjoint schemas for fresh workspace work, fresh full-access work, resume, fork, fresh review, and resumed review. All require model; fresh variants require cwd. Resume/fork advertise no inherited-setting overrides. Existing relay validation, lifecycle, advisory cache hints, and sandbox behavior stay authoritative. |
| `codex.wait` | Keep the bounded event-driven join and canonical terminal handoff. It already combines completion, pending action/input, and compact output. No user wait tuning or polling loop; operator dependency determines when to join. |
| `codex.inspect` | Keep semantic activity, raw retained events, and canonical result text as explicit modes. Existing cursor/text pagination and `selectionComplete` distinguish completeness from authority. Do not replace authoritative result retrieval with a local transcript cache. |
| `codex.query` | Already batches independent model/skill/usage/thread/background-terminal discovery with per-query errors. Keep small actionable projections and existing pagination. No host CLI wrapper for Codex primitives. |
| `codex.act` | Keep explicit action variants, expected-turn protection for steering, pending-request IDs, and batch archive/delete inputs. Preserve upstream errors. A generic batch of unrelated mutations would make partial completion and retries harder to reason about. |
| `workers.open` | Already delivers the initial retained snapshot with the panel opener; no duplicate initial fetch. Keep the panel observational, context attachment explicit, and mutations model-only. |
| `workers.snapshot` (app-only) | Keep explicit Refresh and shared retained observer projections. No browser polling, second worker lifecycle, or conversation ownership. |

## Concrete call savings

For an interactive program with immediate prompts:

| Workflow | Previous calls | Revised calls |
| --- | ---: | ---: |
| Start and inspect the first prompt | 2 | 1 |
| Send input and inspect its response | 2 | 1 |
| Close stdin and inspect exit | 2 | 1 when exit is observed within the yield |

An entire prompt/input/close exchange can therefore use three calls instead of
six. A quiet/slow process still needs reads; savings depend on output timing.
This reduces round trips, not the latency of each individual ngrok request.

Both changed command responses contain `output` (the existing command-read
shape) and `readError`. Start also returns `processId`/`cwd`; write preserves
`written`/`stdinClosed`. Observation never consumes retained output. If it fails
after the operation succeeds, the result keeps the handle or acknowledgement,
sets `output=null`, and reports `readError`. Recover by reading; do not replay
acknowledged input. A lost transport response still requires state recovery.

No compatibility alias, combined execution-mode tool, generic mutation batch,
new lifecycle, or dependency is introduced. The public catalog remains the same
size. The worker schema replaces conditional `allOf`/`if`/`then` rules with
explicit object alternatives, making valid input choices visible to consumers.
Actual ChatGPT discovery/rendering must be verified after deployment/rescan.

## Invariants retained

The [canonical invariants](architecture/overview.md#architectural-invariants)
remain the acceptance criteria:

1. ChatGPT owns action intent; no console/UI mutation path is added.
2. Host/Worker/Platform authority stays separate; connected-account uploads are
   not silently granted to host commands.
3. Commands still use the pinned `command/exec` streaming primitive and official
   write/resize/terminate methods. The new observation reads the existing cache.
4. Handles/recovery stay backend-global; no caller/project ownership is stored.
5. Relay owns observation limits; MCP imports them instead of copying constants.
6. Worker scope/delegation policy is unchanged; routine host work needs no worker.
7. Source, commit, push, build, deployment, live identity, plugin discovery, and
   CI remain separately verified states.
8. Resume/fork inherit canonical cwd, effort, and access. Model validation, advisory cache hints, and upstream root-persistence limitations remain.
9. Output is bounded, retained, cursor-based, and event-driven. Observation
   timeout/caller loss does not establish process or worker failure.
10. Loopback backend, independent ingress/OAuth, scope, and metadata stay intact.
11. The prerelease schema changes coherently without aliases or fallback paths.

Local tool guards remain at or below 48 seconds. PTY support, independent
readers, output eviction indicators, deterministic command limits, worker
approval/sandbox defaults, and read-only reviews remain intact.

## Deliberate limits

Do not add an arbitrary `maxOutputChars` by truncating a cursor batch: advancing
the cursor after cutting text would skip unread bytes. A future smaller-output
option needs pagination that preserves chunk offsets, byte bounds, UTF-8, and
history-loss semantics. Current bounded output remains unchanged.

Host artifact visibility and Google Drive upload are separate concerns. Native
images already work; a future artifact handoff must explicitly preserve host
and PlatformPlane ownership. No new external account access is introduced here.

## Verification

All repository gate checks passed on 2026-10-01: 200 workspace tests, 66 protocol
tests, pinned schema generation check, formatting, diff hygiene, strict Clippy,
and the locked CLI build. Schema validation used the existing Linux Codex
0.159.0 binary through an isolated command PATH; the desktop shell's 0.159.2
binary and the project pin were not changed. This verification record predates
the current 0.160.0 pin; re-run the gate under the current pin before treating
it as current acceptance.

Regression coverage checks combined PTY start/write/close, zero-yield continued
execution, invalid cursor/yield rejection before upstream mutation, retained
start recovery after caller timeout, explicit worker input choices, and the
existing full protocol/lifecycle/authority suite. The repository gate checks the
pinned App Server schemas, formatting, diff hygiene, workspace tests, linting,
build, and protocol integration. These checks do not prove live deployment,
ngrok latency improvement, or refreshed ChatGPT tool discovery.
