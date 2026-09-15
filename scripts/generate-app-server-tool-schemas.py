#!/usr/bin/env python3
"""Generate the exact internal App Server contract closure from the pinned CLI."""

import argparse
import json
import pathlib
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
PIN = (ROOT / "config/codex-cli-pin").read_text().strip()
ARTIFACT = ROOT / "config/app-server-tool-schemas.json"

METHODS = {
    "initialize": ("InitializeParams", "InitializeResponse"),
    "account/rateLimits/read": ("v2/GetAccountRateLimitsParams", "v2/GetAccountRateLimitsResponse"),
    "command/exec": ("v2/CommandExecParams", "v2/CommandExecResponse"),
    "fuzzyFileSearch": ("FuzzyFileSearchParams", "FuzzyFileSearchResponse"),
    "fs/getMetadata": ("v2/FsGetMetadataParams", "v2/FsGetMetadataResponse"),
    "fs/readDirectory": ("v2/FsReadDirectoryParams", "v2/FsReadDirectoryResponse"),
    "fs/readFile": ("v2/FsReadFileParams", "v2/FsReadFileResponse"),
    "model/list": ("v2/ModelListParams", "v2/ModelListResponse"),
    "review/start": ("v2/ReviewStartParams", "v2/ReviewStartResponse"),
    "skills/list": ("v2/SkillsListParams", "v2/SkillsListResponse"),
    "thread/read": ("v2/ThreadReadParams", "v2/ThreadReadResponse"),
    "thread/resume": ("v2/ThreadResumeParams", "v2/ThreadResumeResponse"),
    "thread/start": ("v2/ThreadStartParams", "v2/ThreadStartResponse"),
    "turn/interrupt": ("v2/TurnInterruptParams", "v2/TurnInterruptResponse"),
    "turn/start": ("v2/TurnStartParams", "v2/TurnStartResponse"),
    "turn/steer": ("v2/TurnSteerParams", "v2/TurnSteerResponse"),
}
SERVER_REQUESTS = {
    "item/commandExecution/requestApproval": ("CommandExecutionRequestApprovalParams", "CommandExecutionRequestApprovalResponse"),
    "item/fileChange/requestApproval": ("FileChangeRequestApprovalParams", "FileChangeRequestApprovalResponse"),
    "item/permissions/requestApproval": ("PermissionsRequestApprovalParams", "PermissionsRequestApprovalResponse"),
    "mcpServer/elicitation/request": ("McpServerElicitationRequestParams", "McpServerElicitationRequestResponse"),
    "item/tool/requestUserInput": ("ToolRequestUserInputParams", "ToolRequestUserInputResponse"),
}
# These notifications receive semantic handling; other notifications are opaque journal data.
NOTIFICATIONS = {
    "serverRequest/resolved": "v2/ServerRequestResolvedNotification",
    "turn/completed": "v2/TurnCompletedNotification",
    "thread/started": "v2/ThreadStartedNotification",
}


def refs(value):
    if isinstance(value, dict):
        ref = value.get("$ref")
        if isinstance(ref, str):
            if not ref.startswith("#/definitions/"):
                raise RuntimeError(f"unexpected external schema reference: {ref}")
            yield ref.removeprefix("#/definitions/")
        for child in value.values():
            yield from refs(child)
    elif isinstance(value, list):
        for child in value:
            yield from refs(child)


def lookup(definitions, path):
    value = definitions
    for part in path.split("/"):
        value = value[part]
    return value


def branches(schema):
    return {b["properties"]["method"]["enum"][0]: b for b in schema["oneOf"]}


def build_artifact(output):
    bundle = json.loads((output / "codex_app_server_protocol.schemas.json").read_text())
    definitions = bundle["definitions"]
    artifact = {"codexPin": PIN, "methods": {}, "serverRequests": {}, "notifications": {}}
    for group, contracts, request_name, input_key, output_key in [
        ("methods", METHODS, "ClientRequest", "inputSchema", "outputSchema"),
        ("serverRequests", SERVER_REQUESTS, "ServerRequest", "paramsSchema", "responseSchema"),
    ]:
        requests = branches(definitions[request_name])
        for method, (params, response) in sorted(contracts.items()):
            if method not in requests:
                raise RuntimeError(f"pinned schema removed {method}")
            actual = set(refs(requests[method]["properties"].get("params")))
            if actual != {params}:
                raise RuntimeError(f"schema drift for {method}: expected {params}, found {sorted(actual)}")
            artifact[group][method] = {
                input_key: {"$ref": f"#/definitions/{params}"},
                output_key: {"$ref": f"#/definitions/{response}"},
            }
    notifications = branches(definitions["ServerNotification"])
    for method, params in NOTIFICATIONS.items():
        if set(refs(notifications[method]["properties"]["params"])) != {params}:
            raise RuntimeError(f"notification schema drift for {method}")
        artifact["notifications"][method] = {"$ref": f"#/definitions/{params}"}

    # Deduplicate the transitive closure once, preserving official JSON Pointer paths.
    selected = {}
    seen = set()
    pending = list(refs(artifact))
    while pending:
        path = pending.pop()
        if path in seen:
            continue
        seen.add(path)
        value = lookup(definitions, path)
        destination = selected
        parts = path.split("/")
        for part in parts[:-1]:
            destination = destination.setdefault(part, {})
        destination[parts[-1]] = value
        pending.extend(refs(value))
    artifact["definitions"] = selected
    return json.dumps(artifact, sort_keys=True, separators=(",", ":")) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="fail on drift without rewriting the artifact")
    args = parser.parse_args()
    output = subprocess.check_output(["codex", "--version"], text=True, stderr=subprocess.PIPE)
    if output.strip() != f"codex-cli {PIN}":
        raise SystemExit(f"Codex release mismatch: expected codex-cli {PIN}, found {output.strip()}")
    with tempfile.TemporaryDirectory(prefix="codex-connect-schemas-") as directory:
        subprocess.run(["codex", "app-server", "generate-json-schema", "--out", directory], check=True)
        generated = build_artifact(pathlib.Path(directory))
    if args.check:
        if ARTIFACT.read_text() != generated:
            raise SystemExit("Pinned App Server schema drift: regenerate the artifact and review its diff.")
        print("Pinned App Server schema artifact matches the installed CLI.")
    else:
        ARTIFACT.write_text(generated)


if __name__ == "__main__":
    main()
