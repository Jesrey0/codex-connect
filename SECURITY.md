# Security

Codex Connect is a high-trust local execution bridge. A compromise or unsafe exposure may be equivalent to code execution as the OS account running the backend.

## Reporting

Use GitHub **Security → Report a vulnerability** when private reporting is available:

<https://github.com/Jesrey0/codex-connect/security/advisories/new>

If it is unavailable, open only a minimal public issue requesting a private channel. Do not include exploit details, credentials, account identifiers, private source, or sensitive paths.

## Trust boundary

The MCP listener is loopback-only and has no application-level authentication. Any local process able to reach it can invoke HostPlane and WorkerPlane capabilities. Do not expose port 8767 to a LAN or the internet. On shared or untrusted hosts, use OS isolation or a dedicated account.

ChatGPT is the only supported action/control interface and the primary technical operator. The local console is read-only observability and must not mutate worker, host, or App Server state. HostPlane intentionally uses the OS account's host filesystem/process authority; the managed backend's home-directory cwd is navigation, not an authorization boundary. The dedicated App Server has a process-local `danger-full-access` launch override. WorkerPlane is separately bounded: omitted work `access` uses the canonical writable workspace sandbox, while `access="full"` selects `danger-full-access`; reviews are read-only. `approvalPolicy="never"` prevents mechanical approval stalls but does not expand the sandbox.

The canonical remote boundary is ngrok HTTPS → host ingress → loopback MCP. ngrok owns public TLS. Host ingress owns routing and OAuth authorization: ChatGPT authorization-code flow with S256 PKCE, signed client assertions, and resource-bound Bearer tokens. The human operator authenticates and consents at ingress. Missing, invalid, expired, wrong-audience, or insufficient-scope tokens must be rejected before reaching Codex Connect. Public health/status and internal authentication endpoints remain closed.

Host ingress owns OAuth signing keys, the operator login, token/grant persistence, refresh, revocation, and credential-safe logging. Keep credentials out of project files, logs, issues, and MCP URLs. Codex CLI/App Server and ingress have independent lifecycles; Codex Connect must not rewrite their state. Local maintenance commands and loopback diagnostics are trusted-host administration, not an alternate remote user interface. See the [host-ingress security contract](https://github.com/Jesrey0/host-ingress).

Do not use an OS account whose sudo or filesystem privileges exceed what you intend the remote operator to exercise. The backend intentionally does not apply `NoNewPrivileges=true`.

## Operational guidance

- Keep the backend bound to loopback. The public route is the authenticated host-ingress path.
- Run `codex-connect doctor` after installation or upgrades.
- Verify the exact live build and navigation cwd with `status`; verify public routing, OAuth, and the ChatGPT connection independently.
- Treat HostPlane, WorkerPlane, and PlatformPlane permissions as non-transitive.
- Use `codex-connect uninstall` for Codex Connect state; retire the public route and OAuth grants through host ingress.
