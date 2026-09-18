# Operations

For first-time installation, prerequisites, Secure MCP Tunnel, and ChatGPT custom app setup, start with [Getting Started](getting-started.md). This document covers steady-state operation after installation.

## Ownership boundary

Codex Connect is downstream of both the user-global Codex CLI/App Server and the user-global OpenAI `tunnel-client`. Codex Connect owns only its own backend, workspace-scoped configuration, deployment state, and installed backend artifacts. It must not install, relocate, duplicate, upgrade, delete, or supervise either upstream dependency or its owned configuration/state.

- Codex CLI remains outside `~/projects`, with Codex-owned state/configuration in the normal `~/.codex` home unless the user explicitly selects another global location.
- `tunnel-client` remains outside `~/projects`; this guide uses `~/.config/tunnel-client` for profiles and `~/.local/state/tunnel-client` for native runtime state.
- Workspace `XDG_CONFIG_HOME` / `XDG_STATE_HOME` overrides must not be used to relocate tunnel-client-owned state under `~/projects`.

## Install

```bash
bootstrap_target=target/codex-connect-bootstrap
rm -rf "$bootstrap_target"
CARGO_TARGET_DIR="$bootstrap_target" cargo build --release -p codex-connect --locked
"$bootstrap_target/release/codex-connect" setup
rm -rf "$bootstrap_target"
export PATH="$HOME/projects/.local/bin:$PATH"
codex-connect doctor
codex-connect status
```

`setup` converges the bootstrap binary into the canonical workspace-local runtime layout: one content-addressed artifact under `~/projects/.local/lib/codex-connect/builds/`, `~/projects/.local/bin/codex-connect` as the operator symlink, workspace configuration under `~/projects/.config/codex-connect/`, workspace deployment state under `~/projects/.local/state/codex-connect/`, and `codex-connect.service` pointing at that artifact. The temporary `target/codex-connect-bootstrap/` tree is build-only and should be deleted after setup. Steady-state deployments may retain `target/codex-connect-deploy/build/` as a compiler cache, but no executable under `target/` is runtime authority. The user systemd manager may retain its registration file under its native user-unit directory; that registration is not application-state authority.

Configure the user-global official tunnel client separately to connect its long-lived runtime to `http://127.0.0.1:8767/mcp`. The ChatGPT custom app uses **no authentication**. The tunnel runtime authenticates to OpenAI with its own runtime API key; Codex Connect's MCP endpoint is deliberately loopback-only and has no application-level authentication. Use the tunnel client's native lifecycle commands:

```bash
tunnel-client runtimes connect ...
tunnel-client runtimes status <alias>
```

The canonical tunnel-client profile for this guide is `~/.config/tunnel-client/codex-connect.yaml`, with native runtime state under `~/.local/state/tunnel-client`. It remains tunnel-client-owned; Codex Connect does not read or manage the tunnel ID or its runtime lifecycle.

The exact connection parameters and credentials remain tunnel-client-owned. `codex-connect restart` restarts only the backend and intentionally leaves the native tunnel runtime alone.

For an organization-scoped tunnel, keep the runtime key and organization context available before connecting or repairing the native runtime:

```bash
export CONTROL_PLANE_API_KEY='...'
export CONTROL_PLANE_ORGANIZATION_ID='org_...'
```

The organization variable is sent by `tunnel-client` as the `OpenAI-Organization` header. It is separate from the `--organization-id` lookup scope used by `tunnel-client runtimes connect`.

## Backend commands

`setup` is installation/configuration. Normal operation uses `status`, `restart`, `doctor`, and `logs`:

```bash
codex-connect status
codex-connect restart
codex-connect doctor
codex-connect logs --follow
codex-connect probe --codex-bin "$(command -v codex)" --cwd ~/projects/example-project
```

`codex-connect restart` also enables the backend service if it was installed but disabled, so a successful recovery restores the next-boot invariant.

## Uninstall

Remove only the state owned by Codex Connect with:

```bash
codex-connect uninstall
```

The command stops/disables and removes the persistent backend service, reloads the user systemd manager, removes Codex Connect configuration and deployment state, removes the workspace-local operator symlink, and removes installed content-addressed Codex Connect builds.

The source tree, Codex CLI, and all `tunnel-client` state are intentionally left untouched. If the corresponding ChatGPT tunnel is no longer needed, stop/remove that runtime or profile separately with `tunnel-client`; backend uninstall must not become tunnel lifecycle orchestration.

## After a computer restart

The backend and tunnel are separate failure domains. Do not rerun installation just because the machine restarted.

```bash
codex-connect status
codex-connect doctor
```

`codex-connect.service` should already be enabled. If the backend is stopped or unhealthy, run `codex-connect restart` and recheck it.

For the tunnel, export the runtime API key required by the saved profile and inspect the existing alias:

```bash
export CONTROL_PLANE_API_KEY='...'
export CONTROL_PLANE_ORGANIZATION_ID='org_...'
tunnel-client runtimes status codex-connect --json
```

If the native runtime is stopped, execute the `repair_command` returned by `runtimes status`, then run the status command again. The repair command is tunnel-client-owned and is derived from the saved alias/profile state, so it is preferable to reconstructing account-specific flags by hand.

The canonical profile remains `~/.config/tunnel-client/codex-connect.yaml`. Do not recreate the profile, invent a second alias, or add a systemd tunnel unit for routine reboot recovery.

## Recovery

Check the backend and tunnel independently before changing anything:

```bash
codex-connect status
codex-connect doctor
tunnel-client runtimes status <alias> --json
```

For a failed backend, inspect `codex-connect logs`, then use `codex-connect restart` and rerun `doctor`.

For a native tunnel runtime that is stopped or not ready, use its `tunnel-client runtimes` lifecycle command and recheck its status.

Do not add a per-client backend or systemd tunnel unit. The only persistent Codex Connect unit is `codex-connect.service`; the official tunnel client owns its own runtime lifecycle.

## Configuration

The configuration at `~/projects/.config/codex-connect/config.toml` is deliberately small:

```toml
[scope]
root = "~/projects"

[backend]
listen = "127.0.0.1:8767"
codex_bin = "/home/you/.local/bin/codex"
```

`setup` persists an absolute executable path for the user-global Codex CLI. It first reuses a valid configured executable and otherwise resolves `codex` from `PATH`. The exact absolute path depends on the user's installation method (for example npm under NVM may live beneath `~/.nvm`). Codex Connect does not install or prefer a workspace-local Codex binary. This is the canonical configuration shape for the pre-release backend. Historical configuration forms are not retained.

### Host sandbox source of truth

**Operational invariant:** Codex Connect does not synthesize a default `sandboxPolicy` for host commands. When `command.exec` or `command.start` omits that field, Codex Connect also omits it on the official App Server request. App Server therefore uses the effective configuration already loaded from `$CODEX_HOME/config.toml` (normally `~/.codex/config.toml`).

Sandbox defaults are intentionally not duplicated in Codex Connect configuration. For a workspace-write host baseline with network access, configure Codex itself:

```toml
sandbox_mode = "workspace-write"

[sandbox_workspace_write]
network_access = true
```

`codex.work.start` is different by design: its public MCP contract requires an explicit `sandboxPolicy` for every turn, so delegated agent authority is selected by the operator at task start rather than inherited from that host default.

## Deployment boundary

Deployment is deliberately two-phase so an operator invoking the CLI through the running backend never has to interpret a self-inflicted transport disconnect as command failure.

```bash
codex-connect deploy prepare
codex-connect deploy status <operation-id>
codex-connect deploy activate <operation-id>
codex-connect deploy status <operation-id>
```

`deploy prepare` is a short enqueue operation: it creates a versioned durable record under the user state directory, records the source tree, assigns an operation id, and hands the potentially long release build to a detached systemd unit. The build itself requires neither Git nor a clean working tree. Deployment builds are serialized by a deployment-wide build lock and reuse one persistent Cargo release target at `target/codex-connect-deploy/build`; that directory is only a compilation cache and is never deployment authority. The completed binary is installed as a content-addressed artifact, and the operation records `prepared` or `failed` without touching the running backend. `deploy activate` is also short: an operation-scoped OS lock serializes competing callers, the prepared artifact is validated, `activationQueued` is persisted, and the detached activation is handed to systemd before the foreground command returns. If handoff fails, the record is rolled back to `prepared`; if a detached worker later disappears without recording completion, `deploy status` reconciles that condition to `succeeded` only when the exact runtime and operator artifact prove activation completed, otherwise to `failed`. A `prepared` operation is likewise reconciled to `failed` if its recorded artifact is missing or no longer matches its SHA-256. Successful normal activation is not considered verified until the live backend reports the exact prepared SHA-256. Installed content-addressed artifacts are retained rather than automatically deleted, so activating one durable operation cannot invalidate another prepared operation.

That boundary is intentional. Backend deployment must not become tunnel lifecycle orchestration.

## Security boundary

Codex Connect is a high-trust host execution bridge. Scope fencing protects direct filesystem operations and establishes allowed starting paths; it does not sandbox the whole machine. Review [Security](../SECURITY.md) before changing ingress, scope, command execution, or service privileges.
