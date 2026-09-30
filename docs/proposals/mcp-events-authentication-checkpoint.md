# MCP Events implementation checkpoint

Date: 2026-09-30.
Scope: source implementation in host-ingress and codex-connect only.
Status: ingress and core Events implemented; combined repository gates passed after
integration with the parallel `writableRoots` workstream. Connect deployment,
plugin discovery and live ChatGPT acceptance are unverified.

## Authorization and history

The original inspection worker could write only Connect and therefore recorded a
filesystem blocker after verifying the missing ingress identity/revocation
contract. The interrupted continuation made no implementation changes. This
continuation has explicitly authorized full host access, bounded to these two
main checkouts, and resolves that source prerequisite. Full access did not grant
commit, push, deployment, restart, plugin mutation/rescan or live subscription
authority. No additional workers were started.

The fixed local operator, ingress OAuth provider/store and ngrok routing remain
unchanged. Distinct clients/grants/callbacks describe routing and revocation for
that one trusted user. They create no users, accounts, tenants, roles, worker
ownership or ChatGPT chat identity. The [original proposal](mcp-events-operator-follow-up.md)
is retained as design history; this checkpoint owns the actual contract.

The prior baseline (175 workspace tests and 59 protocol integration tests) was
not rerun before code changes. It did not cover Events. Existing documentation,
including the pre-existing `docs/README.md` change, was preserved; current
security/architecture/operations sections were updated to the implemented contract.

## Implemented ingress contract

Host-ingress remains the sole authority for OAuth, keys, subject and live grants.
No database access or grant-state mirror was added to Connect.

1. The existing successful private token verifier returns
   `X-Host-Ingress-Auth-Context`, an opaque AES-256-GCM context authenticated with
   an ingress-only domain-separated key derived from existing cookie signing
   material. Associated data binds the configured issuer. It contains a request
   purpose, principal, client ID, opaque grant ID, exact resource/scope and
   issuance/expiry times. Lifetime is at most 30 seconds and never outlives the
   access token. Configured older cookie keys permit controlled key rotation.
2. The generated MCP handler uses an ordered Caddy `route`: delete every caller
   context header, run `forward_auth`, copy its context header, reject success
   without context, then proxy. OAuth bearer credentials, cookies and the
   consent-only operator header are still stripped. Context headers are redacted
   from access logs; request bodies are not logged. Public OAuth paths and ngrok
   routing are unchanged.
3. `POST /_host-ingress/events/authorize/codex-connect` exists only on the private
   OAuth listener and is excluded from the public allowlist. An 8192-byte JSON
   request with `{requestContext}` authenticates the short-lived context and
   checks `provider.Grant.find`. Success returns `allowed: true` and an
   `authorization` tuple: `principal`, `clientId`, `grantId`, `resource`, `scope`
   and separately sealed durable `grantContext`. The durable context has a grant
   purpose, no OAuth token and no authority independent of the current grant.
4. The same endpoint accepts `{authorization}` for background validity checks.
   It authenticates the durable context, compares every supplied tuple field and
   checks current provider subject, client and exact resource scope. Request and
   grant contexts cannot be substituted for one another. Missing, malformed,
   expired request contexts, mismatches, scope removal and revoked/missing grants
   deny with 403; an unexpected authority failure is unavailable, never allow.
5. The provider's native `revokeGrantPolicy` explicitly removes the whole grant
   on token/refresh revocation. Ordinary access-token expiry/removal is separate
   from grant revocation. Separate grants for the same operator remain valid
   independently; reconnecting does not revive the old grant.

This deliberately uses the existing trusted-loopback deployment. The sealed
context authenticates private checks; no shared backend key file, remote key
discovery, new login or generic identity framework was introduced. Untrusted
local processes still require OS/account isolation. Request contexts are never
persisted by Connect. Durable grant contexts and callback keys remain sensitive
private state, and are absent from diagnostics/errors.

## Implemented Connect contract

The official [OpenAI MCP Events guide](https://developers.openai.com/plugins/build/mcp-events)
was fetched on this checkpoint date. RMCP stays pinned at 3.4.0 and MCP stays
`2026-07-28`; Codex CLI/App Server pins and the existing 13 tools are unchanged.

At the HTTP boundary, Events calls resolve exactly one context header through the
fixed private ingress endpoint, with a three-second timeout, no redirects and no
proxy. A validated authorization object travels in RMCP request extensions.
Missing/forged/duplicate/unavailable contexts fail closed. Authenticated
`server/discover` responses project root `capabilities.events` through the existing
JSON response boundary, preserving RMCP validation. Trusted-local discovery
without context retains tools but advertises no Events. Event methods always
require the validated context.

RMCP `on_custom_request` implements `events/list`, `events/subscribe` and
`events/unsubscribe`. The sole definition is `codex.turn.terminal`, with required
exact canonical `threadId` and `turnId`; extra/wildcard filters are rejected.
Subscribe checks upstream thread existence, canonical identity and turn
association. It retains observation through the existing relay thread-subscription
owner. App Server's `mcpServer/event/stream/*` remains upstream event consumption
and is not used for webhook publication.

Every authoritative terminal observation, whether notification or read, passes
through `Relay::reconcile_terminal_turn`. Its compact terminal observer records
matching delivery bytes durably before callback I/O. A SHA-256 tuple of event
name/thread/turn supplies the stable logical event ID. Payload data contains only
canonical IDs and upstream `completed`, `interrupted` or `failed` status. Occurrence
time uses upstream `completedAt` when available, otherwise the authoritative
observation time. Full results remain available through `codex.inspect`.

Subscription identity hashes principal, exact callback URL, event name and fixed
canonical arguments; it does not include client or grant. Repeated subscribe is
idempotent and refreshes that identity. Explicit authenticated refresh can bind
it to the current valid grant; new grants never auto-resume old subscriptions.
Unsubscribe uses the original name/arguments/callback and current principal, so a
new valid grant can cancel a subscription whose prior stored grant is revoked.

A subscription is staged before upstream validation, so a terminal observation
during validation/challenge is retained but cannot dispatch before activation.
Validation/challenge failure, request cancellation or restart of unfinished
verification retires the staged work and removes keys. A successful subscription
requires a live grant immediately before verification and activation. A later
denial retires pending work permanently; authority outage pauses it visibly.

## Webhook and storage bounds

| Property | Implemented bound or policy |
| --- | --- |
| Subscriptions/outbox | 128 retained subscriptions, one logical terminal delivery slot each; capacity rejection and durable overflow counter |
| Lifetime | Omitted `ttlMs` defaults to one hour; finite requests are bounded to one minute through 24 hours; `ttlMs: null` grants no expiry and returns `refreshBefore: null` |
| Retention | Retired records pruned 24 hours after granted expiry, or after retirement when no expiry was granted; delivered slots deduplicate subsequent observations while retained |
| Body | One event per request; maximum 256 KiB; compact factual payload |
| Concurrency | One admitted outbound attempt; callback I/O runs outside relay notification handling |
| Network time | Ten-second overall callback attempt/challenge; five-second connect timeout; private ingress calls three seconds |
| Retry | At most eight consumed attempts; exponential 2/4/8/16/32/64/128-second delays between attempts; fresh signing timestamp/signature, stable bytes/ID |
| Nonretryable | Redirect responses and ordinary non-transient 4xx, including 410/413; 408/429 remain bounded transient retries |
| Verification | Fresh random, single-use challenge, signed control body, 2xx plus constant-time echo comparison; five-minute cache from successful verification |
| Rotation | Verification cache is keyed by authenticated principal and callback URL; replacement secrets reuse cached verification and use dual signing for five minutes; only one old key, further change during window rejected |
| Storage | Effective XDG state root `codex-connect/events/store.json`, mode 0600 inside 0700 directory; atomic replacement/fsync and exclusive process lock |
| Authorization | Before activation, verification, restart observation and every delivery attempt; denial stops, outage pauses, no allow cache |

Every verification/delivery attempt requires HTTPS and port 443, rejects URL
credentials/fragments and non-public literal IPs, resolves DNS at connection time,
rejects the entire answer set if any address is unsafe, and pins the connection
to those validated addresses while preserving hostname TLS verification. Proxies
and redirects are disabled. Signatures are Standard Webhooks HMAC-SHA256 over
`webhook-id.timestamp.` plus exactly the serialized body bytes. Secrets decode
from `whsec_` base64 to 24–64 bytes. Signing keys are removed on cancellation,
expiry, recognized revocation or verification failure.

The former CLI atomic-file helper and OS lock primitive now have one shared owner
in `codex-connect-host::storage`. CLI deployment retains its existing blocking
lock semantics; Events uses a nonblocking exclusive store lock. There is no new
database, scheduler, generic event bus or model runner.

On restart, live subscriptions begin paused and require current grant validity
before reattaching exact-turn observation through Relay. Current authority is
checked again after observation before any persisted delivery is admitted. Pending
bytes, IDs, attempt counts and deadlines survive; delivered/exhausted/cancelled/
expired/revoked entries never become pending again. Storage failure stops delivery
and exits the service visibly. `/observe` adds only redacted counts/states.

Cancellation and expiry serialize with admission. An already admitted request
may race subsequent ingress revocation or expiry; neither OAuth nor a callback
provides a distributed transaction. Cancellation acknowledgement follows an
already admitted attempt. A crash after receiver acceptance but before recording
receipt can repeat delivery; receivers must deduplicate the stable ID. There is
no protocol history replay: cursors are null, non-null replay requests are
rejected, and transitions missed while offline are not guaranteed. An explicit
authoritative terminal read can project a known fact using the same logical ID.
A 2xx means receipt, never completed ChatGPT processing.

## Feature evidence and repository gates

Verified gates so far:

- Host-ingress focused auth/gateway suite: 13 passed; full `./scripts/check source`:
  33 passed, including Caddy source configuration and unit validation.
- `cargo test --locked -p codex-connect-mcp events`: 20 passed.
- Pinned App Server schema check, format check and both repository diff checks:
  passed.
- `cargo test --workspace --locked`: 195 passed; doc tests passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed.
- `cargo build --locked -p codex-connect`: passed.
- `python3 tests/protocol_integration.py`: 61 passed in 328.769 seconds, including
  the integrated `writableRoots` forwarding and lifecycle rejection coverage.
- All 23 local Markdown file-link targets in changed canonical documents exist.
  Every pre-existing locked dependency version is retained; additions supply TLS
  HTTP delivery, HMAC/SHA-256 and timestamp formatting. No protocol/CLI pin upgrade.

No warnings or errors remain in completed validation. Focused fixtures exercise the real Caddy boundary and real RMCP HTTP custom
methods plus fake App Server terminal notification/read reconciliation. Callback
receipt fixtures test byte-exact signatures/challenges and delivery state; direct
production transport tests verify rejection of local DNS/private IP destinations.
These fixtures do not contact ChatGPT callbacks or deploy services.

## Changed files and integration ownership

Host-ingress: `auth/events-context.mjs`, `auth/server.mjs`, `lib/apps.mjs`,
`lib/routes.mjs`, `Caddyfile`, `tests/auth.test.mjs`, `tests/gateway.test.mjs`,
`README.md`.

Connect: `crates/mcp/src/events.rs` and `events/{ingress,webhook,tests}.rs`,
`crates/mcp/src/lib.rs`, `crates/mcp/Cargo.toml`, `Cargo.lock`,
`crates/relay/src/lib.rs`, `crates/host/src/storage.rs`, `crates/host/src/lib.rs`,
`crates/host/Cargo.toml`, `crates/cli/src/main.rs`, `crates/cli/src/deployment.rs`,
removal/move of `crates/cli/src/storage.rs`, and test-fixture XDG isolation in
`tests/protocol_integration.py`. Updated canonical docs: this checkpoint, proposal
status/history note, `SECURITY.md`, `docs/architecture/overview.md`,
`docs/operations.md`, `docs/development.md`. `docs/README.md` remains solely the
pre-existing modification.

The separately owned writable-roots worker is isolated in its detached checkout.
This worker did not touch that checkout or implement writableRoots. Integration
may overlap `crates/mcp/src/lib.rs`, `crates/relay/src/lib.rs`,
`tests/protocol_integration.py` and canonical docs. Events changes add a compact
terminal projection/watch method and custom dispatch; they do not alter catalog
or work-start sandbox mapping. The operator should merge these scopes deliberately
and run the combined gate afterward.

## Exact remaining acceptance steps

Each live step below needs separate authorization; none was performed here.

1. Review the final source diff and publish source only if separately instructed.
   Use ingress-owned and Connect-owned deployment procedures to deploy their
   changes separately. Verify exact installed/live builds, private context/check
   behavior, public OAuth denial and closed private paths. Source/tests/Git do not
   establish deployment health. Do not restart unrelated services.
2. Through authenticated MCP on the deployed route, verify only `2026-07-28`,
   root Events capability, sole terminal definition and canonical filters.
   Confirm missing/forged contexts and revoked grants fail closed. Verify live
   grants, disconnect, separate grants for the fixed user and outage/recovery.
3. Obtain plugin rescan authorization, rescan in ChatGPT and verify that the
   actual account/plugin page discovers the event. No account eligibility or
   ChatGPT wake-up behavior is inferred from platform documentation.
4. Start one separately authorized bounded test worker. In an existing ChatGPT
   chat instruct: “Monitor this thread and turn; when it ends, inspect the result
   and report the outcome. Do not start another worker.” Subscribe with its real
   canonical IDs. Record only non-secret worker/subscription/event IDs and
   expiration; verify challenge/persistence without exposing callback/key data.
5. End the initiating assistant turn before completion. Verify a later ChatGPT
   reply receives the factual event, calls `codex.inspect` and reports the result.
   Repeat with the browser closed and confirm after reopening. Webhook 2xx alone
   fails this acceptance requirement.
6. Use two instructed chats/callbacks on the same turn plus a negative thread/turn
   filter. Confirm independent delivery with the same logical ID, no negative
   delivery and no new logical event from repeated authoritative reads.
7. Under explicit restart/test authorization, exercise idempotent refresh,
   finite expiry, dual signing, cancellation during retries, pending restart
   recovery, ingress outage/recovery and grant revocation/disconnection. Verify
   old subscriptions do not revive on reconnect. Run controlled invalid signature,
   duplicate delivery and burst/batching cases without automatic host mutation.
8. Cancel test subscriptions and verify pending work is retired. Record source,
   tests, Git, deployed builds, plugin discovery, account behavior, post-turn and
   browser-closed outcomes separately. Record observed timing without claiming a
   latency guarantee, continuous supervision or usage-pool exemption.
