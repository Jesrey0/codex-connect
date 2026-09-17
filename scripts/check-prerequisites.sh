#!/usr/bin/env bash
set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
pin="$(tr -d '[:space:]' < "$repo_root/config/codex-cli-pin")"
failures=0
warnings=0

pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; failures=$((failures + 1)); }
warn() { printf 'WARN  %s\n' "$*"; warnings=$((warnings + 1)); }

if [[ "$(uname -s)" == "Linux" ]]; then
  pass "Linux host"
else
  fail "managed setup currently requires Linux/systemd (found $(uname -s))"
fi

for cmd in cargo rustc systemctl; do
  if command -v "$cmd" >/dev/null 2>&1; then
    pass "$cmd available"
  else
    fail "$cmd missing"
  fi
done

if command -v systemctl >/dev/null 2>&1; then
  if systemctl --user show-environment >/dev/null 2>&1; then
    pass "systemd user manager reachable"
  else
    fail "systemd user manager is not reachable (systemctl --user show-environment failed)"
  fi
fi

if command -v rustc >/dev/null 2>&1; then
  rust_version="$(rustc --version | awk '{print $2}')"
  minimum="1.85.0"
  if [[ "$(printf '%s\n%s\n' "$minimum" "$rust_version" | sort -V | head -n1)" == "$minimum" ]]; then
    pass "rustc $rust_version (>= $minimum)"
  else
    fail "rustc $rust_version is older than required $minimum"
  fi
fi

if command -v codex >/dev/null 2>&1; then
  codex_release="$(codex --version 2>/dev/null | awk '{print $NF}')"
  if [[ "$codex_release" == "$pin" ]]; then
    pass "Codex CLI release matches project pin"
  else
    fail "Codex CLI release does not match project pin $pin"
  fi
else
  fail "Codex CLI missing (required release: $pin)"
fi

if command -v tunnel-client >/dev/null 2>&1; then
  tunnel_version="$(tunnel-client --version 2>/dev/null | head -n1)"
  pass "tunnel-client available ($tunnel_version)"
else
  fail "tunnel-client missing; install OpenAI Secure MCP Tunnel client"
fi

config_home="${XDG_CONFIG_HOME:-$HOME/projects/.config}"
if [[ -f "$config_home/codex-connect/config.toml" ]]; then
  pass "existing Codex Connect configuration found"
else
  warn "Codex Connect is not configured yet; this is expected before codex-connect setup"
fi

printf '\nSummary: %d failure(s), %d warning(s).\n' "$failures" "$warnings"
if (( failures > 0 )); then
  exit 1
fi
