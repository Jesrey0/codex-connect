# Security

Report suspected vulnerabilities privately to the maintainers. Remove API keys, tunnel identifiers, account identifiers, and local paths from reports and logs.

Codex Connect is a **high-trust local execution bridge**. It intentionally has no application-level authentication of its own. The MCP listener is configuration-fenced to a loopback address, and the intended remote path is the official OpenAI Secure MCP Tunnel. Do not expose the MCP port directly to a LAN or the public internet, and do not modify the loopback-only validation without designing a real authentication and ingress model first.

This trust model is intended for a single-user host. Any local process that can reach the loopback MCP endpoint can invoke Codex Connect capabilities. On shared or untrusted multi-user machines, use OS-level isolation or a dedicated account instead of treating loopback as an authorization boundary.

The native `tunnel-client` owns its control-plane credentials and runtime state. Keep its runtime API key private and grant only the tunnel permissions required for operation. The ChatGPT custom app/connector uses **no authentication**; remote reachability is mediated by the OpenAI tunnel/control-plane configuration rather than a second credential layer in Codex Connect.

The durable host scope root fences direct file tools and official `cwd` / writable-root fields. It is not a system sandbox: commands, `danger-full-access` turns, network access, and `sudo` remain subject to the machine user's normal OS policy. The backend intentionally does not apply `NoNewPrivileges=true`; do not use a host account whose sudo policy is broader than the intended remote-operator trust.
