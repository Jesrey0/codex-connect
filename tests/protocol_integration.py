#!/usr/bin/env python3
"""Exercise the real MCP/relay/transport stack against a pinned-schema-valid peer.

Run after cargo build -p codex-connect. Requires Python jsonschema.
"""

import base64
import json
import os
import pathlib
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request

from support.mcp_client import McpClient

ROOT = pathlib.Path(__file__).resolve().parents[1]
CONTRACT = json.loads((ROOT / "config/app-server-tool-schemas.json").read_text())
EXPECTED = {
    "status", "inspect", "apply_patch", "view_image", "command.exec",
    "command.start", "command.read", "command.write", "command.resize", "command.terminate",
    "codex.work.start", "codex.work.read", "codex.work.wait",
    "codex.work.steer", "codex.work.interrupt", "codex.pendingActions.list",
    "codex.approval.respond", "codex.permissions.respond",
    "codex.elicitation.respond", "codex.userInput.respond",
    "codex.review", "codex.model.list", "codex.skills.list", "codex.usage",
}


class OperatorProtocolTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory(prefix="codex-connect-integration-")
        cls.scope = pathlib.Path(cls.directory.name)
        cls.outside = tempfile.TemporaryDirectory(prefix="codex-connect-outside-")
        cls.outside_path = pathlib.Path(cls.outside.name)
        (cls.outside_path / "external_secret.txt").write_text("secret\n")
        (cls.scope / "escape").symlink_to(cls.outside_path, target_is_directory=True)
        (cls.scope / "sample.txt").write_text("one\ntwo\nthree\n")
        cls.project = cls.scope / "project"
        cls.project.mkdir()
        cls.coverage_path = cls.scope / "fake-app-server-coverage.jsonl"
        (cls.project / "local.txt").write_text("project-local\n")
        (cls.project / "pixel.png").write_bytes(base64.b64decode(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg=="
        ))
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        cls.url = f"http://127.0.0.1:{port}"
        cls.log = tempfile.TemporaryFile(mode="w+")
        cls.process = subprocess.Popen([
            str(ROOT / "target/debug/codex-connect"), "serve", "--scope-root", str(cls.scope),
            "--codex-bin", str(ROOT / "tests/support/fake_codex.py"), "--listen", f"127.0.0.1:{port}",
        ], stdout=cls.log, stderr=cls.log, env={
            **os.environ,
            "CODEX_CONNECT_FAKE_COVERAGE_FILE": str(cls.coverage_path),
        })
        try:
            for _ in range(200):
                if cls.process.poll() is not None:
                    raise RuntimeError("fixture backend exited")
                try:
                    with urllib.request.urlopen(cls.url + "/healthz", timeout=1):
                        break
                except (urllib.error.URLError, TimeoutError):
                    time.sleep(0.025)
            else:
                raise RuntimeError("fixture backend did not become healthy")
            cls.client = McpClient(cls.url)
        except Exception:
            cls.tearDownClass()
            raise

    @classmethod
    def tearDownClass(cls):
        if cls.process.poll() is None:
            cls.process.terminate()
        cls.process.wait(timeout=5)
        cls.log.seek(0)
        output = cls.log.read()
        if "Traceback" in output:
            print(output)
        cls.log.close()
        cls.directory.cleanup()
        cls.outside.cleanup()

    def start(self, scenario, **arguments):
        arguments.setdefault(
            "sandboxPolicy",
            {"type": "workspaceWrite", "networkAccess": True},
        )
        return self.client.call("codex.work.start", {"task": scenario, **arguments})

    def wait(self, work, timeout=1000, **arguments):
        return self.client.call("codex.work.wait", {
            "threadId": work["threadId"], "turnId": work["turnId"], "afterCursor": work["cursor"],
            "timeoutMs": timeout, **arguments,
        })

    def test_catalog_and_status_are_canonical(self):
        self.assertEqual(len(self.client.catalog), 24)
        self.assertEqual(set(self.client.tools), EXPECTED)
        wait_timeout = self.client.tools["codex.work.wait"]["inputSchema"]["properties"]["timeoutMs"]
        self.assertEqual(wait_timeout["default"], 60000)
        self.assertEqual(wait_timeout["maximum"], 120000)
        start_size = self.client.tools["command.start"]["inputSchema"]["properties"]["size"]["anyOf"][0]
        resize = self.client.tools["command.resize"]["inputSchema"]["properties"]
        self.assertEqual(start_size["properties"]["rows"]["minimum"], 1)
        self.assertEqual(start_size["properties"]["cols"]["minimum"], 1)
        self.assertEqual(resize["rows"]["minimum"], 1)
        self.assertEqual(resize["cols"]["minimum"], 1)
        work_start = self.client.tools["codex.work.start"]["inputSchema"]
        self.assertIn("sandboxPolicy", work_start["required"])
        self.client.call(
            "codex.work.start",
            {"task": "no_event"},
            error=True,
            validate_input=False,
        )
        status = self.client.call("status")
        self.assertTrue(status["healthy"])
        self.assertTrue(status["experimentalApi"])
        self.assertEqual(set(status), {
            "healthy", "scopeRoot", "buildId", "binarySha256",
            "executable", "appServerTransport", "experimentalApi",
        })
        with urllib.request.urlopen(self.url + "/status") as response:
            self.assertEqual(status, json.load(response))
        with self.assertRaises(urllib.error.HTTPError) as error:
            urllib.request.urlopen(self.url + "/scope-info")
        self.assertEqual(error.exception.code, 404)

    def test_inspection_uses_advertised_camel_case_and_scope(self):
        result = self.client.call("inspect", {"operations": [
            {"type": "readText", "path": "sample.txt", "startLine": 2, "endLine": 2},
            {"type": "searchContent", "query": "two", "maxResults": 1},
            {"type": "metadata", "path": "sample.txt"},
            {"type": "readDirectory", "path": "."},
            {"type": "fuzzyFileSearch", "query": "smp", "path": "."},
        ]})
        self.assertEqual(result["results"][0]["result"]["text"], "two")
        self.assertEqual(set(result["results"][2]["result"]), {
            "createdAtMs", "isDirectory", "isFile", "isSymlink", "modifiedAtMs",
        })
        self.assertTrue(result["results"][2]["result"]["isFile"])
        self.assertIn(
            "sample.txt",
            {entry["fileName"] for entry in result["results"][3]["result"]["entries"]},
        )
        fuzzy = result["results"][4]["result"]["files"][0]
        self.assertEqual(fuzzy["path"], "sample.txt")
        self.assertEqual(fuzzy["match_type"], "file")
        self.assertEqual(fuzzy["score"], 100)
        escaped = self.client.call("inspect", {"operations": [
            {"type": "fuzzyFileSearch", "query": "external", "path": "."},
        ]})
        self.assertEqual(escaped["results"][0]["result"]["files"], [])
        large = self.scope / "large.txt"
        large.write_bytes(b"x" * (7 * 1024 * 1024))
        large_result = self.client.call("inspect", {"operations": [
            {"type": "readText", "path": "large.txt", "startLine": 1, "endLine": 1},
        ]})
        self.assertEqual(large_result["results"][0]["index"], 0)
        self.assertIn("safe fs/readFile transport limit", large_result["results"][0]["error"])
        large_directory = self.scope / "large-directory"
        large_directory.mkdir()
        suffix = "x" * 240
        for index in range(28_000):
            (large_directory / f"{index:05d}-{suffix}").touch()
        large_directory_result = self.client.call("inspect", {"operations": [
            {"type": "readDirectory", "path": "large-directory"},
        ]})
        self.assertIn("directory listing exceeds", large_directory_result["results"][0]["error"])
        healthy = self.client.call("inspect", {"operations": [
            {"type": "readText", "path": "sample.txt", "startLine": 1, "endLine": 1},
        ]})
        self.assertEqual(healthy["results"][0]["result"]["text"], "one")
        partial = self.client.call("inspect", {"operations": [
            {"type":"readText","path":"/etc/passwd"},
            {"type":"readText","path":"sample.txt","startLine":3,"endLine":3},
        ]})
        self.assertEqual(partial["results"][0]["index"], 0)
        self.assertIn("outside the configured scope root", partial["results"][0]["error"])
        self.assertEqual(partial["results"][1]["index"], 1)
        self.assertEqual(partial["results"][1]["result"]["text"], "three")
        escaped_fuzzy = self.client.call("inspect", {"operations": [
            {"type":"fuzzyFileSearch","query":"etc","path":"/etc"},
        ]})
        self.assertIn("outside the configured scope root", escaped_fuzzy["results"][0]["error"])
        self.client.call("inspect", {
            "cwd":"/etc", "operations":[{"type":"readDirectory","path":"."}],
        }, error=True)

    def test_request_cwd_applies_consistently_to_paths_patch_and_image(self):
        cwd = str(self.project)
        inspected = self.client.call("inspect", {"cwd":cwd,"operations":[
            {"type":"readText","path":"local.txt"},
            {"type":"searchContent","query":"project-local"},
        ]})
        self.assertEqual(inspected["results"][0]["result"]["text"], "project-local")
        self.assertEqual(inspected["results"][1]["result"]["matches"][0]["path"], "project/local.txt")
        self.client.call("apply_patch", {
            "cwd":cwd,
            "patch":"*** Begin Patch\n*** Add File: patch.txt\n+created\n*** End Patch",
        })
        self.assertEqual((self.project / "patch.txt").read_text(), "created\n")
        before_image_reads = sum(
            json.loads(line) == {"kind": "method", "name": "fs/readFile"}
            for line in self.coverage_path.read_text().splitlines()
        )
        image = self.client.call("view_image", {"cwd":cwd,"path":"pixel.png"})
        self.assertEqual(image["path"], "project/pixel.png")
        self.assertEqual(image["mimeType"], "image/png")
        after_image_reads = sum(
            json.loads(line) == {"kind": "method", "name": "fs/readFile"}
            for line in self.coverage_path.read_text().splitlines()
        )
        self.assertEqual(after_image_reads, before_image_reads + 1)
        self.client.call("apply_patch", {
            "cwd":cwd,
            "patch":"*** Begin Patch\n*** Add File: ../escape.txt\n+nope\n*** End Patch",
        }, error=True)

    def test_command_boundaries(self):
        schema = self.client.tools["command.exec"]["inputSchema"]["properties"]
        self.assertNotIn("disableTimeout", schema)
        self.assertNotIn("disableOutputCap", schema)
        self.assertEqual(schema["timeoutMs"]["default"], 30000)
        self.assertEqual(schema["timeoutMs"]["maximum"], 3600000)
        self.assertEqual(schema["outputBytesCap"]["default"], 65536)
        inherited = self.client.call("command.exec", {"command":["fixture-policy"]})
        self.assertIsNone(json.loads(inherited["stdout"]))
        result = self.client.call("command.exec", {"command":["echo","fixture"]})
        self.assertEqual(result["exitCode"],0)
        self.assertEqual(self.client.call("command.exec", {
            "command":["echo","fixture"], "timeoutMs":3600000,
        })["exitCode"], 0)
        absolute_root = str(self.project)
        self.assertEqual(self.client.call("command.exec", {
            "command":["echo","fixture"],
            "sandboxPolicy":{"type":"workspaceWrite","writableRoots":[absolute_root],"networkAccess":False},
        })["exitCode"], 0)
        for arguments in [
            {"command":[]}, {"command":["echo"],"tty":True},
            {"command":["echo"],"disableTimeout":True},
            {"command":["echo"],"disableOutputCap":True},
            {"command":["echo"],"timeoutMs":3600001},
            {"command":["echo"],"sandboxPolicy":{"type":"externalSandbox"}},
            {"command":["echo"],"sandboxPolicy":{"type":"workspaceWrite","writableRoots":["project"]}},
            {"command":["echo"],"cwd":"/etc"},
        ]:
            self.client.call("command.exec",arguments,error=True,validate_input=False)

    def test_persistent_nonpty_streaming_and_sandbox(self):
        started = self.client.call("command.start", {
            "command": ["fixture-stream"],
            "sandboxPolicy": {"type": "workspaceWrite", "writableRoots": [str(self.project)], "networkAccess": False},
        })
        self.assertEqual(started["state"], "running")
        self.assertFalse(started["tty"])
        cursor = started["cursor"]
        stdout = ""
        stderr = ""
        for _ in range(2):
            result = self.client.call("command.read", {
                "processId": started["processId"], "afterCursor": cursor, "timeoutMs": 1000,
            })
            self.assertEqual(result["wakeReason"], "output")
            cursor = result["cursor"]
            stdout += result["stdout"]
            stderr += result["stderr"]
            if stdout == "ready\n" and stderr == "warning\n":
                break
        self.assertEqual(stdout, "ready\n")
        self.assertEqual(stderr, "warning\n")
        self.client.call("command.resize", {
            "processId": started["processId"], "rows": 24, "cols": 80,
        }, error=True)
        self.client.call("command.terminate", {"processId": started["processId"]})
        exited = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": cursor, "timeoutMs": 1000,
        })
        self.assertEqual(exited["state"], "exited")
        self.assertEqual(exited["wakeReason"], "exit")
        self.assertEqual(exited["exitCode"], 143)

        sandboxed = self.client.call("command.start", {
            "command": ["fixture-sandbox"],
            "sandboxPolicy": {"type": "readOnly", "networkAccess": False},
        })
        sandbox_output = self.client.call("command.read", {
            "processId": sandboxed["processId"], "timeoutMs": 1000,
        })
        self.assertIn('"type": "readOnly"', sandbox_output["stdout"])
        self.assertIn('"networkAccess": false', sandbox_output["stdout"])
        self.client.call("command.terminate", {"processId": sandboxed["processId"]})

        context = self.client.call("command.start", {
            "command": ["fixture-context"],
            "cwd": "project",
            "env": {"CC_TEST": "yes", "CC_UNSET": None},
        })
        context_output = self.client.call("command.read", {
            "processId": context["processId"], "timeoutMs": 1000,
        })
        decoded = json.loads(context_output["stdout"])
        self.assertEqual(decoded["cwd"], str(self.project))
        self.assertEqual(decoded["env"], {"CC_TEST": "yes", "CC_UNSET": None})
        self.assertIsNone(decoded["sandboxPolicy"])
        self.client.call("command.terminate", {"processId": context["processId"]})

    def test_persistent_pty_round_trip_resize_and_close(self):
        early = self.client.call("command.start", {
            "command": ["fixture-quiet"],
            "tty": True,
            "size": {"rows": 24, "cols": 80},
        })
        self.client.call("command.resize", {
            "processId": early["processId"], "rows": 33, "cols": 99,
        })
        self.client.call("command.terminate", {"processId": early["processId"]})

        started = self.client.call("command.start", {
            "command": ["fixture-repl"],
            "tty": True,
            "size": {"rows": 24, "cols": 80},
        })
        prompt = self.client.call("command.read", {
            "processId": started["processId"], "timeoutMs": 1000,
        })
        self.assertEqual(prompt["stdout"], ">>> ")
        self.assertEqual(prompt["stderr"], "")
        self.client.call("command.resize", {
            "processId": started["processId"], "rows": 40, "cols": 120,
        })
        self.client.call("command.write", {
            "processId": started["processId"], "input": "2+2\n",
        })
        answer = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": prompt["cursor"], "timeoutMs": 1000,
        })
        self.assertEqual(answer["stdout"], "4\n>>> ")
        self.client.call("command.write", {
            "processId": started["processId"], "closeStdin": True,
        })
        exited = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": answer["cursor"], "timeoutMs": 1000,
        })
        self.assertEqual(exited["state"], "exited")
        self.assertEqual(exited["exitCode"], 0)
        for tool, args in [
            ("command.write", {"processId": started["processId"], "input": "x"}),
            ("command.resize", {"processId": started["processId"], "rows": 1, "cols": 1}),
            ("command.terminate", {"processId": started["processId"]}),
        ]:
            self.client.call(tool, args, error=True)

    def test_persistent_read_timeout_exit_and_invalid_handle(self):
        quiet = self.client.call("command.start", {"command": ["fixture-quiet"]})
        timeout = self.client.call("command.read", {
            "processId": quiet["processId"], "timeoutMs": 100,
        })
        self.assertEqual(timeout["state"], "running")
        self.assertEqual(timeout["wakeReason"], "timeout")
        self.assertEqual(timeout["stdout"], "")
        self.client.call("command.terminate", {"processId": quiet["processId"]})

        delayed = self.client.call("command.start", {"command": ["fixture-delayed-exit"]})
        exited = self.client.call("command.read", {
            "processId": delayed["processId"], "timeoutMs": 3000,
        })
        self.assertEqual(exited["state"], "exited")
        self.assertEqual(exited["wakeReason"], "exit")

        immediate = self.client.call("command.start", {"command": ["fixture-exit"]})
        immediate_result = self.client.call("command.read", {
            "processId": immediate["processId"], "timeoutMs": 1000,
        })
        self.assertEqual(immediate_result["stdout"], "done\n")
        if immediate_result["state"] == "running":
            immediate_result = self.client.call("command.read", {
                "processId": immediate["processId"],
                "afterCursor": immediate_result["cursor"],
                "timeoutMs": 1000,
            })
        self.assertEqual(immediate_result["state"], "exited")
        second = self.client.call("command.read", {
            "processId": immediate["processId"],
            "afterCursor": immediate_result["cursor"],
            "timeoutMs": 0,
        })
        self.assertEqual(second["state"], "exited")
        self.assertEqual(second["wakeReason"], "exit")
        self.assertEqual(second["cursor"], immediate_result["cursor"])
        self.assertEqual(second["stdout"], "")
        self.assertEqual(second["stderr"], "")
        self.client.call("command.read", {"processId": "missing", "timeoutMs": 0}, error=True)

    def test_persistent_output_retention_is_bounded(self):
        started = self.client.call("command.start", {"command": ["fixture-bounded"]})
        time.sleep(0.2)
        result = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": 0, "timeoutMs": 1000,
        })
        self.assertTrue(result["historyLost"])
        self.assertLessEqual(len(result["stdout"].encode()), 128 * 1024)
        stale = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": 1, "timeoutMs": 0,
        })
        self.assertTrue(stale["historyLost"])
        self.client.call("command.terminate", {"processId": started["processId"]})

    def test_persistent_command_validation_and_independent_reads(self):
        for arguments in [
            {"command": []},
            {"command": [""]},
            {"command": ["fixture-quiet"], "tty": True, "size": {"rows": 0, "cols": 80}},
            {"command": ["fixture-quiet"], "tty": True, "size": {"rows": 24, "cols": 0}},
        ]:
            self.client.call("command.start", arguments, error=True, validate_input=False)

        started = self.client.call("command.start", {"command": ["fixture-stream"]})
        for _ in range(4):
            first = self.client.call("command.read", {
                "processId": started["processId"], "afterCursor": 0, "timeoutMs": 1000,
            })
            if first["stdout"] == "ready\n" and first["stderr"] == "warning\n":
                break
        self.assertEqual(first["stdout"], "ready\n")
        self.assertEqual(first["stderr"], "warning\n")
        second = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": 0, "timeoutMs": 1000,
        })
        self.assertEqual(second["cursor"], first["cursor"])
        self.assertEqual(second["stdout"], first["stdout"])
        self.assertEqual(second["stderr"], first["stderr"])

        self.client.call("command.write", {
            "processId": started["processId"], "input": None, "closeStdin": False,
        }, error=True)
        self.client.call("command.write", {
            "processId": started["processId"], "input": "x" * (64 * 1024 + 1),
        }, error=True, validate_input=False)
        self.client.call("command.resize", {
            "processId": started["processId"], "rows": 0, "cols": 80,
        }, error=True, validate_input=False)
        self.client.call("command.terminate", {"processId": started["processId"]})

    def test_authoritative_completion_without_events_and_unknown_turn(self):
        work = self.start("no_event")
        result = self.wait(work)
        self.assertEqual(result["state"],"terminal")
        self.assertEqual(result["wakeReason"],"terminal")
        self.assertEqual(result["turn"]["output"][0]["text"],"fixture complete")
        self.client.call("codex.work.wait",{"threadId":work["threadId"],"turnId":"missing","timeoutMs":0},error=True)
        next_work = self.start("idle",threadId=work["threadId"])
        self.assertFalse(next_work["createdThread"])
        idle = self.wait(next_work,timeout=0)
        self.assertEqual(idle["state"],"active")
        self.assertEqual(idle["wakeReason"],"timeout")
        read = self.client.call("codex.work.read",{"threadId":work["threadId"]})
        self.assertEqual(read["latestTurn"]["id"], next_work["turnId"])

    def test_wait_uses_paginated_turn_lookup(self):
        work = self.start("inflate_history")
        result = self.wait(work, timeout=1000)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["turnId"], work["turnId"])

    def test_wait_ignores_progress_until_lease_expiry_and_preserves_journal(self):
        for scenario in ("idle","progress","oversized"):
            work = self.start(scenario)
            result = self.wait(work,timeout=100)
            self.assertEqual(result["state"],"active")
            self.assertEqual(result["wakeReason"],"timeout")
            if scenario == "progress":
                self.assertIn("turn/started", [event["method"] for event in result["events"]])
                self.assertIn("item/agentMessage/delta", [event["method"] for event in result["events"]])
            if scenario == "oversized":
                self.assertTrue(any(event["truncated"] for event in result["events"]))
            self.client.call("codex.work.steer",{"threadId":work["threadId"],"expectedTurnId":work["turnId"],"instruction":"continue"})
            self.client.call("codex.work.interrupt",{"threadId":work["threadId"],"turnId":work["turnId"]})
            interrupted = self.wait(work)
            self.assertEqual(interrupted["state"],"terminal")
            self.assertEqual(interrupted["wakeReason"],"terminal")
            self.assertEqual(interrupted["turn"]["status"],"interrupted")

    def test_oversized_wire_messages_are_contained(self):
        notification = self.start("wire_oversized")
        notification_result = self.wait(notification, timeout=5000)
        self.assertEqual(notification_result["state"], "terminal")
        self.assertEqual(notification_result["wakeReason"], "terminal")
        self.assertTrue(notification_result["historyLost"])

        request = self.start("oversized_question")
        request_result = self.wait(request, timeout=5000)
        if request_result["state"] != "terminal":
            request_result = self.wait(
                request,
                timeout=5000,
                afterCursor=request_result["cursor"],
            )
        self.assertEqual(request_result["state"], "terminal")
        self.assertEqual(request_result["pendingActions"], [])

        status = self.client.call("status")
        self.assertTrue(status["healthy"])

    def test_questions_wake_wait_and_preserve_the_same_turn(self):
        work = self.start("delayed_question")
        result = self.wait(work,timeout=3000)
        self.assertEqual(result["state"],"active")
        self.assertEqual(result["wakeReason"],"inputRequired")
        pending = result["pendingActions"][0]
        self.assertEqual(pending["params"]["questions"][0]["id"],"format")
        self.assertTrue(pending["isBlocking"])
        request_id = pending["requestId"]
        self.client.call("codex.approval.respond",{"requestId":request_id,"decision":"approve"},error=True)
        self.client.call("codex.userInput.respond",{"requestId":request_id,"answers":{"wrong":["JSON"]}},error=True)
        self.client.call("codex.userInput.respond",{"requestId":request_id,"answers":{"format":["JSON"]}})
        completed = self.wait(work)
        self.assertEqual(completed["state"],"terminal")
        self.assertEqual(completed["wakeReason"],"terminal")
        self.client.call("codex.userInput.respond",{"requestId":request_id,"answers":{"format":["JSON"]}},error=True)
        actions = self.client.call("codex.pendingActions.list",{"threadId":work["threadId"]})["actions"]
        self.assertEqual(actions,[])

    def test_nonblocking_question_does_not_wake_join_and_interrupt_cleans_up(self):
        work = self.start("nonblocking")
        result = self.wait(work,timeout=100)
        self.assertEqual(result["state"],"active")
        self.assertEqual(result["wakeReason"],"timeout")
        self.assertFalse(result["pendingActions"][0]["isBlocking"])
        self.client.call("codex.work.interrupt",{"threadId":work["threadId"],"turnId":work["turnId"]})
        self.assertEqual(self.client.call("codex.pendingActions.list",{"threadId":work["threadId"]})["actions"],[])

    def test_typed_approval_permission_and_elicitation_wire_responses(self):
        cases = [
            ("approval","codex.approval.respond",{"decision":"approve"}),
            ("file","codex.approval.respond",{"decision":"decline"}),
            ("permissions","codex.permissions.respond",{"permissions":{"network":{"enabled":True}},"scope":"turn"}),
            ("form","codex.elicitation.respond",{"action":"accept","content":{"name":"Ada"}}),
            ("openai_form","codex.elicitation.respond",{"action":"accept","content":{"name":"Ada"}}),
            ("url","codex.elicitation.respond",{"action":"accept"}),
        ]
        for scenario, responder, answer in cases:
            with self.subTest(scenario=scenario):
                work = self.start(scenario)
                result = self.wait(work)
                self.assertEqual(result["state"],"active")
                self.assertEqual(result["wakeReason"],"actionRequired")
                request_id = result["pendingActions"][0]["requestId"]
                self.client.call(responder,{"requestId":request_id,**answer})
                completed = self.wait(work)
                self.assertEqual(completed["state"],"terminal")
                self.assertEqual(completed["wakeReason"],"terminal")

    def test_review_and_discovery(self):
        source = self.start("complete")
        source_result = self.wait(source)
        self.assertEqual(source_result["state"],"terminal")
        self.assertEqual(source_result["wakeReason"],"terminal")
        review = self.client.call("codex.review",{
            "threadId": source["threadId"], "target":{"type":"uncommittedChanges"},
        })
        self.assertFalse(review["createdThread"])
        self.assertEqual(review["threadId"], source["threadId"])
        result = self.wait(review)
        self.assertEqual(result["state"],"terminal")
        self.assertEqual(result["wakeReason"],"terminal")
        self.assertEqual(result["threadId"], source["threadId"])
        self.assertEqual(result["turnId"], review["turnId"])
        self.client.call("codex.model.list")
        self.client.call("codex.skills.list")
        usage = self.client.call("codex.usage")
        self.assertFalse(usage["ordinaryUsageAllowed"])
        self.assertEqual(usage["rateLimitResetCredits"]["availableCount"], 2)
        self.assertEqual(usage["accountId"], "fixture-account")

    def test_wait_tracks_review_turn_before_thread_history_catches_up(self):
        source = self.start("complete")
        self.assertEqual(self.wait(source)["state"], "terminal")
        review = self.client.call("codex.review", {
            "threadId": source["threadId"],
            "target": {"type": "custom", "instructions": "delayed_visibility"},
        })
        result = self.wait(review, timeout=1000)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["wakeReason"], "terminal")
        self.assertEqual(result["turnId"], review["turnId"])
        self.assertEqual(result["turn"]["status"], "failed")

    def test_contract_surface_is_completely_reachable_and_schema_valid(self):
        inspected = self.client.call("inspect", {"operations": [
            {"type": "readText", "path": "sample.txt"},
            {"type": "readDirectory", "path": "."},
            {"type": "metadata", "path": "sample.txt"},
            {"type": "fuzzyFileSearch", "query": "sample", "path": "."},
        ]})
        self.assertEqual(len(inspected["results"]), 4)

        source = self.start("complete")
        self.assertEqual(self.wait(source)["state"], "terminal")
        self.client.call("codex.work.read", {"threadId": source["threadId"]})

        resumed = self.start("idle", threadId=source["threadId"])
        self.client.call("codex.work.steer", {
            "threadId": resumed["threadId"],
            "expectedTurnId": resumed["turnId"],
            "instruction": "continue",
        })
        self.client.call("codex.work.interrupt", {
            "threadId": resumed["threadId"], "turnId": resumed["turnId"],
        })
        self.assertEqual(self.wait(resumed)["state"], "terminal")

        review = self.client.call("codex.review", {
            "threadId": source["threadId"], "target": {"type": "uncommittedChanges"},
        })
        self.assertEqual(self.wait(review)["state"], "terminal")

        self.client.call("command.exec", {"command": ["echo", "coverage"]})
        repl = self.client.call("command.start", {
            "command": ["fixture-repl"], "tty": True, "size": {"rows": 24, "cols": 80},
        })
        self.client.call("command.read", {"processId": repl["processId"], "timeoutMs": 1000})
        self.client.call("command.resize", {"processId": repl["processId"], "rows": 30, "cols": 100})
        self.client.call("command.write", {
            "processId": repl["processId"], "input": "2+2\n", "closeStdin": True,
        })
        self.client.call("command.read", {"processId": repl["processId"], "timeoutMs": 1000})
        quiet = self.client.call("command.start", {"command": ["fixture-quiet"]})
        self.client.call("command.terminate", {"processId": quiet["processId"]})
        self.client.call("command.read", {"processId": quiet["processId"], "timeoutMs": 1000})

        for scenario, responder, answer in [
            ("approval", "codex.approval.respond", {"decision": "approve"}),
            ("file", "codex.approval.respond", {"decision": "decline"}),
            ("permissions", "codex.permissions.respond", {"permissions": {"network": {"enabled": True}}, "scope": "turn"}),
            ("form", "codex.elicitation.respond", {"action": "accept", "content": {"name": "Ada"}}),
            ("question", "codex.userInput.respond", {"answers": {"format": ["JSON"]}}),
        ]:
            work = self.start(scenario)
            pending = self.wait(work)["pendingActions"][0]
            self.client.call(responder, {"requestId": pending["requestId"], **answer})
            self.assertEqual(self.wait(work)["state"], "terminal")

        self.client.call("codex.model.list")
        self.client.call("codex.skills.list")
        self.client.call("codex.usage")

        observed = {"method": set(), "serverRequest": set(), "notification": set()}
        for line in self.coverage_path.read_text().splitlines():
            entry = json.loads(line)
            observed[entry["kind"]].add(entry["name"])
        self.assertEqual(observed["method"], set(CONTRACT["methods"]))
        self.assertEqual(observed["serverRequest"], set(CONTRACT["serverRequests"]))
        self.assertEqual(observed["notification"], set(CONTRACT["notifications"]))

    def test_z_disconnect_exits_backend_for_service_recovery(self):
        self.start("question")
        persistent = self.client.call("command.start", {"command": ["fixture-quiet"]})
        self.assertEqual(persistent["state"], "running")
        try:
            self.client.call("command.exec",{"command":["disconnect"]},error=True)
        except (urllib.error.URLError, ConnectionError):
            pass
        self.assertNotEqual(self.process.wait(timeout=5),0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
