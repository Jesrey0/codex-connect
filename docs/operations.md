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

`status` reports readiness, live build identity, navigation cwd, and Codex defaults. `doctor` provides local diagnostics. `console` follows worker activity, quota, pending actions, and transcripts without changing worker state.

## Host commands

Use `command.exec` for short, non-interactive commands and `command.start` for
long-running or interactive work. Commands take argv; invoke a shell explicitly
for pipes, redirects, or expansion. Set `tty=true` only when a terminal is needed.

After `command.start`, retain `processId` and pass each returned cursor to
`command.read`. `timeoutMs=0` reads immediately; otherwise the read waits for output
or exit. Timeout does not terminate the process. Continue until `drained=true` for
final output; `historyLost=true` means older output was evicted. Termination can be
forceful: confirm exit with `command.read`. Process handles belong to the backend's
App Server connection and do not survive its restart.

## Worker lifecycle

Supply `codex.start` with a self-contained task, host paths, constraints, and
acceptance criteria. Use `codex.info` to discover model IDs and supported effort.
Work can resume a thread and override model, effort, and access for that turn;
review retains an existing thread's settings and rejects a model override.

Retain `threadId` and `turnId`. Continue only non-overlapping operator work, then
call `codex.wait` with those IDs. It uses a fixed server wait and returns:

- `terminal`: read the turn status, output, and error.
- `actionRequired` or `inputRequired`: respond with `codex.action.respond`, matching
  the pending kind, requestId, and requested answers or decisions; then wait again.
- `timeout`: the worker is active. Continue waiting or inspect activity; timeout
  does not establish a stall or authorize taking over its scope.

Use `codex.inspect` and its cursor for activity/history, `codex.control` to steer
or interrupt, and `codex.wait` to confirm terminal state after interruption.

A lost `codex.start` response does not cancel creation. Before retrying, check a
host tool response (for example `status`) for a one-shot `workerStarted` event and
save its IDs. `workerEvents` can also report completion or required action. Their
absence provides no worker status; `historyLost` means notifications are missing. After a
backend restart, use retained thread/turn IDs to reconcile through `codex.wait`.

## Ingress lifecycle

Verify ingress independently:

```bash
# from the host-ingress checkout
./scripts/status
CHECK_PUBLIC=1 ./scripts/check
```

Host ingress owns `host-ngrok`, `host-ingress` (Caddy), and `host-oauth` services, their startup ordering, public URL, credentials, and reconnect behavior. Codex Connect readiness describes the local backend; it does not assert public reachability or a valid ChatGPT connection. Backend restart/deployment/uninstall do not manage ingress. Rediscover/refresh the ChatGPT app separately when its tool catalog changes.

### Secondary fallback

An independently configured OpenAI Secure MCP Tunnel can provide recovery access. Manage it with `tunnel-client`, outside Connect configuration/state, and keep it inactive unless needed. Backend health and deployment do not manage it.

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

Verify source, Git, prepared artifact, live build identity, connector discovery, and CI separately. Deployment does not commit or push.

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
