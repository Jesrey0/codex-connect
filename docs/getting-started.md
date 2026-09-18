# Getting Started

This is the shortest supported path from a fresh Linux host to a working Codex Connect app in ChatGPT.

The target success condition is simple:

> From ChatGPT, `@codexConnect` can call `status` and receive the live backend/App Server status from your machine.

## Current support boundary

Codex Connect's managed setup currently targets **Linux with systemd user services**. The underlying Codex CLI and Secure MCP Tunnel support additional platforms, but `codex-connect setup` uses a systemd user service today.

The pre-release installation layout is intentionally opinionated: `~/projects` is the default host scope, and the managed operator symlink/build store live under `~/projects/.local`. Configuration and deployment state default to `~/projects/.config` and `~/projects/.local/state` respectively, while honoring explicit `XDG_CONFIG_HOME` / `XDG_STATE_HOME`. You can change the configured host scope after setup; doing so does not relocate the managed operator/build store.

### ChatGPT plan scope

This pre-release project currently documents **paid ChatGPT accounts only**.

- The maintainer has verified the end-to-end Codex Connect flow on **ChatGPT Plus**.
- Higher paid tiers are expected to work, but have not all been independently validated by this project.
- The Free tier has not been tested, so this guide makes no claim about it.
- OpenAI's public developer-mode documentation currently describes plan-specific MCP availability differently (including full MCP for Business/Enterprise/Edu and read/fetch MCP access for Pro). Feature availability and UI can therefore be account- or rollout-dependent. If Developer mode or custom app/connector creation is absent from your account, stop there rather than inventing a workaround.

Treat the first bullet as this project's observed behavior, not as an official OpenAI entitlement statement.

## What you need

Before obtaining the source tree, you need:

- a Linux machine with `systemctl --user` available;
- Rust **1.85 or newer** and Cargo;
- the official OpenAI Codex CLI release pinned by this project;
- an authenticated Codex CLI session;
- the official OpenAI `tunnel-client` for Secure MCP Tunnel;
- access to OpenAI Tunnels management and a runtime API key with **Tunnels Read + Use** for the tunnel;
- a paid ChatGPT account on which Developer mode/custom MCP app creation is available.

The tunnel keeps the MCP backend on localhost. Do **not** expose port `8767` directly to the public internet.

## 1. Obtain the source and check the host

Download and extract the Codex Connect source archive, or clone it with Git if you prefer a contributor workflow. Git is not required to build, deploy, or operate the backend.

```bash
cd codex-connect
./scripts/check-prerequisites.sh
```

The checker is intentionally read-only. On a fresh machine it may report missing Codex or tunnel-client; install them in the next steps and rerun it.

Fast builds (optional): for the mold linker plus the sccache cache, copy
`.cargo/config.toml.example` to `.cargo/config.toml` with your home substituted
(the example header shows the one-line sed). Plain `cargo build` works without it.

## 2. Install the pinned Codex CLI

Codex Connect generates and validates its forwarded App Server schemas against the exact CLI release in `config/codex-cli-pin`. Do not silently substitute another release.

The official Codex project supports the standalone installer, npm, Homebrew, and release binaries. For an exact project-pinned install, npm is convenient when Node/npm is already present:

```bash
CODEX_PIN="$(cat config/codex-cli-pin)"
npm install -g "@openai/codex@${CODEX_PIN}"
codex --version
```

The release printed by `codex --version` must match `config/codex-cli-pin`.

If you do not use npm, install the matching release from the official Codex releases page instead:

- https://github.com/openai/codex/releases

Then authenticate interactively:

```bash
codex
```

Choose **Sign in with ChatGPT** when prompted. The official Codex project documents ChatGPT-plan login for Plus, Pro, Business, Edu, and Enterprise.

## 3. Install Secure MCP Tunnel

ChatGPT does not connect directly to a localhost MCP server. Use OpenAI's **Secure MCP Tunnel** rather than an ad-hoc public tunnel.

Get the supported `tunnel-client` from OpenAI's Tunnels management page or the official repository/release page:

- https://platform.openai.com/settings/organization/tunnels
- https://github.com/openai/tunnel-client
- https://github.com/openai/tunnel-client/releases/latest

After installation:

```bash
tunnel-client --version
tunnel-client help quickstart
```

The tunnel client is independently owned. Codex Connect does not install it, create its credentials, or supervise its lifecycle.

## 4. Build and install Codex Connect

```bash
bootstrap_target=target/codex-connect-bootstrap
rm -rf "$bootstrap_target"
CARGO_TARGET_DIR="$bootstrap_target" cargo build --release -p codex-connect --locked
"$bootstrap_target/release/codex-connect" setup
rm -rf "$bootstrap_target"
export PATH="$HOME/projects/.local/bin:$PATH"
codex-connect doctor
```

`setup` is the one-time backend installer. It installs the running bootstrap binary into the content-addressed build store under `~/projects/.local/lib/codex-connect/builds/`, points `~/projects/.local/bin/codex-connect` at that artifact, creates the default host scope (`~/projects` if absent), writes configuration under `~/projects/.config/codex-connect/`, keeps deployment state under `~/projects/.local/state/codex-connect/`, installs and enables `codex-connect.service` as a user service, starts it, and waits for backend health. The one-time `target/codex-connect-bootstrap/` directory is disposable and should be removed after setup; it is never a runtime authority. Codex itself remains user-global: setup resolves the configured Codex executable, or falls back to `codex` from `PATH`, and persists that absolute path. It does not install a workspace copy of Codex or relocate Codex-owned state from the normal `~/.codex` home.

For later source updates, run:

```bash
codex-connect deploy prepare
# use the operation id printed above
codex-connect deploy status <operation-id>
# once status reports prepared
codex-connect deploy activate <operation-id>
# after the backend reconnects, verify the same operation
codex-connect deploy status <operation-id>
```

`deploy prepare` immediately writes a durable operation record and queues the release build as a detached user-systemd job, then returns without waiting for compilation. Deployment builds serialize through a deployment-wide build lock and reuse the persistent Cargo release target at `target/codex-connect-deploy/build`; that directory is only a compilation cache. The resulting binary is installed as a content-addressed artifact, and the operation moves from `building` to `prepared` (or `failed`) without changing the running backend. `deploy activate` validates that prepared artifact, serializes competing activation requests for that operation, queues detached activation, and returns before the backend restart begins. The detached activation restarts only the Codex Connect backend, verifies backend health, and records success or failure. `deploy status` is authoritative for the whole transaction: it converts an abandoned detached job into a terminal result, verifies that a `prepared` artifact still exists with its recorded SHA-256, and marks that operation failed if the artifact is unavailable or changed. After activation success it verifies that the live backend is running the exact prepared SHA-256. Installed content-addressed builds are retained so one prepared operation cannot be invalidated by activating another. Deployment never restarts or recreates the independent tunnel runtime.

Expected final state: `deploy status` reports `state=succeeded verified=true`, and `codex-connect status` reports the backend service running at `http://127.0.0.1:8767/mcp`.

## 5. Create or select an OpenAI tunnel

Open:

- https://platform.openai.com/settings/organization/tunnels

Create a tunnel (or reuse one intended for this host), and create a **runtime API key** whose principal has **Tunnels Read + Use** for that tunnel. Keep admin CRUD credentials separate from the runtime key.

Official permission references:

- roles: https://platform.openai.com/settings/organization/people/roles
- groups: https://platform.openai.com/settings/organization/people/groups
- runtime API keys: https://platform.openai.com/settings/organization/api-keys

Export the runtime key only in the shell/session that manages the tunnel:

```bash
export CONTROL_PLANE_API_KEY='...'
export CONTROL_PLANE_ORGANIZATION_ID='org_...'
export CONTROL_PLANE_TUNNEL_ID='tunnel_...'
```

For an organization-scoped tunnel, set `CONTROL_PLANE_ORGANIZATION_ID` in the environment used to start or repair the managed runtime. `tunnel-client` sends it as the `OpenAI-Organization` header on control-plane requests. The `--organization-id` flag in the next step scopes tunnel lookup/creation; keep the environment variable set as well because that flag does not replace the runtime header configuration.

Do not put literal API keys in project files or generated profiles. `tunnel-client` supports secret references such as `env:CONTROL_PLANE_API_KEY`.

## 6. Connect the tunnel to Codex Connect

Codex Connect deliberately has **no application-level authentication**. Its MCP listener is constrained to loopback (`127.0.0.1`) and is intended to be reached by the local Secure MCP Tunnel runtime on a single-user host.

Use the native long-lived runtime lifecycle:

```bash
tunnel-client runtimes connect \
  --alias codex-connect \
  --organization-id "$CONTROL_PLANE_ORGANIZATION_ID" \
  --tunnel-id "$CONTROL_PLANE_TUNNEL_ID" \
  --runtime-api-key env:CONTROL_PLANE_API_KEY \
  --profile codex-connect \
  --profile-dir "$HOME/projects/.config/tunnel-client" \
  --mcp-server-url http://127.0.0.1:8767/mcp
```

The canonical tunnel-client profile path is `~/projects/.config/tunnel-client/codex-connect.yaml`. It is owned by the tunnel client and remains independent of Codex Connect's backend configuration and lifecycle.

`CONTROL_PLANE_API_KEY` authenticates `tunnel-client` to OpenAI's tunnel control plane. `CONTROL_PLANE_ORGANIZATION_ID` selects the active organization context for those requests. Neither is a Codex Connect credential, and neither should be supplied to the ChatGPT connector.

Then verify the runtime instead of assuming the launch succeeded:

```bash
tunnel-client runtimes status codex-connect --json
```

Do not report this step as complete until the runtime reports its process running and health/readiness as expected. Use `tunnel-client help troubleshooting` if it does not.

Do not use `nohup`, `disown`, or a second systemd service to supervise the tunnel. Native `tunnel-client runtimes ...` lifecycle owns it.

## After a computer restart

Do not rerun first-time setup or recreate the tunnel. The two runtime owners recover independently:

1. **Backend:** `codex-connect.service` is enabled by setup and should start with the user systemd manager. Verify it with:

   ```bash
   codex-connect status
   codex-connect doctor
   ```

   If it is not healthy, run `codex-connect restart`. This starts the service if necessary, ensures it is enabled for future restarts, and waits for backend health.

2. **Tunnel:** the tunnel-client profile and alias survive reboot, but its native managed runtime may need to be resumed. Export the runtime key required by the profile, then inspect the saved alias:

   ```bash
   export CONTROL_PLANE_API_KEY='...'
   export CONTROL_PLANE_ORGANIZATION_ID='org_...'
   tunnel-client runtimes status codex-connect --json
   ```

   Keep both variables available if the runtime is stopped and you use the `repair_command` reported by `runtimes status`; it is generated from tunnel-client's saved alias/profile metadata. Then verify again:

   ```bash
   tunnel-client runtimes status codex-connect --json
   ```

Do not create a second tunnel profile, a second tunnel alias, or a systemd tunnel service merely to recover from a reboot.

## 7. Add Codex Connect to ChatGPT

Perform initial custom app/connector setup from the ChatGPT settings surface where Developer mode is available.

1. Open ChatGPT settings.
2. Go to **Apps** and enable **Developer mode** / custom MCP app creation. UI wording can change as the feature evolves.
3. Create a custom app/connector named **Codex Connect**.
4. Set its description to: **Connect ChatGPT to your host workspace and Codex CLI. Inspect files, run commands, edit code, and delegate autonomous engineering work to Codex through the official Codex App Server.**
5. Choose **Tunnel** as the connection type when that option is presented.
6. Select the tunnel created above or paste its tunnel ID.
7. Set **Authentication** to **None / No authentication**.
8. Scan/discover the tools and create the app.

Do not paste the OpenAI tunnel runtime key into the ChatGPT connector. It belongs only to `tunnel-client`; Codex Connect has no connector credential.

OpenAI's current developer-mode documentation says custom MCP configuration requires Developer mode and that local/private MCP servers should use Secure MCP Tunnel.

## 8. Prove the complete chain

Start a new ChatGPT conversation with the custom app/connector enabled and ask:

```text
@codexConnect check status
```

A working installation should return live data from `status`, including an available worker and `stdio` App Server transport.

Then test the useful boundary, not just connectivity:

```text
@codexConnect show the configured host scope, create a disposable project under it,
write a small file, read it back, and start a disposable Codex thread in that project.
```

That demonstrates the actual architecture: ChatGPT is the remote control plane, Codex Connect exposes the persistent host, and official Codex App Server semantics own the reasoning/session lifecycle.

## Remove Codex Connect

To remove the managed backend from the host:

```bash
codex-connect uninstall
```

`uninstall` stops/disables and removes `codex-connect.service`, removes Codex Connect's managed configuration and deployment state, deletes the workspace-local operator symlink and installed content-addressed builds, and reloads the user systemd manager. It deliberately does **not** delete the source tree, Codex CLI, or `tunnel-client` state.

If the ChatGPT connection is no longer needed, stop/remove the corresponding tunnel-client runtime/profile separately using the tunnel client's own lifecycle commands. Do not delete tunnel credentials or profiles through Codex Connect.

## Agent handoff

If you are giving the source tree to a coding agent, use this prompt:

> Set up Codex Connect end-to-end by following `docs/getting-started.md`. Keep Codex Connect, Codex App Server, ChatGPT, and Secure MCP Tunnel as separate lifecycle owners. Treat the host scope as a general workspace and do not introduce Git/version-control workflow unless explicitly requested. Never expose or print credentials. Use the project-pinned Codex CLI release. Stop only when human interaction is required in OpenAI/ChatGPT account UI. Before declaring success, verify `codex-connect doctor`, `codex-connect status`, `tunnel-client runtimes status codex-connect --json`, and finally have me confirm that `@codexConnect` can call `status` from ChatGPT.

The agent should not replace Secure MCP Tunnel with ngrok or another ad-hoc ingress, create duplicate services, or add alternate protocol paths around the official App Server contract.

## Troubleshooting checkpoints

Use the boundary that failed rather than restarting everything:

| Check | What it proves |
| --- | --- |
| `codex --version` | the exact App Server implementation expected by this repo is installed |
| `codex-connect doctor` | local config, binary, service, and App Server integration are coherent |
| `codex-connect status` | the local loopback MCP backend is alive |
| `tunnel-client runtimes status codex-connect --json` | the outbound Secure MCP Tunnel runtime is alive/healthy |
| ChatGPT tool scan | ChatGPT can reach the MCP backend through the tunnel with connector authentication set to None |
| `status` from ChatGPT | the complete control path is working |

For backend problems, use `codex-connect logs --follow`. For tunnel problems, use `tunnel-client help troubleshooting`. Do not restart both failure domains blindly.

## Official references

Research for this guide was checked against the current upstream documentation on 2026-09-14:

- Codex CLI: https://github.com/openai/codex
- Codex installation/system requirements: https://github.com/openai/codex/blob/main/docs/install.md
- Codex releases: https://github.com/openai/codex/releases
- Secure MCP Tunnel client: https://github.com/openai/tunnel-client
- Tunnel permissions and ChatGPT connector setup: https://github.com/openai/tunnel-client/blob/master/docs/permissions.md
- ChatGPT Developer mode and MCP apps: https://help.openai.com/en/articles/12584461

OpenAI product availability and UI labels can change. Prefer these upstream references over stale screenshots or copied setup instructions.
