---
name: 0xoperator
description: Own connected host and coding work across Codex Connect and OpenCode Connect. Select a substrate for a coherent workstream and apply its provider mechanics. Use for operator work, delegation, recovery, and verification through these connections.
---

# 0x0perator

ChatGPT owns the user's goal, strategy, integration, deployment, and final acceptance.
Choose the available substrate that fits the workstream, then read its provider
skill and live tool schemas. Those schemas own call arguments and lifecycle contracts.

## Select an owner

One active substrate owns each coherent workstream: host operations, worker
lifecycle, recovery, and event handling stay with that provider. Codex Connect
uses threads and turns; OpenCode Connect uses persisted sessions and messages.
Keep their explicit namespaces and canonical identities.

A cross-provider workstream needs a clear purpose and ownership boundary, such
as implementation in one substrate and independent review in the other. Establish
the current canonical state before transitioning. Slowness or a lost response
does not establish failure or authorize replacement work in another provider.

## Use the owning plane

PlatformPlane owns ChatGPT capabilities and subscriptions. HostPlane owns files,
processes, Git, deployment, and live services. WorkerPlane belongs to the selected
runtime. Use deterministic host/runtime primitives for direct operations; delegate
when autonomous investigation, implementation, or independent review materially helps.

Give workers bounded objectives and runtime-enforced authority where available.
Keep useful non-overlapping operator work with ChatGPT while they run. Worker
narration and conversation history provide context; canonical host/runtime state
provides evidence.

## Recover and verify

Retain canonical worker and command handles. Recover retained work before creating
a replacement after interruption, transport loss, or missing events. An event is
a wake-up hint; inspect persisted state before acting on it. Use bounded waits at
real dependency boundaries and preserve authority across reconnects.

Verify source changes, checks, Git commits, prepared artifacts, deployment, live
services, connector discovery, event delivery, and CI separately. Finish with
verified outcomes and any remaining concrete acceptance or user action.

Codex and OpenCode remain independently deployed runtimes. Shared ingress and
operator doctrine do not create a universal backend or a second authority for
either runtime's worker state or model catalog.
