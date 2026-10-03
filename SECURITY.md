# Security

Codex Connect is a high-trust local execution bridge. A compromise or unsafe exposure may be equivalent to code execution as the OS account running the backend.

## Reporting

Use GitHub **Security → Report a vulnerability** when private reporting is available:

<https://github.com/Jesrey0/codex-connect/security/advisories/new>

If it is unavailable, open only a minimal public issue requesting a private channel. Do not include exploit details, credentials, account identifiers, private source, or sensitive paths.

## Trust boundary

The MCP listener is loopback-only and has no application-level authentication. Any local process able to reach it can invoke HostPlane and WorkerPlane capabilities. Do not expose port 8767 to a LAN or the internet. On shared or untrusted hosts, use OS isolation or a dedicated account.

ChatGPT is the only supported action/control interface and the primary technical operator. The local console is read-only observability and must not mutate worker, host, or App Server state. HostPlane intentionally uses the OS account's host filesystem/process authority; the managed backend's home-directory cwd is navigation, not an authorization boundary. The dedicated App Server has a process-local `danger-full-access` launch override. WorkerPlane is separately bounded: omitted work `access` uses the canonical writable workspace sandbox, while `access="full"` selects `danger-full-access`; reviews are read-only. `approvalPolicy="never"` prevents mechanical approval stalls but does not expand the sandbox.

Multiple ChatGPT operators may use the same backend concurrently, but this is not a multi-tenant security boundary. They share the same trusted OS account and backend-global recovery state; Codex Connect does not isolate workers, retained commands, or host authority by ChatGPT conversation, account, session, or project. Only connect identities that belong inside the same trust domain.

Fresh workspace workers may explicitly receive additional absolute write directories through `writableRoots`; cwd remains their primary working directory. The roots use upstream sandbox semantics, including its protected paths and temporary-directory defaults. Full access and reviews do not accept this field. Resume and fork reject root overrides at the Connect boundary. Pinned Codex 0.160.0 restores its separate native `runtimeWorkspaceRoots` state on cold resume, but Connect's additional roots are forwarded through legacy `sandboxPolicy` and are not covered by that restoration path. Do not rely on additional Connect write roots surviving a cold reload or fork; see the [pinned persistence details](docs/development.md#writable-root-persistence).

The canonical remote boundary is ngrok HTTPS → host ingress → loopback MCP. ngrok owns public TLS. Host ingress owns routing and OAuth authorization: ChatGPT authorization-code flow with S256 PKCE, signed client assertions, and resource-bound Bearer tokens. The human operator authenticates and consents at ingress. Missing, invalid, expired, wrong-audience, or insufficient-scope tokens must be rejected before reaching Codex Connect. Public health/status and internal authentication endpoints remain closed.

Host ingress owns OAuth signing keys, the operator login, token/grant persistence, refresh, revocation, and credential-safe logging. Keep credentials out of project files, logs, issues, and MCP URLs. Codex CLI/App Server and ingress have independent lifecycles; Codex Connect must not rewrite their state. Local maintenance commands and loopback diagnostics are trusted-host administration, not an alternate remote user interface. See the [host-ingress security contract](https://github.com/Jesrey0/host-ingress).

Do not use an OS account whose sudo or filesystem privileges exceed what you intend the remote operator to exercise. The backend intentionally does not apply `NoNewPrivileges=true`.

## MCP Events authorization and outbound delivery

Events require an ingress-authenticated authorization object. Caddy deletes caller
context headers and forwards only an ingress-encrypted, short-lived request
context. Connect resolves it at ingress's fixed private loopback endpoint; it
accepts no caller-selected authority, key or issuer. Discovery advertises root
`capabilities.events` only after this check; all event methods require it. The
existing trusted-local tool surface remains unchanged.

Subscriptions persist the principal/client/grant/resource/scope tuple and an
opaque authenticated grant context. Host ingress checks that context and its
current provider grant before activation, callback verification, recovery and
each delivery. Connect stores no OAuth access/refresh token or request assertion,
reads no ingress database, and creates no user, tenant, worker or chat ownership.
Separate grants for the same fixed user are independent. A denial retires pending
work; an authority outage pauses it. A valid refreshed request may explicitly
rebind the same subscription identity to its current grant. Cancellation uses the
current principal and original identity, so it can cancel an old revoked grant's
subscription. Ordinary access-token expiry is independent of subscription expiry.

Callbacks require HTTPS, public unicast DNS answers and port 443. Every attempt,
including verification, resolves and validates all addresses and pins the actual
connection to those addresses while retaining hostname TLS verification. There
are no proxies or redirects. Verification uses a fresh random challenge, signed
body and constant-time echo comparison before activation. Delivery signs exactly
the serialized bytes with Standard Webhooks HMAC-SHA256. Secrets and contexts are
excluded from diagnostics and errors; private state is mode 0600 inside a 0700
directory with one process holding its OS lock. Key rotation retains one old key
for at most five minutes and removes keys on cancellation, expiry or revocation.

Admission is serialized with cancellation and expiry. An already admitted request
may race a later ingress revocation or expiry; there is no distributed transaction
with the OAuth provider or receiver. Cancellation acknowledgement follows any
in-flight admitted attempt. Durable pending deliveries retain IDs and attempt
counts through restart; delivery may repeat after a crash, and receivers must
deduplicate. Connect does not promise capture while offline or history replay.

## Operational guidance

- Keep the backend bound to loopback. The public route is the authenticated host-ingress path.
- Run `codex-connect doctor` after installation or upgrades.
- Verify the exact live build and default cwd with `status`; verify public routing, OAuth, and the ChatGPT connection independently.
- Treat HostPlane, WorkerPlane, and PlatformPlane permissions as non-transitive.
- Use `codex-connect uninstall` for Codex Connect state; retire the public route and OAuth grants through host ingress.
