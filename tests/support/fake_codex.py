#!/usr/bin/env python3
"""Pinned-schema-valid App Server peer for operator integration tests."""

import copy
import json
import os
import pathlib
import sys
import threading
import time

import jsonschema

ROOT = pathlib.Path(__file__).resolve().parents[2]
CONTRACT = json.loads((ROOT / "config/app-server-tool-schemas.json").read_text())
if "--version" in sys.argv:
    print(f"codex-cli {CONTRACT['codexPin']}")
    raise SystemExit


def resolve(schema):
    while "$ref" in schema:
        value = CONTRACT
        for part in schema["$ref"].removeprefix("#/").split("/"):
            value = value[part]
        schema = value
    return schema


def sample(schema):
    schema = resolve(schema)
    if "enum" in schema:
        return schema["enum"][0]
    if "const" in schema:
        return schema["const"]
    for union in ("oneOf", "anyOf", "allOf"):
        if union in schema:
            return sample(schema[union][0])
    kind = schema.get("type")
    if isinstance(kind, list):
        kind = "null" if "null" in kind else kind[0]
    if kind == "object":
        return {key: sample(schema["properties"][key]) for key in schema.get("required", [])}
    if kind == "array":
        return []
    if kind == "string":
        return "fixture"
    if kind in ("integer", "number"):
        return schema.get("minimum", 0)
    if kind == "boolean":
        return False
    return None


def validate(value, schema):
    jsonschema.Draft7Validator({**schema, "definitions": CONTRACT["definitions"]}).validate(value)


lock = threading.RLock()
threads = {}
pending = {}
initialized = False
handshake = False


def send(message):
    with lock:
        print(json.dumps(message), flush=True)


def respond(message, result):
    validate(result, CONTRACT["methods"][message["method"]]["outputSchema"])
    send({"id": message["id"], "result": result})


def complete(thread_id, turn_id, status="completed", notify=True):
    with lock:
        turn = next(t for t in threads[thread_id]["turns"] if t["id"] == turn_id)
        turn["status"] = status
        turn["items"] = [{"type": "agentMessage", "id": "answer", "text": "fixture complete", "phase": "final_answer"}]
        if notify:
            send({"method": "turn/completed", "params": {"threadId": thread_id, "turn": turn}})


def action(thread_id, turn_id, scenario):
    with lock:
        method = {
            "question": "item/tool/requestUserInput",
            "nonblocking": "item/tool/requestUserInput",
            "approval": "item/commandExecution/requestApproval",
            "file": "item/fileChange/requestApproval",
            "permissions": "item/permissions/requestApproval",
            "form": "mcpServer/elicitation/request",
            "url": "mcpServer/elicitation/request",
        }[scenario]
        params = sample(CONTRACT["serverRequests"][method]["paramsSchema"])
        params.update(threadId=thread_id, turnId=turn_id)
        if scenario in ("question", "nonblocking"):
            params.update(isBlocking=scenario == "question", questions=[{
                "id": "format", "header": "Format", "question": "Which output format?",
                "options": [{"label": "JSON", "description": "Structured output"}],
            }])
        elif scenario == "permissions":
            params.update(cwd=os.getcwd(), permissions={"network": {"enabled": True}})
        elif scenario == "form":
            params.update(serverName="fixture", mode="form", message="Name", requestedSchema={"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]})
        elif scenario == "url":
            params = {"threadId": thread_id, "turnId": turn_id, "serverName": "fixture", "mode": "url", "message": "Authorize", "url": "https://example.com/authorize", "elicitationId": "url1"}
        validate(params, CONTRACT["serverRequests"][method]["paramsSchema"])
        request_id = f"request-{turn_id}"
        pending[request_id] = (method, thread_id, turn_id)
        send({"id": request_id, "method": method, "params": params})


for line in sys.stdin:
    message = json.loads(line)
    if "method" not in message:
        method, thread_id, turn_id = pending.pop(message["id"])
        if "result" in message:
            validate(message["result"], CONTRACT["serverRequests"][method]["responseSchema"])
        send({"method": "serverRequest/resolved", "params": {"threadId": thread_id, "requestId": message["id"]}})
        complete(thread_id, turn_id)
        continue
    method = message["method"]
    params = message.get("params", {})
    if method == "initialized":
        assert handshake
        initialized = True
        continue
    validate(params, CONTRACT["methods"][method]["inputSchema"])
    result = sample(CONTRACT["methods"][method]["outputSchema"])
    if method == "initialize":
        assert not handshake
        assert params["capabilities"] == {"experimentalApi": True, "requestAttestation": False}
        handshake = True
        respond(message, result)
        continue
    assert initialized
    if method == "thread/start":
        thread_id = f"thread-{len(threads) + 1}"
        thread = result["thread"]
        thread.update(id=thread_id, cwd=params.get("cwd", os.getcwd()), turns=[])
        threads[thread_id] = thread
        result["cwd"] = thread["cwd"]
    elif method in ("thread/resume", "thread/read"):
        thread = copy.deepcopy(threads[params["threadId"]])
        result["thread"] = thread
        if method == "thread/resume":
            thread["cwd"] = params.get("cwd", thread["cwd"])
            result["cwd"] = thread["cwd"]
    elif method in ("turn/start", "review/start"):
        thread_id = params["threadId"]
        thread = threads[thread_id]
        turn = result["turn"]
        turn_id = f"{thread_id}-turn-{len(thread['turns']) + 1}"
        turn.update(id=turn_id, status="inProgress", items=[], error=None)
        thread["turns"].append(turn)
        scenario = params["input"][0]["text"] if method == "turn/start" else "complete"
        if method == "review/start":
            result["reviewThreadId"] = thread_id
        respond(message, result)
        if scenario == "no_event":
            complete(thread_id, turn_id, notify=False)
        elif scenario == "progress":
            send({"method":"item/agentMessage/delta","params":{"threadId":thread_id,"turnId":turn_id,"delta":"Working"}})
        elif scenario == "oversized":
            send({"method":"item/completed","params":{"threadId":thread_id,"turnId":turn_id,"data":"x"*140000}})
        elif scenario == "idle":
            pass
        elif scenario == "delayed_question":
            timer = threading.Timer(0.25, action, (thread_id, turn_id, "question"))
            timer.daemon = True
            timer.start()
        elif scenario in ("question", "nonblocking", "approval", "file", "permissions", "form", "url"):
            action(thread_id, turn_id, scenario)
        else:
            complete(thread_id, turn_id)
        continue
    elif method == "turn/steer":
        result["turnId"] = params["expectedTurnId"]
    elif method == "turn/interrupt":
        for request_id, (_, thread_id, turn_id) in list(pending.items()):
            if turn_id == params["turnId"]:
                pending.pop(request_id)
                send({"method":"serverRequest/resolved","params":{"threadId":thread_id,"requestId":request_id}})
        complete(params["threadId"], params["turnId"], status="interrupted")
    elif method == "command/exec":
        assert not any(key in params for key in ("tty", "streamStdin", "disableTimeout", "disableOutputCap"))
        assert 0 < params["timeoutMs"] <= 300000
        assert params["outputBytesCap"] <= 262144
        if params["command"] == ["disconnect"]:
            raise SystemExit
        result.update(exitCode=0, stdout="fixture command\n", stderr="")
    respond(message, result)
