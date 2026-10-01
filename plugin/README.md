# Operator plugin update source

This directory records the editable operator skill and manifests for the existing
private `codex-connect` account plugin. It is an update overlay, not a standalone
installation package. The account release retains the unchanged app binding,
MCP configuration, and icon assets; those facts are not duplicated here.

Read the current eligible release through Plugin Creator before editing. Keep
the verified plugin identity, scope, audience, integrations, and default prompts.
Synchronize both manifests, advance the existing plugin version, validate the
skill, and package changed files at these relative paths. Publish with the
observed release ID as the conflict guard, then read the affected files back.
The account API preserves omitted files; it cannot delete files through omission.

Git commits, saved plugin releases, backend deployment, and connector refresh are
separate states. Publishing a skill update does not deploy the MCP backend.
