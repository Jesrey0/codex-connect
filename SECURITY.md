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

The durable host scope root fences direct file tools and official `cwd` / writable-root fields. It is not a system-wide sandbox: commands, `danger-full-access` turns, network access, and `sudo` remain subject to Codex sandbox policy and the machine user's normal OS permissions. The backend intentionally does not apply `NoNewPrivileges=true`; do not use a host account whose sudo policy is broader than the intended remote-operator trust.

## Operational guidance

- Keep the MCP backend bound to loopback.
- Use OpenAI Secure MCP Tunnel rather than exposing the backend through an ad-hoc public tunnel.
- Keep tunnel runtime credentials out of project files, logs, bug reports, and ChatGPT connector configuration.
- Review the effective Codex sandbox configuration before allowing host command execution.
- Run `codex-connect doctor` after installation or upgrades and treat unexpected scope/build identity as a failure.
- Use `codex-connect uninstall` when removing the backend; tunnel-client state remains independently managed and must be removed separately if no longer needed.
