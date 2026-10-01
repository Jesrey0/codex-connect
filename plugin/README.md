# Operator plugin update source

This directory records the editable operator skill and manifests for the existing
private `codex-connect` account plugin, displayed as **0x0perator**. It is an
update overlay, not a standalone installation package. The account release
retains unchanged MCP configuration and icon assets. The app binding records
the existing Codex app and the replacement OpenCode app selected by the owner.

The instruction release contains three skills: the small provider-neutral
`skills/0xoperator/SKILL.md`, the Codex provider maintained here, and the OpenCode
provider maintained at `../opencode-connect/skills/opencode-connect/SKILL.md`
relative to the project directory. Package that sibling source as
`skills/opencode-connect/SKILL.md` in the same update overlay. Keep one maintained
source for each provider skill; do not copy it into this repository. Each app
has its own connection, and each backend remains independently deployable.

Read the current eligible release through Plugin Creator before editing. Keep
the verified plugin identity, scope, audience, integrations, and default prompts.
Synchronize both manifests, advance the existing plugin version, validate the
skill, and package changed files at these relative paths. Publish with the
observed release ID as the conflict guard, then read the affected files back.
The account API preserves omitted files; it cannot delete files through omission.

Git commits, saved plugin releases, backend deployment, and connector refresh are
separate states. Publishing a skill update does not deploy either MCP backend.
