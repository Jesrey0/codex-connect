# Security

Codex Connect is a **high-trust local execution bridge**. Treat a successful compromise or unsafe configuration as potentially equivalent to code execution under the OS account running the backend.

## Reporting a vulnerability

Use the repository's **Security → Report a vulnerability** flow when GitHub Private Vulnerability Reporting is available:

`https://github.com/Jesrey0/codex-connect/security/advisories/new`

If that private-reporting control is unavailable, open a minimal public issue asking the maintainer for a private contact channel **without including vulnerability details**. Do not post proof-of-concept code, credentials, tunnel identifiers, account identifiers, private source, or sensitive local paths publicly.

## Trust boundary

Codex Connect intentionally has no application-level authentication of its own. The MCP listener is configuration-fenced to a loopback address, and the intended remote path is the official OpenAI Secure MCP Tunnel. Do not expose the MCP port directly to a LAN or the public internet, and do not modify the loopback-only validation without designing a real authentication and ingress model first.

This trust model is intended for a single-user host. Any local process that can reach the loopback MCP endpoint can invoke Codex Connect capabilities. On shared or untrusted multi-user machines, use OS-level isolation or a dedicated account instead of treating loopback as an authorization boundary.

The native `tunnel-client` owns its control-plane credentials and runtime state. Keep its runtime API key private and grant only the tunnel permissions required for operation. The ChatGPT custom app/connector uses **no authentication**; remote reachability is mediated by the OpenAI tunnel/control-plane configuration rather than a second credential layer in Codex Connect.

The primary operator plane intentionally has the host user's filesystem/process authority. Direct host tools accept absolute paths, and the dedicated App Server is launched with a process-local `danger-full-access` sandbox default; the configured default workspace is navigation only, not an authorization fence. Delegated Codex work remains separately policy-bounded per turn, and new official review threads are read-only. The backend intentionally does not apply `NoNewPrivileges=true`; do not use a host account whose sudo policy is broader than the intended remote-operator trust.

## Operational guidance

- Keep the MCP backend bound to loopback.
- Use OpenAI Secure MCP Tunnel rather than exposing the backend through an ad-hoc public tunnel.
- Keep tunnel runtime credentials out of project files, logs, bug reports, and ChatGPT connector configuration.
- Treat connector/tunnel access as equivalent to high-trust host-operator access and keep the backend loopback-only.
- Run `codex-connect doctor` after installation or upgrades and treat unexpected default-workspace/build identity as a failure.
- Use `codex-connect uninstall` when removing the backend; tunnel-client state remains independently managed and must be removed separately if no longer needed.
