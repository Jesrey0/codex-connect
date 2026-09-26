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
- ChatGPT Developer mode with access to personal Plugins/MCP connections.
  OpenAI's current setup path is documented in the
  [Plugins quickstart](https://developers.openai.com/plugins/quickstart).

Codex CLI and host ingress are independently managed. Codex Connect owns its backend service, deployment records/cache, and installed artifacts.

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
installs/enables the user service, and waits for private backend health. The managed
backend resolves `codex` from its captured service PATH; App Server verifies the
pinned release during startup.

Codex Connect has no product configuration file. The managed backend binds
`127.0.0.1:8767`, uses `~` as its default cwd, and resolves `codex` from the
service PATH captured by setup. Deployment state and cache use the standard XDG
state/cache locations.

Expected private endpoint: `http://127.0.0.1:8767/mcp`. Never enter this localhost
address as the ChatGPT plugin MCP URL.

## 3. Configure and verify host ingress

Follow the [host-ingress README](https://github.com/Jesrey0/host-ingress) for OAuth setup and deployment. From its checkout:

```bash
./scripts/status
CHECK_PUBLIC=1 ./scripts/check
```

The public URL is `${NGROK_URL}/codex-connect/mcp`; host ingress reports `NGROK_URL` through `scripts/status`.

Anonymous and invalid-token MCP requests must return 401 with a Bearer discovery
challenge. Public metadata must describe the exact HTTPS resource and issuer.
Private status/health endpoints and undeclared routes must remain inaccessible.

## 4. Connect ChatGPT

1. In ChatGPT, open **Settings → Security and login** and enable **Developer mode**.
2. Open **ChatGPT Plugins**, add a new MCP server, and enter the public HTTPS MCP URL above.
3. Configure OAuth. Prefer **CIMD** when offered by the builder and supported by host ingress;
   do not invent or paste a static client secret for ChatGPT.
4. Complete the browser operator login and consent. The login uses the existing
   host-ingress credential; never put credentials in the MCP URL or tool arguments.
5. Scan/discover the server and install/enable the resulting Codex Connect plugin. Expect exactly 13 tools.

OpenAI's product labels and supported invocation surfaces can change independently of
Codex Connect. The architectural requirement here is stable: **all actions and mutations
flow through ChatGPT**. The local `codex-connect console` is the deliberate exception for
read-only worker visibility; it must never become a second control surface.

The backend may be connected from multiple ChatGPT operators for the same trusted user.
They share one backend-global recovery view and the same underlying OS-account authority;
Codex Connect does not create per-conversation or per-account ownership boundaries. Do not
use this architecture as a multi-tenant isolation mechanism.

Backend deployment and plugin scan/refresh are separate. Rescan after tool metadata
changes; a running service does not prove ChatGPT's cached tool snapshot is current.

## 5. Verify from ChatGPT

Use ChatGPT with the connected plugin to call `status` and `inspect`; check live build identity and default cwd. Verify command, worker, and pending-action flows with disposable work as described in [Operations](operations.md). Repeat after an idle period and a controlled backend restart.

## Updates, restart, and removal

Use the backend-only deployment transaction described in [Operations](operations.md):

```bash
codex-connect deploy prepare
codex-connect deploy status <operation-id>
codex-connect deploy activate <operation-id>
codex-connect deploy status <operation-id>
```

Activate only after preparation and verification. Final status must report the exact
prepared artifact live. Deployment does not imply Git commit/push or a ChatGPT plugin rescan.

After reboot, check `codex-connect doctor` and host-ingress health separately. Repair
the failed service rather than rerunning setup. OAuth state survives ingress restarts;
MCP clients send independent `2026-07-28` requests after the backend restarts.

`codex-connect uninstall` removes only its managed backend installation and state.
Remove its public route and ChatGPT plugin connection separately when retiring the service.

## References

- [Codex releases](https://github.com/openai/codex/releases)
- [OpenAI Plugins quickstart](https://developers.openai.com/plugins/quickstart)
- [OpenAI MCP server guidance](https://developers.openai.com/plugins/build/mcp-server)
- [OpenAI authentication](https://developers.openai.com/plugins/build/auth)
- [MCP authorization](https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization)
- [Host ingress](https://github.com/Jesrey0/host-ingress)
