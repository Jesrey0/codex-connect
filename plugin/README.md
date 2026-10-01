# Operator plugin update source

This tree owns the shared `0xoperator` and `0xoperator-codex` skill sources for
the existing private `codex-connect` account plugin, displayed as **0x0perator**.
The OpenCode adapter is maintained in the sibling repository at
`opencode-connect/skills/0xoperator-opencode/`. The operator assembles these
three canonical skills; each backend remains independently owned and deployed.

Use substrate for the runtime, connector for its integration, and provider for
the upstream model provider. Exact routing lives in the core skill. Protocol
details live in each adapter’s linked references; do not duplicate doctrine.

Read the current eligible release through Plugin Creator before changing a
release. Preserve plugin identity, audience, app bindings, icons and the full
defaultPrompt value/type/order. Synchronize both manifests and advance the
plugin version independently of either backend version.

Build the update from maintained sources:

```bash
python3 plugin/assemble.py --opencode ../opencode-connect --archive work/operator-update.zip
```

The assembly validates canonical names/frontmatter, packaged links, manifest
agreement, retained migration policy, and excluded component references. It
includes changed text files; the account publisher preserves omitted app/MCP
bindings and binary assets. Read back the resulting release and verify those
preservation checks after guarded publication.

The account editor overlays files and cannot delete them. The assembly replaces
the two retained old skill paths with migration notices without runtime doctrine
and disables their implicit invocation. They route explicit old-name requests to
the canonical adapters. They are not a second implementation or a backend API
alias. Do not claim the old paths were physically deleted from the account.

Source commits, saved releases, backend deployment and connector refresh are
separate facts. Publishing instructions does not deploy either MCP backend.
