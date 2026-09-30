# Proposal: MCP Events for asynchronous ChatGPT operator follow-up

Status: core source implementation completed in host-ingress and codex-connect; repository validation in progress; deployment and account acceptance pending. See the [checkpoint report](mcp-events-authentication-checkpoint.md).
Date: 2026-09-30.
Scope: codexConnect repository. This document authorizes no deployment or plugin rescan.

This document retains the original design proposal as historical context. Its
pre-implementation inventory and future-tense design sections are superseded by
the [actual implementation contract and validation](mcp-events-authentication-checkpoint.md).
The ingress contract uses encrypted opaque contexts and its native provider grant
checks; the earlier signed-context suggestion was not implemented. No deployment,
plugin rescan, live subscription or account acceptance has occurred.

## Problem and intended behavior

A retained Codex worker can outlive the ChatGPT turn that started it. Today, the operator must return and inspect the worker or wait at a synchronization boundary. We want a subscribed ChatGPT conversation to receive a meaningful worker transition and perform the follow-up the user requested, even after the initiating assistant turn has ended.

Example: the user asks ChatGPT to monitor a particular worker and report its result. ChatGPT finishes its current reply. The worker completes later; codexConnect delivers an event; ChatGPT starts a follow-up in the subscribed chat, reads the authoritative result, and reports it. Further delegation requires an existing bounded user instruction.

This keeps ChatGPT as operator and App Server as worker authority. Event delivery is an additional notification path, not a new host agent or a replacement worker state machine.

## Documented platform contract

[OpenAI MCP Events](https://developers.openai.com/plugins/build/mcp-events), checked 2026-09-30, documents ChatGPT webhook subscriptions on MCP 2.0 (`2026-07-28`). ChatGPT supplies a callback and signing secret, receives matching events in the subscribed chat, and acts on the user's instructions. Receipt is asynchronous; HTTP success does not prove that the follow-up ran. Delivery may be batched.

Advertise `events` in `server/discover`; implement `events/list`, `events/subscribe`, and `events/unsubscribe`. Use verified HTTPS callbacks and Standard Webhooks signatures. Persist subscriptions through restart, enforce access and expiration, support refresh and cancellation. Preserve event IDs on bounded retries; avoid retrying `410` or `413`. Keep each body within 256 KiB. Replay is optional; use null cursors when unavailable.

The integration supports webhook delivery rather than polling or streaming. Follow the linked guide for exact schemas, verification challenges, signing, callback address validation, key rotation, and lifecycle rules.

Platform support is documented. This user's Plus account eligibility, deployed integration behavior, post-turn wake-up, and browser-closed behavior remain untested. No usage-pool exemption, unlimited inference, delivery latency, or continuous supervision is assumed.

### Integration boundary and ngrok routing

ChatGPT owns the association between a subscription and the subscribed ChatGPT chat. The documented `events/subscribe` request supplies an event name, filters, callback URL, signing secret, and optional replay cursor; it does not require a ChatGPT thread or session ID. Connect stores the verified callback and subscription metadata, then delivers matching events to that callback. Do not infer chat identity by decoding callback URLs or transport session IDs.

The existing ngrok agent exposes the host ingress for incoming ChatGPT calls, including event discovery, subscription, refresh, and cancellation. Ingress authenticates those calls before forwarding them to Connect. Event delivery travels in the other direction: Connect makes an outbound HTTPS request to the ChatGPT-supplied callback. This does not require routing delivery through the inbound ngrok tunnel. Ngrok does not create ChatGPT chats, subscriptions, or operator sessions.

An existing ChatGPT chat must first establish a subscription with the user's monitoring and follow-up instructions. Later matching events can trigger asynchronous processing in that subscribed chat after the initiating turn ends. Without a subscription there is no callback destination; the documented event flow does not provision an unbound ChatGPT chat automatically. An active subscription is required, rather than a continuously running assistant turn.

Keep the identifiers distinct: the subscription ID identifies delivery state; the callback is the ChatGPT-provided delivery destination; `threadId` and `turnId` filters below identify the upstream Codex worker resource. Neither worker handles nor MCP transport sessions identify the ChatGPT conversation. The authenticated principal authorizes monitoring and is separate from all of these identifiers.

## Repository findings

Inspected the source tree on `main` with a clean working tree before this proposal:

- [Development](../development.md) already specifies only MCP `2026-07-28`; this feature does not need a protocol migration or legacy compatibility path.
- [MCP boundary](../../crates/mcp/src/lib.rs) owns public transport and tool dispatch. Event discovery and dispatch belong at this boundary, separate from the existing 13-tool catalog.
- [Relay](../../crates/relay/src/lib.rs) processes App Server notifications and reconciles authoritative terminal reads. Both paths must converge before generating an external transition.
- [Event journal](../../crates/relay/src/event_journal.rs) is bounded, in-memory recovery/observer history. It is not a durable webhook queue or replay guarantee.
- [Thread subscriptions](../../crates/relay/src/thread_subscriptions.rs) manage upstream App Server observation. They are distinct from ChatGPT webhook subscriptions.
- [Storage](../../crates/host/src/storage.rs) provides atomic file replacement; [paths](../../crates/cli/src/paths.rs) owns XDG roots.
- [Architecture](../architecture/overview.md) and [Security](../../SECURITY.md) keep OAuth at independently managed host ingress. Ingress strips credentials before forwarding to the loopback backend.

These are source findings, not a claim about the currently deployed build.

## Proposed first release

### Events and filters

Start with one event, `codex.turn.terminal`. It represents any authoritative terminal outcome, including failure or interruption, rather than implying that every completed turn succeeded. Exact upstream status mapping must be checked against the pinned App Server contract.

Require `threadId` and `turnId` subscription filters. These identify resources being watched; they do not establish conversation, project, or operator ownership. Do not add backend-global wildcard monitoring in the first release.

Proposed application data:

| Field | Purpose |
| --- | --- |
| `threadId` | Canonical upstream thread handle |
| `turnId` | Canonical upstream turn handle |
| `status` | Upstream terminal outcome |
| `cwd` | Factual navigation context, never authorization |
| `summary` | Short factual handoff hint; optional |

Use existing `codex.inspect` result reads for the full handoff. Do not publish transcripts, host secrets, raw approval contents, or instructions in payloads. Defer a separate `codex.action.required` event until pending-request transitions and their lifecycle can be mapped without duplicating upstream authority. Retained command notifications are a later extension.

### Transition and delivery ownership

Use a single projection after relay terminal reconciliation, whether terminal state came from a notification or an authoritative read. A stable terminal-event identity based on thread, turn, and event kind prevents the two paths from producing different logical events.

Keep webhook subscription state and pending deliveries in a small, concrete service composed by the CLI. The MCP crate owns wire schemas; Relay emits compact transition facts; the delivery service owns only notification state. Inspect RMCP 3.4.0 extension support before selecting the dispatch mechanism. Reuse an upstream implementation where available.

Propose an atomic-file-backed subscription/outbox store under the effective user-global codexConnect state root. Adapt the existing storage helper into one shared owner if needed; avoid copying it. Keep secrets owner-readable and exclude them from logs and observer output. No new database, scheduler, generic event bus, or independent model runner is proposed.

For each active matching subscription, durably record a delivery before network I/O. Store attempt count, next attempt time, and terminal delivery outcome. Do not allow a slow callback to block App Server notification consumption or an MCP tool response. Define bounded queue size, retention, retry exhaustion, and overflow diagnostics before release.

Initial replay policy: no protocol history replay. Retry persisted pending deliveries through restart. Record observed transitions durably, but explicitly document that the current live observation machinery cannot guarantee capture of every transition during backend downtime. Revisit replay only with a durable sequence and proven gap handling.

### Ingress boundary: blocking design decision

The subscription contract needs a trusted principal and access-revocation checks, while current ingress strips credentials and the backend is intentionally single-user.

Before production implementation, agree an ingress-owned authenticated context and revocation mechanism. Any forwarded identity must be overwritten by ingress and never accepted from a public caller as proof. Do not persist bearer tokens, invent ChatGPT conversation identity, or add a parallel OAuth authority in Connect.

A principal associated with a delivery subscription is protocol authorization metadata, not new worker or project ownership. Explain this distinction in the architecture update. If the ingress contract cannot provide the required authorization semantics, record a blocked milestone rather than claiming production support.

## Operator behavior

The user selects what to watch and what follow-up to perform. A useful initial instruction is: “Monitor this thread and turn; when it ends, inspect the result and report the outcome. Do not start another worker.”

On receipt, ChatGPT rechecks mutable worker and host facts through existing tools. A receipt acknowledgement proves delivery only. Multiple subscribed chats may observe the same trusted-user resource; observing it does not grant exclusive control. Automatic mutation needs a bounded goal and a demonstrated way to avoid duplicate side effects under concurrent or repeated follow-ups.

Keep subscriptions limited to meaningful transitions, not token deltas or routine progress. Cancel monitoring when the user's goal is complete. Preserve native waiting for actual synchronization boundaries; events do not justify progress polling.

## Implementation sequence and acceptance

1. **Contract spike.** Inspect the pinned RMCP/App Server contracts and ingress integration. Choose the authorization design above. Implement the smallest usable terminal event path. Through an explicitly authorized test deployment/rescan, verify event discovery and subscription on the user's account. Stop and record the concrete blocker if unavailable.
2. **Prove asynchronous follow-up.** Start a bounded test worker, subscribe to its IDs, and let the initiating assistant turn finish before the worker ends. Confirm a later ChatGPT turn receives the event and reads the result. Separately test a closed browser. Record subscription, event, worker handles and observed timing without credentials; a webhook `2xx` alone fails this acceptance criterion.
3. **Durable delivery.** Implement the store/outbox, startup recovery, bounded dispatch, expiration, refresh, cancellation, and diagnostics. Test two subscriptions to the same worker with distinct callbacks, and prove delivery does not create operator ownership.
4. **Failure coverage.** Exercise notification/read reconciliation races, duplicate terminal observations, duplicate subscription requests, wrong resource filters, restart with pending delivery, retry exhaustion, revoked access, cancellation during retry, secret rotation, unsafe callback destinations, and queue overflow. Verify existing tools and App Server subscriptions retain their semantics.
5. **Review and release.** Run the [repository gate](../development.md#validation), update architecture/security/operations and operator skill guidance, then obtain explicit deployment and rescan authorization. Verify source, deployed build, plugin discovery, and real ChatGPT follow-up separately.

## Next operator handoff

Begin with the ingress authorization decision and the smallest post-turn account test; do not start with a broad framework. Inspect the current working tree and pinned dependencies again because this proposal is a source snapshot. Keep implementation on the existing architecture, remove dead paths when semantics change, and preserve all unrelated work.

This handoff contains a proposal only. No runtime changes, subscription, automation, commit, push, deployment, or plugin rescan were performed.

