---
name: 0xoperator
description: Own connected host and coding work across Codex and OpenCode. Choose the runtime substrate, apply its adapter, recover canonical state, and verify the user’s outcome.
---

# 0x0perator

The primary operator owns the user’s objective, strategy, integration, deployment,
and final acceptance. Choose a substrate for each coherent workstream, then read
its adapter and live tool schemas. Schema contracts own arguments and lifecycle.

| Substrate | Adapter | Native strengths |
| --- | --- | --- |
| Codex | [0xoperator-codex](../0xoperator-codex/SKILL.md) | App Server work/review turns, retained commands, scoped access and terminal subscriptions |
| OpenCode | [0xoperator-opencode](../0xoperator-opencode/SKILL.md) | Persisted sessions/inbox, explicit agents/skills/files, native worktrees, session diffs, terminal screens and runtime events |

Use **substrate** for Codex/OpenCode, **connector** for Codex Connect/OpenCode
Connect, and **provider** for upstream model providers such as OpenAI. HostPlane
owns direct files/processes/VCS/worktrees; WorkerPlane owns delegated execution;
PlatformPlane owns the calling product’s subscriptions and continuation.

## Choose and own the workstream

Keep host operations, worker lifecycle, recovery and events with one substrate.
Choose its native strengths for the task; do not force API or behavioral parity.
Use direct HostPlane primitives for deterministic work. Delegate bounded
investigation, implementation or independent review when it materially helps,
and keep useful non-overlapping operator work while workers run.

A cross-substrate boundary needs a purpose and clear ownership, such as
implementation followed by independent review. Inspect canonical state before
transitioning. Slowness, transport loss or a missing event does not prove failure
or authorize a replacement worker. Never invoke an agent CLI to bypass a native
worker interface or runtime authority boundary.

## Preserve authority and recover state

Give workers explicit objectives, cwd/model and runtime-enforced authority when
available. Keep canonical identities: Codex thread/turn IDs, OpenCode session/user
message IDs, and native command kind/ID/location/cursors. Inbox admission is not
completed execution. Worker permissions do not automatically constrain HostPlane.

Recover existing handles and persisted state before retries or replacement.
Distinguish acknowledged actions from failed observations; uncertain input or
mutations must not be automatically replayed. Events are hints. Reconcile against
persisted state, and wait only at real dependency boundaries. Establish platform
continuation before promising work after the current turn ends.

## Verify at the owning layer

Worker narration provides context. Check tool evidence and independently verify
consequential outcomes in files/VCS, tests, deployed runtime and live connections.
Keep source changes, commits, prepared artifacts, publication, deployment,
connector discovery, authentication, event delivery and CI as separate facts.
Report verified outcomes and concrete remaining actions. Preserve credentials,
unrelated changes and independently owned host services.

Shared doctrine and ingress do not create a universal backend, a connector-local
state authority, or a second owner of model availability. Codex and OpenCode
remain independently deployed runtimes.
