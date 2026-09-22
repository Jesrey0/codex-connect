# Getting Started

Codex Connect runs on Linux with systemd user services. The canonical remote path is:

```text
ChatGPT → ngrok HTTPS → host ingress (OAuth) → loopback Codex Connect → Codex App Server
```

Read [Security](../SECURITY.md) before installation. The backend is a high-trust
host bridge. Its port must remain on loopback; authentication belongs to host ingress.

## Prerequisites

- Linux with `systemctl --user`, Rust 1.88 or newer, and Python 3.
- The exact official Codex CLI release in `config/codex-cli-pin`, authenticated.
- The [host-ingress deployment](https://github.com/Jesrey0/host-ingress), providing
  a stable ngrok HTTPS endpoint, Caddy, and ChatGPT-compatible OAuth.
- ChatGPT Developer mode/custom MCP access with the required read/write tools.
  Availability and UI labels vary by account and workspace; check current settings
  and [OpenAI's developer-mode guide](https://developers.openai.com/api/docs/guides/developer-mode).

Codex CLI and host ingress are user-global, independently managed dependencies.
Codex Connect owns its backend, configuration, deployment records, and installed
artifacts. Secure MCP Tunnel is optional secondary recovery only; it is not required
for normal installation, health, deployment, or connector operation.

## 1. Check the host and install Codex

Obtain the source archive or clone the repository, then run:

```bash
./scripts/check-prerequisites.sh
```

The checker is read-only. Install the exact pinned Codex release using an official
distribution. With Node/npm already installed:

```bash
CODEX_PIN="$(cat config/codex-cli-pin)"
npm install -g "@openai/codex@${CODEX_PIN}"
codex --version
codex
```

Authenticate interactively. Keep Codex binaries and its normal `~/.codex` home
user-global. Do not relocate credentials into the source checkout.

## 2. Build and install the backend

```bash
cargo build --release -p codex-connect --locked
target/release/codex-connect setup
export PATH="$HOME/.local/bin:$PATH"
codex-connect doctor
codex-connect status
```

`setup` installs the running binary into the content-addressed build store under
`~/.local/lib/codex-connect/builds/`, selects it through `~/.local/bin/codex-connect`,
installs/enables the user service, and waits for private backend health. It resolves
the user-global Codex executable and persists its absolute path.

Configuration uses `$XDG_CONFIG_HOME/codex-connect` or `~/.config/codex-connect`;
state and cache use the equivalent XDG locations. The default navigation cwd is
`~`, which is navigation context, not an authorization boundary.

Expected private endpoint: `http://127.0.0.1:8767/mcp`. Never enter this localhost
address as the remote ChatGPT connector URL.

## 3. Configure and verify host ingress

Follow the host-ingress README for its credential setup, one-time OAuth identity
initialization, source validation, and deployment. Use the existing ngrok agent;
do not create another public endpoint or proxy. From the host-ingress checkout:

```bash
./scripts/status
CHECK_PUBLIC=1 ./scripts/check
```

The canonical public URL is `${NGROK_URL}/codex-connect/mcp`; host ingress owns
`NGROK_URL` and reports it through `scripts/status`. Its authentication contract is:

- OAuth authorization code with S256 PKCE and ChatGPT CIMD client registration.
- ChatGPT's signed `private_key_jwt` client assertion at the token endpoint.
- Single-operator browser login and explicit consent at host ingress.
- Access tokens bound to the canonical MCP resource and `codex-connect:access`.
- `offline_access` with persistent, rotating refresh tokens.

Anonymous and invalid-token MCP requests must return 401 with a Bearer discovery
challenge. Public metadata must describe the exact HTTPS resource and issuer.
Private status/health endpoints and undeclared routes must remain inaccessible.

## 4. Connect ChatGPT

1. Enable Developer mode/custom app creation in your ChatGPT account or workspace.
2. Create or update **Codex Connect** using the public HTTPS MCP URL above.
3. Select **OAuth** authentication and **CIMD** when offered. Leave static client
   ID/secret fields empty.
4. Complete the browser operator login and consent. The login uses the existing
   host-ingress credential; do not put it in the MCP URL or ChatGPT client fields.
5. Scan/discover tools and enable the app. Expect exactly 14 tools.

The authorization server uses issuer identification and the stable callback
`https://chatgpt.com/connector_platform_oauth_redirect`. The allowed client metadata
document is `https://chatgpt.com/oauth/client.json`.

Backend deployment and connector refresh are separate. Refresh/rediscover after
tool metadata changes; a running service does not prove the connector snapshot is current.

## 5. Verify from ChatGPT

Use the connected app to call `status` and `inspect`; check the live build and
navigation cwd. Then verify `command.exec`, `command.start`, `command.read`,
`command.control`, worker lifecycle, and any pending action/approval flow using
disposable work. Repeat after an idle period and after a controlled backend restart.
Run only 5 × status and 5 × inspect for a lightweight latency sanity check.

Treat latency as an environment-specific validation signal rather than a fixed product
threshold. Establish a local baseline with lightweight calls after ingress is stable;
measure command initiation separately because backend/App Server work can dominate it.

### Optional secondary fallback

You may retain an independently configured OpenAI Secure MCP Tunnel for recovery.
Keep it outside the Codex Connect source/config tree and manage it only through the
upstream `tunnel-client`. Normal operation uses the ngrok/host-ingress connector;
keep the fallback inactive unless needed, and never make backend health, deployment,
or application timeout behavior depend on it.

## Updates, restart, and removal

Use the backend-only deployment transaction described in [Operations](operations.md):

```bash
codex-connect deploy prepare
codex-connect deploy status <operation-id>
codex-connect deploy activate <operation-id>
codex-connect deploy status <operation-id>
```

Activate only after preparation and verification. Final status must report the exact
prepared artifact live. Deployment does not imply Git commit/push or connector refresh.

After reboot, check `codex-connect doctor` and host-ingress health separately. Repair
the failed service rather than rerunning setup. OAuth state survives ingress restarts;
MCP clients establish a new session after the backend restarts.

`codex-connect uninstall` removes only its managed backend installation and state.
Remove its public route and ChatGPT connector separately when retiring the service.

## References

- [Codex releases](https://github.com/openai/codex/releases)
- [OpenAI authentication](https://developers.openai.com/plugins/build/auth)
- [MCP authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization)
- [Host ingress](https://github.com/Jesrey0/host-ingress)
