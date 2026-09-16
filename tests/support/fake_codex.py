#!/usr/bin/env python3
"""Pinned-schema-valid App Server peer for operator integration tests."""

import base64
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
command_sessions = {}
initialized = False
handshake = False


def send(message):
    with lock:
        print(json.dumps(message), flush=True)


def respond(message, result):
    validate(result, CONTRACT["methods"][message["method"]]["outputSchema"])
    send({"id": message["id"], "result": result})


def command_output(process_id, stream, text):
    send({
        "method": "command/exec/outputDelta",
        "params": {
            "processId": process_id,
            "stream": stream,
            "deltaBase64": base64.b64encode(text.encode()).decode(),
            "capReached": False,
        },
    })


def finish_command(process_id, exit_code=0):
    with lock:
        session = command_sessions.pop(process_id)
        result = sample(CONTRACT["methods"]["command/exec"]["outputSchema"])
        result.update(exitCode=exit_code, stdout="", stderr="")
        respond(session["message"], result)


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
            "oversized_question": "item/tool/requestUserInput",
            "nonblocking": "item/tool/requestUserInput",
            "approval": "item/commandExecution/requestApproval",
            "file": "item/fileChange/requestApproval",
            "permissions": "item/permissions/requestApproval",
            "form": "mcpServer/elicitation/request",
            "openai_form": "mcpServer/elicitation/request",
            "url": "mcpServer/elicitation/request",
        }[scenario]
        params = sample(CONTRACT["serverRequests"][method]["paramsSchema"])
        params.update(threadId=thread_id, turnId=turn_id)
        if scenario in ("question", "oversized_question", "nonblocking"):
            params.update(isBlocking=scenario == "question", questions=[{
                "id": "format", "header": "Format",
                "question": "Q" * (9 * 1024 * 1024) if scenario == "oversized_question" else "Which output format?",
                "options": [{"label": "JSON", "description": "Structured output"}],
            }])
            if scenario == "oversized_question":
                params["isBlocking"] = True
        elif scenario == "permissions":
            params.update(cwd=os.getcwd(), permissions={"network": {"enabled": True}})
        elif scenario == "form":
            params.update(serverName="fixture", mode="form", message="Name", requestedSchema={"type": "object", "properties": {"name": {"type": "string"}}, "required": ["name"]})
        elif scenario == "openai_form":
            params = {"threadId": thread_id, "turnId": turn_id, "serverName": "fixture", "mode": "openai/form", "message": "Name", "requestedSchema": {"opaque": True}}
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
        assert params["capabilities"] == {
            "experimentalApi": True,
            "requestAttestation": False,
            "extensions": {"openai/form": {}},
        }
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
        if method == "thread/read":
            assert params["includeTurns"] is False
            thread["turns"] = []
        result["thread"] = thread
        if method == "thread/resume":
            thread["cwd"] = params.get("cwd", thread["cwd"])
            result["cwd"] = thread["cwd"]
    elif method == "thread/items/list":
        entries = []
        for turn in threads[params["threadId"]]["turns"]:
            if params.get("turnId") not in (None, turn["id"]):
                continue
            entries.extend({"turnId": turn["id"], "item": copy.deepcopy(item)} for item in turn["items"])
        if params.get("sortDirection", "asc") == "desc":
            entries.reverse()
        start = int(params.get("cursor") or 0)
        limit = params.get("limit") or 100
        result["data"] = entries[start:start + limit]
        result["nextCursor"] = str(start + limit) if start + limit < len(entries) else None
    elif method == "thread/turns/list":
        turns = copy.deepcopy(threads[params["threadId"]]["turns"])
        if params.get("sortDirection", "desc") == "desc":
            turns.reverse()
        start = int(params.get("cursor") or 0)
        limit = params.get("limit") or 50
        result["data"] = turns[start:start + limit]
        result["nextCursor"] = str(start + limit) if start + limit < len(turns) else None
    elif method == "turn/start":
        thread_id = params["threadId"]
        thread = threads[thread_id]
        turn = result["turn"]
        turn_id = f"{thread_id}-turn-{len(thread['turns']) + 1}"
        turn.update(id=turn_id, status="inProgress", items=[], error=None)
        thread["turns"].append(turn)
        scenario = params["input"][0]["text"]
        respond(message, result)
        if scenario == "no_event":
            complete(thread_id, turn_id, notify=False)
        elif scenario == "progress":
            send({"method":"item/agentMessage/delta","params":{"threadId":thread_id,"turnId":turn_id,"delta":"Working"}})
        elif scenario == "oversized":
            send({"method":"item/completed","params":{"threadId":thread_id,"turnId":turn_id,"data":"x"*140000}})
        elif scenario == "wire_oversized":
            send({"method":"item/completed","params":{"threadId":thread_id,"turnId":turn_id,"data":"x"*(9*1024*1024)}})
            complete(thread_id, turn_id)
        elif scenario == "idle":
            pass
        elif scenario == "inflate_history":
            for index in range(60):
                filler = copy.deepcopy(turn)
                filler.update(
                    id=f"{thread_id}-filler-{index}",
                    status="completed",
                    items=[],
                    error=None,
                )
                thread["turns"].append(filler)
            complete(thread_id, turn_id)
        elif scenario == "delayed_question":
            timer = threading.Timer(0.25, action, (thread_id, turn_id, "question"))
            timer.daemon = True
            timer.start()
        elif scenario in ("question", "oversized_question", "nonblocking", "approval", "file", "permissions", "form", "openai_form", "url"):
            action(thread_id, turn_id, scenario)
        else:
            complete(thread_id, turn_id)
        continue
    elif method == "review/start":
        assert params["delivery"] == "inline"
        thread_id = params["threadId"]
        thread = threads[thread_id]
        turn = result["turn"]
        turn_id = f"{thread_id}-turn-{len(thread['turns']) + 1}"
        turn.update(id=turn_id, status="inProgress", items=[], error=None)
        delayed_visibility = params["target"].get("type") == "custom" and params["target"].get("instructions") == "delayed_visibility"
        if not delayed_visibility:
            thread["turns"].append(turn)

        # Mirror live 0.154.0: reviewThreadId can identify the internal reviewer while the
        # response turn itself belongs to the source thread.
        review_thread_id = f"review-thread-{thread_id}-{len(thread['turns'])}"
        review_thread = copy.deepcopy(thread)
        review_thread["id"] = review_thread_id
        review_turn = copy.deepcopy(turn)
        review_turn.update(id=f"{review_thread_id}-turn-1", status="inProgress", items=[])
        review_thread["turns"] = [review_turn]
        threads[review_thread_id] = review_thread

        result["reviewThreadId"] = review_thread_id
        respond(message, result)
        if delayed_visibility:
            completed_turn = copy.deepcopy(turn)
            completed_turn.update(
                status="failed",
                error={"message": "quota exhausted", "codexErrorInfo": "usageLimitExceeded"},
                items=[],
            )
            send({"method": "turn/completed", "params": {"threadId": thread_id, "turn": completed_turn}})

            def publish_review_turn(source_thread_id, published_turn):
                with lock:
                    threads[source_thread_id]["turns"].append(published_turn)

            timer = threading.Timer(0.25, publish_review_turn, (thread_id, copy.deepcopy(completed_turn)))
            timer.daemon = True
            timer.start()
        else:
            complete(thread_id, turn_id)
        complete(review_thread_id, review_turn["id"], status="interrupted")
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
        if "processId" not in params:
            assert not any(key in params for key in ("tty", "streamStdin", "disableTimeout", "disableOutputCap"))
            assert 0 < params["timeoutMs"] <= 3600000
            assert params["outputBytesCap"] <= 262144
            if params["command"] == ["disconnect"]:
                raise SystemExit
            result.update(exitCode=0, stdout="fixture command\n", stderr="")
        else:
            process_id = params["processId"]
            assert params["streamStdin"] is True
            assert params["streamStdoutStderr"] is True
            assert params["disableTimeout"] is True
            assert params["disableOutputCap"] is True
            command_sessions[process_id] = {
                "message": copy.deepcopy(message),
                "tty": params["tty"],
                "size": copy.deepcopy(params.get("size")),
            }
            scenario = params["command"][0]
            if scenario == "fixture-stream":
                command_output(process_id, "stdout", "ready\n")
                command_output(process_id, "stderr", "warning\n")
            elif scenario == "fixture-repl":
                assert params["tty"] is True
                command_output(process_id, "stdout", ">>> ")
            elif scenario == "fixture-exit":
                command_output(process_id, "stdout", "done\n")
                finish_command(process_id, 0)
            elif scenario == "fixture-delayed-exit":
                timer = threading.Timer(0.25, finish_command, (process_id, 0))
                timer.daemon = True
                timer.start()
            elif scenario == "fixture-bounded":
                for _ in range(20):
                    command_output(process_id, "stdout", "x" * 40000)
            elif scenario == "fixture-sandbox":
                command_output(process_id, "stdout", json.dumps(params["sandboxPolicy"]) + "\n")
            elif scenario == "fixture-context":
                command_output(process_id, "stdout", json.dumps({
                    "cwd": params.get("cwd"),
                    "env": params.get("env"),
                }) + "\n")
            elif scenario == "fixture-quiet":
                pass
            else:
                raise AssertionError(f"unknown persistent command fixture: {scenario}")
            continue
    elif method == "command/exec/write":
        process_id = params["processId"]
        session = command_sessions[process_id]
        text = base64.b64decode(params.get("deltaBase64") or "").decode()
        if session["tty"] and text:
            if text == "2+2\n":
                command_output(process_id, "stdout", "4\n>>> ")
            else:
                command_output(process_id, "stdout", text)
        if params.get("closeStdin"):
            finish_command(process_id, 0)
    elif method == "command/exec/resize":
        process_id = params["processId"]
        assert command_sessions[process_id]["tty"] is True
        command_sessions[process_id]["size"] = copy.deepcopy(params["size"])
    elif method == "command/exec/terminate":
        process_id = params["processId"]
        if process_id in command_sessions:
            finish_command(process_id, 143)
    elif method == "fs/readFile":
        result["dataBase64"] = base64.b64encode(pathlib.Path(params["path"]).read_bytes()).decode()
    elif method == "fs/readDirectory":
        result["entries"] = [{
            "fileName": child.name,
            "isDirectory": child.is_dir(),
            "isFile": child.is_file(),
        } for child in pathlib.Path(params["path"]).iterdir()]
    elif method == "fs/getMetadata":
        stat = pathlib.Path(params["path"]).lstat()
        result.update(
            createdAtMs=int(stat.st_ctime * 1000),
            isDirectory=pathlib.Path(params["path"]).is_dir(),
            isFile=pathlib.Path(params["path"]).is_file(),
            isSymlink=pathlib.Path(params["path"]).is_symlink(),
            modifiedAtMs=int(stat.st_mtime * 1000),
        )
    elif method == "fuzzyFileSearch":
        root = pathlib.Path(params["roots"][0])
        relative = "escape/external_secret.txt" if params["query"] == "external" else "sample.txt"
        candidate = root / relative
        result["files"] = [{
            "root": str(root),
            "path": relative,
            "match_type": "file",
            "file_name": candidate.name,
            "score": 100,
            "indices": [0, 2],
        }] if candidate.is_file() else []
    elif method == "account/rateLimits/read":
        result.update(
            accountId="fixture-account",
            ordinaryUsageAllowed=False,
            rateLimitResetCredits={"availableCount": 2, "credits": None},
            rateLimitUpsell={"message": "fixture upsell"},
            rateLimits={"limitId": "codex", "primary": {"usedPercent": 100}},
            rateLimitsByLimitId={"codex": {"limitId": "codex", "primary": {"usedPercent": 100}}},
        )
    respond(message, result)
