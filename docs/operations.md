# Operations

Use [Getting Started](getting-started.md) for a fresh installation. This guide covers the steady-state owners, backend lifecycle, deployment, and recovery.

## Ownership

Codex Connect owns its backend service, user-global configuration/state/cache, deployment records, and installed content-addressed artifacts. Codex CLI/App Server and host ingress have independent user-global lifecycles. Host ingress is the sole owner of public routing and OAuth.

The backend listens on loopback at `127.0.0.1:8767/mcp`. Public calls use `${NGROK_URL}/codex-connect/mcp` through ngrok and the OAuth-protected host ingress. Never expose the backend port directly.

## Backend lifecycle

Initial setup is documented in [Getting Started](getting-started.md). Routine commands are:

```bash
codex-connect status
codex-connect doctor
codex-connect restart
codex-connect logs --follow
codex-connect console
codex-connect probe --codex-bin "$(command -v codex)" --cwd ~/src/example-project
```

`status` is concise runtime/readiness/orientation: readiness, live build identity, navigation cwd, and Codex default provenance. `console` is a read-only event-driven human projection of workers, quota, pending actions, and selected transcripts; it hydrates once, then waits for observer changes rather than polling. Its local UI clock only redraws animation/countdowns. Quota refresh is triggered by observer launch and worker/message lifecycle boundaries and is independent from the worker projection. `doctor` is the detailed local diagnostic. PTY command sessions are controlled through the MCP `command.start/read/control` tools.

## Ingress lifecycle

Verify ingress independently:

```bash
# from the host-ingress checkout
./scripts/status
CHECK_PUBLIC=1 ./scripts/check
```

Host ingress owns `host-ngrok`, `host-ingress` (Caddy), and `host-oauth` services, their startup ordering, public URL, credentials, and reconnect behavior. Codex Connect readiness describes the local backend; it does not assert public reachability or a valid ChatGPT connection. Backend restart/deployment/uninstall do not manage ingress. Rediscover/refresh the ChatGPT app separately when its tool catalog changes.

### Secondary fallback

OpenAI Secure MCP Tunnel may be retained as an explicitly secondary recovery path. Its process, profile, credentials, reconnect behavior, and native state are upstream-owned and must stay outside the Codex Connect source/config tree. Normal operation uses ngrok through host ingress; keep the fallback inactive unless needed and operate it only through native `tunnel-client` lifecycle commands. Backend health, deployment, timeout behavior, and ingress readiness never depend on it.

## Restart recovery

After reboot, verify backend and ingress separately:

```bash
codex-connect status
codex-connect doctor
# from the host-ingress checkout
CHECK_PUBLIC=1 ./scripts/check
```

Restart only the failed service. User-systemd services are enabled independently; ngrok wants Caddy, and Caddy wants the OAuth service. OAuth grants/refresh state survive restarts. MCP clients initialize a new session after backend restart. Verify a real authenticated call after recovery rather than rerunning setup.

## Deployment

Deployment is a two-phase, backend-only workflow:

```bash
codex-connect deploy prepare
codex-connect deploy status <operation-id>
codex-connect deploy activate <operation-id>
codex-connect deploy status <operation-id>
```

`prepare` queues a detached release build and records a durable operation. Wait for `prepared`; `activate` queues the backend restart; the final `status` verifies the exact prepared artifact is live. The deployment build cache at `~/.cache/codex-connect/deploy/build` is a compiler cache, not runtime authority. Deployment does not restart ingress or refresh the connector.

The states are deliberately distinct: `SourceChanged != Committed != Pushed != Deployed != Live != CIGreen`. Git commits/pushes are operator workflow and are never implied by deployment. Verify source, Git, deployment, service/build identity, connector discovery, and CI at their respective owners.

## Configuration and uninstall

The canonical configuration is small:

```toml
[workspace]
default_cwd = "~"

[backend]
listen = "127.0.0.1:8767"
codex_bin = "/home/you/.local/bin/codex"
```

`default_cwd` is navigation/startup context only. Worker defaults may be reported by `status` with provenance `userConfig` or `upstream`; worker instruction sources remain the normal Codex config and AGENTS.md chain.

```bash
codex-connect uninstall
```

Uninstall removes Codex Connect's service, configuration, state/cache, operator symlink, and installed artifacts. It leaves the source tree, Codex CLI/App Server, and host ingress state untouched.

For security implications, see [Security](../SECURITY.md).
