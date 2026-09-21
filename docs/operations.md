# Operations

Use [Getting Started](getting-started.md) for a fresh installation. This guide covers the steady-state owners, backend lifecycle, deployment, and recovery.

## Ownership

Codex Connect owns its backend service, user-global configuration/state/cache, deployment records, and installed content-addressed artifacts. Codex CLI/App Server and OpenAI `tunnel-client` are independent upstream dependencies. Their binaries, credentials, profiles, state, and lifecycle remain user-global and are not installed, relocated, duplicated, upgraded, deleted, or supervised by Codex Connect.

The backend listens on loopback at `127.0.0.1:8767/mcp`. It has no application-level authentication; Secure MCP Tunnel provides the remote path. Never expose the port directly.

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

## Tunnel lifecycle

Manage the native tunnel independently:

```bash
tunnel-client runtimes status codex-connect --json
# use the repair_command reported by tunnel-client when needed
```

The tunnel owns its runtime API key, organization context, profile, reconnect behavior, and native state. Do not create a second systemd tunnel service or duplicate profile. Backend `restart`, deployment, and uninstall do not manage the tunnel. Backend deployment is not connector refresh; rediscover/refresh the ChatGPT app separately when its tool catalog needs updating.

## Restart recovery

After reboot, verify the two owners separately:

```bash
codex-connect status
codex-connect doctor
tunnel-client runtimes status codex-connect --json
```

Restart the backend only if its checks fail. If the tunnel runtime is stopped, use its reported native repair command and recheck status. Do not rerun installation or recreate the profile merely because the computer restarted.

## Deployment

Deployment is a two-phase, backend-only workflow:

```bash
codex-connect deploy prepare
codex-connect deploy status <operation-id>
codex-connect deploy activate <operation-id>
codex-connect deploy status <operation-id>
```

`prepare` queues a detached release build and records a durable operation. Wait for `prepared`; `activate` queues the backend restart; the final `status` verifies the exact prepared artifact is live. The deployment build cache at `~/.cache/codex-connect/deploy/build` is a compiler cache, not runtime authority. Deployment does not restart the tunnel or refresh the connector.

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

Uninstall removes Codex Connect's service, configuration, state/cache, operator symlink, and installed artifacts. It leaves the source tree, Codex CLI/App Server, and tunnel-client state untouched.

For security implications, see [Security](../SECURITY.md).
