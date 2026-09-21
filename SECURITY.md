# Security

Codex Connect is a high-trust local execution bridge. A compromise or unsafe configuration may be equivalent to code execution as the OS account running the backend.

## Reporting

Use GitHub **Security → Report a vulnerability** when private reporting is available:

<https://github.com/Jesrey0/codex-connect/security/advisories/new>

If it is unavailable, open only a minimal public issue requesting a private channel. Do not include exploit details, credentials, tunnel IDs, account identifiers, private source, or sensitive paths.

## Trust boundary

The MCP listener is loopback-only and has no application-level authentication. Any local process able to reach it can invoke HostPlane and WorkerPlane capabilities. Do not expose port 8767 to a LAN or the internet. On shared or untrusted hosts, use OS isolation or a dedicated account.

ChatGPT is the primary technical operator. HostPlane intentionally uses the OS account's host filesystem/process authority; `default_cwd` is navigation, not an authorization boundary. The dedicated App Server has a process-local `danger-full-access` launch override. WorkerPlane is separately bounded: omitted work `access` uses the canonical writable workspace sandbox, while `access="full"` selects `danger-full-access`; reviews are read-only. `approvalPolicy="never"` prevents mechanical approval stalls but does not expand the sandbox.

Secure MCP Tunnel mediates remote reachability and owns its control-plane credentials and runtime state. The ChatGPT connector uses no application credential. Keep tunnel keys out of project files, logs, issues, and connector configuration. Codex CLI/App Server and tunnel-client are independent dependencies; Codex Connect must not supervise or rewrite their state.

Do not use an OS account whose sudo or filesystem privileges exceed what you intend the remote operator to exercise. The backend intentionally does not apply `NoNewPrivileges=true`.

## Operational guidance

- Keep the backend bound to loopback and use the official Secure MCP Tunnel.
- Run `codex-connect doctor` after installation or upgrades.
- Verify the exact live build and navigation cwd with `status`; verify the tunnel separately with `tunnel-client runtimes status`.
- Treat HostPlane, WorkerPlane, and PlatformPlane permissions as non-transitive.
- Use `codex-connect uninstall` for Codex Connect state; remove tunnel-client state through tunnel-client if needed.
