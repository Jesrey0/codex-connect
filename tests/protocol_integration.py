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
import urllib.parse
import urllib.request

from support.mcp_client import McpClient

ROOT = pathlib.Path(__file__).resolve().parents[1]
CONTRACT = json.loads((ROOT / "config/app-server-tool-schemas.json").read_text())
EXPECTED = {
    "status", "inspect", "apply_patch", "view_image", "command.exec",
    "command.start", "command.read", "command.control",
    "codex.start", "codex.wait", "codex.inspect", "codex.control", "codex.action.respond", "codex.info",
}


class OperatorProtocolTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory(prefix="codex-connect-integration-")
        cls.workspace = pathlib.Path(cls.directory.name)
        cls.outside = tempfile.TemporaryDirectory(prefix="codex-connect-outside-")
        cls.outside_path = pathlib.Path(cls.outside.name)
        (cls.outside_path / "external_secret.txt").write_text("secret\n")
        (cls.workspace / "escape").symlink_to(cls.outside_path, target_is_directory=True)
        (cls.workspace / "sample.txt").write_text("one\ntwo\nthree\n")
        cls.project = cls.workspace / "project"
        cls.project.mkdir()
        cls.coverage_path = cls.workspace / "fake-app-server-coverage.jsonl"
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
            str(ROOT / "target/debug/codex-connect"), "serve", "--default-cwd", str(cls.workspace),
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
        return self.client.call("codex.start", {"mode": "work", "task": scenario, **arguments})

    def wait(self, work, timeout=1000, **arguments):
        return self.client.call("codex.wait", {
            "threadId": work["threadId"], "turnId": work["turnId"],
            "timeoutMs": timeout, **arguments,
        })

    def inspect_turn(self, work, detail="semantic", after_cursor=None):
        return self.client.call("codex.inspect", {
            "threadId": work["threadId"],
            "turnId": work["turnId"],
            "afterCursor": work["cursor"] if after_cursor is None else after_cursor,
            "detail": detail,
        })

    def transcript(self, work):
        thread_id = urllib.parse.quote(work["threadId"], safe="")
        turn_id = urllib.parse.quote(work["turnId"], safe="")
        with urllib.request.urlopen(
            f"{self.url}/observe/transcript/{thread_id}/{turn_id}"
        ) as response:
            return json.load(response)

    def method_params(self, method):
        return [
            entry["params"]
            for entry in (
                json.loads(line) for line in self.coverage_path.read_text().splitlines()
            )
            if entry["kind"] == "method" and entry["name"] == method and "params" in entry
        ]

    def wait_for_method_count(self, method, count, timeout=2.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            params = self.method_params(method)
            if len(params) >= count:
                return params
            time.sleep(0.025)
        self.fail(f"{method} did not reach {count} calls")

    def wait_for_worker_event(self, predicate, timeout=2.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            status = self.client.call("status")
            for event in status.get("workerEvents", []):
                if predicate(event):
                    return event
            time.sleep(0.025)
        self.fail("worker event was not delivered on the host plane")

    def test_catalog_and_status_are_canonical(self):
        self.assertEqual(len(self.client.catalog), 14)
        self.assertEqual(set(self.client.tools), EXPECTED)
        wait_timeout = self.client.tools["codex.wait"]["inputSchema"]["properties"]["timeoutMs"]
        self.assertEqual(wait_timeout["default"], 60000)
        self.assertEqual(wait_timeout["maximum"], 120000)
        self.assertEqual(wait_timeout["minimum"], 1)
        self.assertNotIn("afterCursor", self.client.tools["codex.wait"]["inputSchema"]["properties"])
        self.assertEqual(
            self.client.tools["codex.inspect"]["inputSchema"]["properties"]["detail"]["default"],
            "semantic",
        )
        exec_timeout = self.client.tools["command.exec"]["inputSchema"]["properties"]["timeoutMs"]
        self.assertEqual(exec_timeout["default"], 60000)
        self.assertEqual(exec_timeout["maximum"], 300000)
        read_timeout = self.client.tools["command.read"]["inputSchema"]["properties"]["timeoutMs"]
        self.assertEqual(read_timeout["default"], 30000)
        self.assertEqual(read_timeout["maximum"], 120000)
        start_size = self.client.tools["command.start"]["inputSchema"]["properties"]["size"]["anyOf"][0]
        resize = self.client.tools["command.control"]["inputSchema"]["oneOf"][1]["properties"]
        self.assertEqual(start_size["properties"]["rows"]["minimum"], 1)
        self.assertEqual(start_size["properties"]["cols"]["minimum"], 1)
        self.assertEqual(resize["rows"]["minimum"], 1)
        self.assertEqual(resize["cols"]["minimum"], 1)
        work_start = self.client.tools["codex.start"]["inputSchema"]["oneOf"][0]
        review_start = self.client.tools["codex.start"]["inputSchema"]["oneOf"][1]
        self.assertNotIn("approvalPolicy", work_start["properties"])
        self.assertIn("sandboxPolicy", work_start["required"])
        self.assertIn('"readOnly"', json.dumps(work_start["properties"]["sandboxPolicy"]))
        for host_tool in ("command.exec", "command.start"):
            self.assertNotIn(
                "sandboxPolicy",
                self.client.tools[host_tool]["inputSchema"]["properties"],
            )
        self.assertIn("model", review_start["properties"])
        self.assertNotIn("sandboxPolicy", review_start["properties"])
        self.assertNotIn("effort", review_start["properties"])
        self.assertNotIn("serviceTier", review_start["properties"])
        self.client.call(
            "codex.start",
            {"mode": "work", "task": "no_event"},
            error=True,
            validate_input=False,
        )
        status = self.client.call("status")
        worker_events = status.pop("workerEvents", [])
        for event in worker_events:
            self.assertIn(event["kind"], {"turnTerminal", "actionRequired", "historyLost"})
        self.assertTrue(status["ready"])
        self.assertEqual(set(status), {"ready", "cwd", "buildId", "codex"})
        self.assertEqual(status["cwd"], str(self.workspace))
        self.assertEqual(set(status["codex"]), {"release", "defaults"})
        self.assertEqual(set(status["codex"]["defaults"]), {
            "model", "reasoningEffort", "serviceTier", "source",
        })
        self.assertIn(status["codex"]["defaults"]["source"], {"userConfig", "upstream"})
        with urllib.request.urlopen(self.url + "/runtime") as response:
            runtime = json.load(response)
        self.assertTrue(runtime["ready"])
        self.assertEqual(runtime["cwd"], str(self.workspace))
        self.assertIn("binarySha256", runtime)
        self.assertEqual(runtime["appServerTransport"], "stdio")
        self.assertTrue(runtime["experimentalApi"])
        self.assertIn('sandbox_mode="danger-full-access"', runtime["appServer"]["launchOverrides"])
        request = urllib.request.Request(
            self.url + "/mcp",
            data=b"{}",
            headers={"Content-Type": "application/json", "Origin": "https://evil.example"},
            method="POST",
        )
        with self.assertRaises(urllib.error.HTTPError) as rejected:
            urllib.request.urlopen(request)
        self.assertEqual(rejected.exception.code, 403)
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        self.assertEqual(observer["runtime"], runtime)
        projection = observer["projection"]
        self.assertEqual(set(projection), {
            "cwd", "usage", "usageRefreshMs", "activeTurns",
            "pendingActions", "cursor", "historyLost", "events",
        })
        self.assertEqual(projection["cwd"], str(self.workspace))
        self.assertEqual(projection["usageRefreshMs"], 5000)
        self.assertIn("rateLimits", projection["usage"])
    def test_inspection_uses_default_cwd_and_accepts_absolute_host_paths(self):
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
        large = self.workspace / "large.txt"
        large.write_bytes(b"x" * (7 * 1024 * 1024))
        large_result = self.client.call("inspect", {"operations": [
            {"type": "readText", "path": "large.txt", "startLine": 1, "endLine": 1},
        ]})
        self.assertEqual(large_result["results"][0]["index"], 0)
        self.assertIn("safe fs/readFile transport limit", large_result["results"][0]["error"])
        large_directory = self.workspace / "large-directory"
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
        self.assertIn("root:", partial["results"][0]["result"]["text"])
        self.assertEqual(partial["results"][1]["index"], 1)
        self.assertEqual(partial["results"][1]["result"]["text"], "three")
        escaped_fuzzy = self.client.call("inspect", {"operations": [
            {"type":"fuzzyFileSearch","query":"passwd","path":"/etc"},
        ]})
        self.assertEqual(escaped_fuzzy["results"][0]["result"]["files"][0]["root"], "/etc")
        outside_cwd = self.client.call("inspect", {
            "cwd":"/etc", "operations":[{"type":"readDirectory","path":"."}],
        })
        self.assertIn("passwd", {entry["fileName"] for entry in outside_cwd["results"][0]["result"]["entries"]})

    def test_request_cwd_applies_consistently_to_paths_patch_and_image(self):
        cwd = str(self.project)
        inspected = self.client.call("inspect", {"cwd":cwd,"operations":[
            {"type":"readText","path":"local.txt"},
            {"type":"searchContent","query":"project-local"},
        ]})
        self.assertEqual(inspected["results"][0]["result"]["text"], "project-local")
        self.assertEqual(inspected["results"][1]["result"]["matches"][0]["path"], "local.txt")
        self.client.call("apply_patch", {
            "cwd":cwd,
            "patch":"*** Begin Patch\n*** Add File: patch.txt\n+created\n*** End Patch",
        })
        self.assertEqual((self.project / "patch.txt").read_text(), "created\n")
        before_image_reads = sum(
            (entry := json.loads(line))["kind"] == "method" and entry["name"] == "fs/readFile"
            for line in self.coverage_path.read_text().splitlines()
        )
        image = self.client.call("view_image", {"cwd":cwd,"path":"pixel.png"})
        self.assertEqual(image["path"], "project/pixel.png")
        self.assertEqual(image["mimeType"], "image/png")
        after_image_reads = sum(
            (entry := json.loads(line))["kind"] == "method" and entry["name"] == "fs/readFile"
            for line in self.coverage_path.read_text().splitlines()
        )
        self.assertEqual(after_image_reads, before_image_reads + 1)
        self.client.call("apply_patch", {
            "cwd":cwd,
            "patch":"*** Begin Patch\n*** Add File: ../escape.txt\n+outside-cwd\n*** End Patch",
        })
        self.assertEqual((self.workspace / "escape.txt").read_text(), "outside-cwd\n")

    def test_command_boundaries(self):
        schema = self.client.tools["command.exec"]["inputSchema"]["properties"]
        self.assertNotIn("disableTimeout", schema)
        self.assertNotIn("disableOutputCap", schema)
        self.assertEqual(schema["timeoutMs"]["default"], 60000)
        self.assertEqual(schema["timeoutMs"]["maximum"], 300000)
        self.assertEqual(schema["outputBytesCap"]["default"], 65536)
        inherited = self.client.call("command.exec", {"command":["fixture-policy"]})
        self.assertIsNone(json.loads(inherited["stdout"]))
        result = self.client.call("command.exec", {"command":["echo","fixture"]})
        self.assertEqual(result["exitCode"],0)
        self.assertEqual(result["stdoutBytes"], len(result["stdout"].encode()))
        self.assertEqual(result["stderrBytes"], len(result["stderr"].encode()))
        self.assertFalse(result["stdoutMayBeTruncated"])
        self.assertFalse(result["stderrMayBeTruncated"])
        self.assertGreaterEqual(result["durationMs"], 0)
        self.assertEqual(self.client.call("command.exec", {
            "command":["echo","fixture"], "timeoutMs":300000,
        })["exitCode"], 0)
        self.assertEqual(self.client.call("command.exec", {
            "command":["echo","fixture"], "cwd":"/etc",
        })["exitCode"], 0)
        for arguments in [
            {"command":[]}, {"command":["echo"],"tty":True},
            {"command":["echo"],"disableTimeout":True},
            {"command":["echo"],"disableOutputCap":True},
            {"command":["echo"],"timeoutMs":300001},
            {"command":["echo"],"sandboxPolicy":{"type":"externalSandbox"}},
            {"command":["echo"],"sandboxPolicy":{"type":"workspaceWrite","writableRoots":["project"]}},
        ]:
            self.client.call("command.exec",arguments,error=True,validate_input=False)

    def test_persistent_nonpty_streaming_and_host_context(self):
        started = self.client.call("command.start", {
            "command": ["fixture-stream"],
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
        self.client.call("command.control", {
            "action": "resize", "processId": started["processId"], "rows": 24, "cols": 80,
        }, error=True)
        self.client.call("command.control", {"action": "terminate", "processId": started["processId"]})
        exited = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": cursor, "timeoutMs": 1000,
        })
        self.assertEqual(exited["state"], "exited")
        self.assertEqual(exited["wakeReason"], "exit")
        self.assertEqual(exited["exitCode"], 143)
        self.client.call("command.read", {
            "processId": started["processId"], "timeoutMs": 120001,
        }, error=True, validate_input=False)

        self.client.call("command.start", {
            "command": ["fixture-sandbox"],
            "sandboxPolicy": {"type": "readOnly", "networkAccess": False},
        }, error=True, validate_input=False)

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
        self.client.call("command.control", {"action": "terminate", "processId": context["processId"]})

    def test_persistent_pty_round_trip_resize_and_close(self):
        early = self.client.call("command.start", {
            "command": ["fixture-quiet"],
            "tty": True,
            "size": {"rows": 24, "cols": 80},
        })
        self.client.call("command.control", {
            "action": "resize", "processId": early["processId"], "rows": 33, "cols": 99,
        })
        self.client.call("command.control", {"action": "terminate", "processId": early["processId"]})

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
        self.client.call("command.control", {
            "action": "resize", "processId": started["processId"], "rows": 40, "cols": 120,
        })
        self.client.call("command.control", {
            "action": "write", "processId": started["processId"], "input": "2+2\n",
        })
        answer = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": prompt["cursor"], "timeoutMs": 1000,
        })
        self.assertEqual(answer["stdout"], "4\n>>> ")
        self.client.call("command.control", {
            "action": "write", "processId": started["processId"], "closeStdin": True,
        })
        exited = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": answer["cursor"], "timeoutMs": 1000,
        })
        self.assertEqual(exited["state"], "exited")
        self.assertEqual(exited["exitCode"], 0)
        for args in [
            {"action": "write", "processId": started["processId"], "input": "x"},
            {"action": "resize", "processId": started["processId"], "rows": 1, "cols": 1},
            {"action": "terminate", "processId": started["processId"]},
        ]:
            self.client.call("command.control", args, error=True)

    def test_persistent_read_timeout_exit_and_invalid_handle(self):
        quiet = self.client.call("command.start", {"command": ["fixture-quiet"]})
        timeout = self.client.call("command.read", {
            "processId": quiet["processId"], "timeoutMs": 100,
        })
        self.assertEqual(timeout["state"], "running")
        self.assertEqual(timeout["wakeReason"], "timeout")
        self.assertEqual(timeout["stdout"], "")
        self.client.call("command.control", {"action": "terminate", "processId": quiet["processId"]})

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
        self.client.call("command.control", {"action": "terminate", "processId": started["processId"]})

    def test_persistent_terminal_state_does_not_imply_output_is_drained(self):
        started = self.client.call("command.start", {"command": ["fixture-drain"]})
        self.client.call("command.control", {
            "action": "write", "processId": started["processId"], "input": "produce\n",
        })

        cursor = 0
        output = ""
        for _ in range(3):
            batch = self.client.call("command.read", {
                "processId": started["processId"], "afterCursor": cursor, "timeoutMs": 1000,
            })
            self.assertEqual(batch["state"], "running")
            self.assertGreater(batch["cursor"], cursor)
            cursor = batch["cursor"]
            output += batch["stdout"]
            if len(output.encode()) == (128 * 1024) + 20000:
                break
        self.assertEqual(len(output.encode()), (128 * 1024) + 20000)

        self.client.call("command.control", {
            "action": "write", "processId": started["processId"], "closeStdin": True,
        })
        terminal = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": cursor, "timeoutMs": 1000,
        })
        self.assertEqual(terminal["state"], "exited")
        self.assertEqual(terminal["stdout"], "")

        first = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": 0, "timeoutMs": 0,
        })
        self.assertEqual(first["state"], "exited")
        self.assertTrue(first["hasMoreOutput"])
        self.assertFalse(first["drained"])
        self.assertLess(first["cursor"], terminal["cursor"])
        self.assertEqual(len(first["stdout"].encode()), 128 * 1024)

        second = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": first["cursor"], "timeoutMs": 0,
        })
        self.assertEqual(second["state"], "exited")
        self.assertFalse(second["hasMoreOutput"])
        self.assertTrue(second["drained"])
        self.assertGreater(second["cursor"], first["cursor"])
        self.assertEqual(second["stdout"], "c" * 20000)

        drained = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": second["cursor"], "timeoutMs": 0,
        })
        self.assertEqual(drained["state"], "exited")
        self.assertEqual(drained["cursor"], second["cursor"])
        self.assertFalse(drained["hasMoreOutput"])
        self.assertTrue(drained["drained"])
        self.assertEqual(drained["stdout"], "")
        self.assertEqual(drained["stderr"], "")

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

        self.client.call("command.control", {
            "action": "write", "processId": started["processId"], "input": None, "closeStdin": False,
        }, error=True)
        self.client.call("command.control", {
            "action": "write", "processId": started["processId"], "input": "x" * (64 * 1024 + 1),
        }, error=True, validate_input=False)
        self.client.call("command.control", {
            "action": "resize", "processId": started["processId"], "rows": 0, "cols": 80,
        }, error=True, validate_input=False)
        self.client.call("command.control", {"action": "terminate", "processId": started["processId"]})

    def test_authoritative_completion_without_events_and_unknown_turn(self):
        work = self.start("no_event")
        result = self.wait(work)
        self.assertEqual(result["state"],"terminal")
        self.assertEqual(result["wakeReason"],"terminal")
        self.assertEqual(result["turn"]["output"][0]["text"],"fixture complete")
        self.client.call("codex.inspect",{"threadId":work["threadId"],"turnId":"missing"},error=True)
        next_work = self.start(
            "idle",
            threadId=work["threadId"],
            model="gpt-6-astra",
            effort="high",
            serviceTier="priority",
        )
        self.assertFalse(next_work["createdThread"])
        idle = self.inspect_turn(next_work)
        self.assertEqual(idle["status"],"inProgress")
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        observed = next(
            turn for turn in observer["projection"]["activeTurns"]
            if turn["turnId"] == next_work["turnId"]
        )
        self.assertEqual(observed["mode"], "work")
        self.assertEqual(observed["model"], "gpt-6-astra")
        self.assertEqual(observed["effort"], "high")
        self.assertEqual(observed["serviceTier"], "priority")
        self.assertGreater(observed["lastActivityAtMs"], 0)
        self.assertIn(observed["activityKind"], {"turn", "think", "message", "tool", "file", "search", "item", "waiting"})
        self.assertIn("activitySummary", observed)
        self.assertEqual(set(observed["tokenUsage"]), {"totalTokens", "modelContextWindow"})
        snapshot = self.inspect_turn(next_work)
        self.assertEqual(snapshot["turnId"], next_work["turnId"])
        self.assertEqual(snapshot["detail"], "semantic")

    def test_observer_transcript_is_live_and_survives_terminal_cleanup(self):
        work = self.start("progress")
        active = self.transcript(work)
        self.assertEqual(active["threadId"], work["threadId"])
        self.assertEqual(active["turnId"], work["turnId"])
        self.assertEqual(active["status"], "inProgress")
        self.assertEqual(active["activity"]["kind"], "message")
        self.assertIn("Working", active["activity"]["summary"])

        self.client.call("codex.control", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")
        terminal = self.transcript(work)
        self.assertEqual(terminal["status"], "interrupted")
        self.assertTrue(any(
            entry["kind"] == "agent" and entry.get("text") == "fixture complete"
            for entry in terminal["entries"]
        ))

    def test_observer_transcript_stops_hydrating_after_the_entry_cap(self):
        work = self.start("long_transcript")
        self.assertEqual(self.wait(work)["state"], "terminal")
        before = len(self.method_params("thread/items/list"))
        transcript = self.transcript(work)
        calls = self.method_params("thread/items/list")[before:]
        self.assertTrue(transcript["truncated"])
        self.assertEqual(len(transcript["entries"]), 512)
        self.assertIn(13, [call["limit"] for call in calls])

    def test_wait_rejects_timeout_above_server_limit(self):
        work = self.start("idle")
        reads_before = len(self.method_params("thread/read"))
        self.client.call("codex.wait", {
            "threadId": work["threadId"],
            "turnId": work["turnId"],
            "timeoutMs": 120001,
        }, error=True, validate_input=False)
        self.assertEqual(len(self.method_params("thread/read")), reads_before)
        self.client.call("codex.wait", {
            "threadId": work["threadId"],
            "turnId": work["turnId"],
            "timeoutMs": 0,
        }, error=True, validate_input=False)
        self.client.call("codex.control", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_observer_does_not_regress_early_completed_turn_to_in_progress(self):
        unsubscribes_before = len(self.method_params("thread/unsubscribe"))
        work = self.start(
            "early_complete",
            model="gpt-6-astra",
            effort="high",
        )
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        self.assertNotIn(
            work["turnId"],
            [turn["turnId"] for turn in observer["projection"]["activeTurns"]],
        )
        result = self.wait(work)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["turn"]["status"], "completed")
        unsubscribes = self.wait_for_method_count(
            "thread/unsubscribe", unsubscribes_before + 1,
        )
        self.assertEqual(
            unsubscribes[unsubscribes_before],
            {"threadId": work["threadId"]},
        )
        self.assertEqual(len(self.method_params("thread/unsubscribe")), unsubscribes_before + 1)

    def test_host_plane_delivers_terminal_and_action_worker_events_once(self):
        self.client.call("status")  # discard events from earlier tests
        unsubscribes_before = len(self.method_params("thread/unsubscribe"))
        complete_work = self.start("complete")
        terminal = self.wait_for_worker_event(
            lambda event: event.get("kind") == "turnTerminal"
            and event.get("turnId") == complete_work["turnId"]
        )
        self.assertEqual(terminal["mode"], "work")
        self.assertEqual(terminal["status"], "completed")
        self.assertFalse(any(
            event.get("turnId") == complete_work["turnId"]
            for event in self.client.call("status").get("workerEvents", [])
        ))
        unsubscribes = self.wait_for_method_count(
            "thread/unsubscribe", unsubscribes_before + 1,
        )
        self.assertEqual(
            unsubscribes[unsubscribes_before],
            {"threadId": complete_work["threadId"]},
        )
        self.assertEqual(self.wait(complete_work)["state"], "terminal")
        time.sleep(0.05)
        self.assertEqual(len(self.method_params("thread/unsubscribe")), unsubscribes_before + 1)

        self.client.call("status")
        approval_work = self.start("approval")
        action_event = self.wait_for_worker_event(
            lambda event: event.get("kind") == "actionRequired"
            and event.get("turnId") == approval_work["turnId"]
        )
        self.assertEqual(action_event["actionKind"], "approval")
        self.assertTrue(action_event["blocking"])
        self.assertFalse(any(
            event.get("requestId") == action_event["requestId"]
            for event in self.client.call("status").get("workerEvents", [])
        ))
        pending = self.wait(approval_work, timeout=1)["pendingActions"][0]
        self.assertEqual(pending["requestId"], action_event["requestId"])
        self.client.call("codex.action.respond", {
            "type": "approval",
            "requestId": pending["requestId"],
            "decision": "approve",
        })
        self.assertEqual(self.wait(approval_work)["state"], "terminal")

    def test_explicit_wait_acknowledges_passive_worker_events(self):
        self.client.call("status")
        complete_work = self.start("complete")
        self.assertEqual(self.wait(complete_work)["state"], "terminal")
        time.sleep(0.05)
        self.assertFalse(any(
            event.get("turnId") == complete_work["turnId"]
            for event in self.client.call("status").get("workerEvents", [])
        ))

        approval_work = self.start("approval")
        pending = self.wait(approval_work, timeout=1000)["pendingActions"][0]
        self.assertFalse(any(
            event.get("requestId") == pending["requestId"]
            for event in self.client.call("status").get("workerEvents", [])
        ))
        self.client.call("codex.action.respond", {
            "type": "approval",
            "requestId": pending["requestId"],
            "decision": "approve",
        })
        self.assertEqual(self.wait(approval_work)["state"], "terminal")

    def test_authoritative_terminal_reconciliation_releases_thread_subscription(self):
        unsubscribes_before = len(self.method_params("thread/unsubscribe"))
        work = self.start("no_event")
        self.assertEqual(self.wait(work)["state"], "terminal")
        unsubscribes = self.wait_for_method_count(
            "thread/unsubscribe", unsubscribes_before + 1,
        )
        self.assertEqual(
            unsubscribes[unsubscribes_before],
            {"threadId": work["threadId"]},
        )
        self.assertEqual(len(self.method_params("thread/unsubscribe")), unsubscribes_before + 1)
        deadline = time.time() + 2.0
        while time.time() < deadline:
            with urllib.request.urlopen(self.url + "/observe") as response:
                observer = json.load(response)
            successes = [
                event for event in observer["projection"]["events"]
                if event["kind"] == "system"
                and event["threadId"] == work["threadId"]
                and "thread unsubscribed" in (event["summary"] or "")
            ]
            if successes:
                self.assertIn("unsubscribed", successes[-1]["summary"])
                break
            time.sleep(0.025)
        else:
            self.fail("unsubscribe success was not exposed through the observer journal")

    def test_start_failure_after_thread_load_releases_subscription(self):
        unsubscribes_before = len(self.method_params("thread/unsubscribe"))
        self.client.call("codex.start", {
            "mode": "work",
            "task": "start_error",
            "sandboxPolicy": {"type": "dangerFullAccess"},
        }, error=True)
        unsubscribes = self.wait_for_method_count(
            "thread/unsubscribe", unsubscribes_before + 1,
        )
        self.assertEqual(len(unsubscribes), unsubscribes_before + 1)

    def test_unsubscribe_failure_is_visible_and_retried_after_reuse(self):
        unsubscribes_before = len(self.method_params("thread/unsubscribe"))
        work = self.start("unsubscribe_error")
        self.assertEqual(self.wait(work)["state"], "terminal")
        self.wait_for_method_count("thread/unsubscribe", unsubscribes_before + 1)

        deadline = time.time() + 2.0
        while time.time() < deadline:
            with urllib.request.urlopen(self.url + "/observe") as response:
                observer = json.load(response)
            failures = [
                event for event in observer["projection"]["events"]
                if event["kind"] == "error"
                and event["threadId"] == work["threadId"]
                and "thread unsubscribe failed" in (event["summary"] or "")
            ]
            if failures:
                break
            time.sleep(0.025)
        else:
            self.fail("unsubscribe failure was not exposed through the observer journal")

        resumed = self.start("complete", threadId=work["threadId"])
        self.assertEqual(self.wait(resumed)["state"], "terminal")
        unsubscribes = self.wait_for_method_count(
            "thread/unsubscribe", unsubscribes_before + 2,
        )
        self.assertEqual(
            unsubscribes[-1],
            {"threadId": work["threadId"]},
        )

    def test_thread_subscription_is_released_only_after_the_last_active_turn(self):
        unsubscribes_before = len(self.method_params("thread/unsubscribe"))
        first = self.start("idle")
        second = self.start("idle", threadId=first["threadId"])

        self.client.call("codex.control", {
            "action": "interrupt", "threadId": first["threadId"], "turnId": first["turnId"],
        })
        self.assertEqual(self.wait(first)["state"], "terminal")
        time.sleep(0.05)
        self.assertEqual(len(self.method_params("thread/unsubscribe")), unsubscribes_before)

        self.client.call("codex.control", {
            "action": "interrupt", "threadId": second["threadId"], "turnId": second["turnId"],
        })
        self.assertEqual(self.wait(second)["state"], "terminal")
        unsubscribes = self.wait_for_method_count(
            "thread/unsubscribe", unsubscribes_before + 1,
        )
        self.assertEqual(
            unsubscribes[unsubscribes_before],
            {"threadId": first["threadId"]},
        )
        self.assertEqual(len(self.method_params("thread/unsubscribe")), unsubscribes_before + 1)

    def test_observer_hides_unannotated_review_auxiliary_turn(self):
        review = self.client.call("codex.start", {
            "mode": "review",
            "target": {"type": "uncommittedChanges"},
            "model": "gpt-6-astra",
        })
        self.assertEqual(self.wait(review)["state"], "terminal")
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        self.assertNotIn(
            f"{review['threadId']}-review-auxiliary",
            [turn["turnId"] for turn in observer["projection"]["activeTurns"]],
        )

    def test_wait_uses_paginated_turn_lookup(self):
        work = self.start("inflate_history")
        result = self.wait(work, timeout=1000)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["turnId"], work["turnId"])

    def test_wait_ignores_progress_and_inspect_owns_worker_history(self):
        for scenario in ("idle","progress","oversized"):
            work = self.start(scenario)
            result = self.wait(work,timeout=100)
            self.assertEqual(result["state"],"active")
            self.assertEqual(result["wakeReason"],"timeout")
            self.assertNotIn("events", result)
            self.assertNotIn("cursor", result)
            self.assertIn("currentActivity", result)
            if scenario == "progress":
                semantic = self.inspect_turn(work)
                self.assertEqual(semantic["detail"], "semantic")
                self.assertIn("turn", [event["kind"] for event in semantic["events"]])
                self.assertNotIn("delta", json.dumps(semantic["events"]))
                raw = self.inspect_turn(work, detail="raw")
                self.assertIn("item/agentMessage/delta", [event["method"] for event in raw["events"]])
            if scenario == "oversized":
                raw = self.inspect_turn(work, detail="raw")
                self.assertTrue(any(event["truncated"] for event in raw["events"]))
            self.client.call("codex.control",{"action":"steer","threadId":work["threadId"],"expectedTurnId":work["turnId"],"instruction":"continue"})
            self.client.call("codex.control",{"action":"interrupt","threadId":work["threadId"],"turnId":work["turnId"]})
            interrupted = self.wait(work)
            self.assertEqual(interrupted["state"],"terminal")
            self.assertEqual(interrupted["wakeReason"],"terminal")
            self.assertEqual(interrupted["turn"]["status"],"interrupted")

    def test_quiet_wait_reconciles_status_without_rehydrating_items(self):
        work = self.start("idle")
        turns_before = len(self.method_params("thread/turns/list"))
        items_before = len(self.method_params("thread/items/list"))
        result = self.wait(work, timeout=2500)
        self.assertEqual(result["state"], "active")
        self.assertEqual(result["wakeReason"], "timeout")
        self.assertGreaterEqual(len(self.method_params("thread/turns/list")) - turns_before, 2)
        self.assertEqual(len(self.method_params("thread/items/list")) - items_before, 0)
        self.client.call("codex.control", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_terminal_wait_hydrates_items_once_after_nonterminal_reconciliation(self):
        work = self.start("delayed_complete")
        turns_before = len(self.method_params("thread/turns/list"))
        items_before = len(self.method_params("thread/items/list"))
        result = self.wait(work, timeout=3000)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["wakeReason"], "terminal")
        self.assertEqual(result["turn"]["output"][0]["text"], "fixture complete")
        self.assertGreaterEqual(len(self.method_params("thread/turns/list")) - turns_before, 2)
        self.assertEqual(len(self.method_params("thread/items/list")) - items_before, 1)

    def test_slow_reconciliation_does_not_overrun_wait_lease(self):
        work = self.start("slow_reconcile")
        started = time.monotonic()
        result = self.wait(work, timeout=1100)
        elapsed = time.monotonic() - started
        self.assertEqual(result["state"], "active")
        self.assertEqual(result["wakeReason"], "timeout")
        self.assertLess(elapsed, 1.8)
        self.client.call("codex.control", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_slow_initial_reconciliation_uses_live_turn_at_lease_expiry(self):
        work = self.start("slow_initial_reconcile")
        started = time.monotonic()
        result = self.wait(work, timeout=200)
        elapsed = time.monotonic() - started
        self.assertEqual(result["state"], "active")
        self.assertEqual(result["wakeReason"], "timeout")
        self.assertEqual(result["turn"]["id"], work["turnId"])
        self.assertLess(elapsed, 0.9)
        self.client.call("codex.control", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_oversized_wire_messages_are_contained(self):
        self.client.call("status")
        notification = self.start("wire_oversized")
        notification_result = self.wait(notification, timeout=5000)
        self.assertEqual(notification_result["state"], "terminal")
        self.assertEqual(notification_result["wakeReason"], "terminal")
        self.assertTrue(self.inspect_turn(notification)["historyLost"])
        self.assertTrue(any(
            event.get("kind") == "historyLost"
            for event in self.client.call("status").get("workerEvents", [])
        ))

        request = self.start("oversized_question")
        request_result = self.wait(request, timeout=5000)
        if request_result["state"] != "terminal":
            request_result = self.wait(request, timeout=5000)
        self.assertEqual(request_result["state"], "terminal")
        self.assertEqual(request_result["pendingActions"], [])

        status = self.client.call("status")
        self.assertTrue(status["ready"])

    def test_questions_wake_wait_and_preserve_the_same_turn(self):
        work = self.start("delayed_question")
        result = self.wait(work,timeout=3000)
        self.assertEqual(result["state"],"active")
        self.assertEqual(result["wakeReason"],"inputRequired")
        pending = result["pendingActions"][0]
        self.assertEqual(pending["params"]["questions"][0]["id"],"format")
        self.assertTrue(pending["isBlocking"])
        request_id = pending["requestId"]
        self.client.call("codex.action.respond",{"type":"approval","requestId":request_id,"decision":"approve"},error=True)
        self.client.call("codex.action.respond",{"type":"userInput","requestId":request_id,"answers":{"wrong":["JSON"]}},error=True)
        self.client.call("codex.action.respond",{"type":"userInput","requestId":request_id,"answers":{"format":["JSON"]}})
        completed = self.wait(work)
        self.assertEqual(completed["state"],"terminal")
        self.assertEqual(completed["wakeReason"],"terminal")
        self.client.call("codex.action.respond",{"type":"userInput","requestId":request_id,"answers":{"format":["JSON"]}},error=True)
        self.assertEqual(self.client.call("codex.wait",{
            "threadId": work["threadId"], "turnId": work["turnId"], "timeoutMs": 1,
        })["pendingActions"], [])

    def test_nonblocking_question_does_not_wake_join_and_interrupt_cleans_up(self):
        work = self.start("nonblocking")
        result = self.wait(work,timeout=100)
        self.assertEqual(result["state"],"active")
        self.assertEqual(result["wakeReason"],"timeout")
        self.assertFalse(result["pendingActions"][0]["isBlocking"])
        self.client.call("codex.control",{"action":"interrupt","threadId":work["threadId"],"turnId":work["turnId"]})
        self.assertEqual(self.client.call("codex.wait",{
            "threadId": work["threadId"], "turnId": work["turnId"], "timeoutMs": 1,
        })["pendingActions"], [])

    def test_typed_approval_permission_and_elicitation_paths(self):
        cases = [
            ("approval",{"type":"approval","decision":"approve"}),
            ("file",{"type":"approval","decision":"decline"}),
            ("permissions",{"type":"permissions","permissions":{"network":{"enabled":True}},"scope":"turn"}),
        ]
        for scenario, answer in cases:
            with self.subTest(scenario=scenario):
                work = self.start(scenario)
                result = self.wait(work)
                self.assertEqual(result["state"],"active")
                self.assertEqual(result["wakeReason"],"actionRequired")
                request_id = result["pendingActions"][0]["requestId"]
                self.client.call("codex.action.respond",{"requestId":request_id,**answer})
                completed = self.wait(work)
                self.assertEqual(completed["state"],"terminal")
                self.assertEqual(completed["wakeReason"],"terminal")

        for scenario, response in [
            ("form", {"action": "accept", "content": {"name": "Operator"}}),
            ("openai_form", {"action": "accept", "content": {"opaque": True}}),
            ("url", {"action": "cancel"}),
        ]:
            with self.subTest(scenario=scenario):
                work = self.start(scenario)
                result = self.wait(work)
                pending = result["pendingActions"][0]
                self.assertEqual(pending["kind"], "elicitation")
                self.client.call("codex.action.respond", {
                    "type": "elicitation", "requestId": pending["requestId"], **response,
                })
                self.assertEqual(self.wait(work)["state"], "terminal")

    def test_review_and_discovery(self):
        source = self.start("complete")
        source_result = self.wait(source)
        self.assertEqual(source_result["state"],"terminal")
        self.assertEqual(source_result["wakeReason"],"terminal")
        review = self.client.call("codex.start",{
            "mode":"review", "threadId": source["threadId"], "target":{"type":"uncommittedChanges"},
        })
        self.assertFalse(review["createdThread"])
        self.assertEqual(review["threadId"], source["threadId"])
        result = self.wait(review)
        self.assertEqual(result["state"],"terminal")
        self.assertEqual(result["wakeReason"],"terminal")
        self.assertEqual(result["threadId"], source["threadId"])
        self.assertEqual(result["turnId"], review["turnId"])
        info = self.client.call("codex.info", {"queries":[
            {"type":"models"}, {"type":"skills"}, {"type":"usage"},
        ]})
        self.assertEqual([entry["type"] for entry in info["results"]], ["models", "skills", "usage"])
        usage = info["results"][2]["result"]
        self.assertFalse(usage["ordinaryUsageAllowed"])
        self.assertEqual(usage["rateLimitResetCredits"]["availableCount"], 2)
        self.assertEqual(usage["accountId"], "fixture-account")
        self.client.call("codex.info", {"queries": []}, error=True, validate_input=False)
        self.client.call(
            "codex.info",
            {"queries": [{"type": "usage"}] * 11},
            error=True,
            validate_input=False,
        )

    def test_new_thread_policy_projection_and_review_model_routing(self):
        for policy, expected_mode in [
            ({"type": "readOnly", "networkAccess": False}, "read-only"),
            ({"type": "workspaceWrite", "networkAccess": True}, "workspace-write"),
            ({"type": "dangerFullAccess"}, "danger-full-access"),
        ]:
            with self.subTest(policy=policy["type"]):
                before = len(self.method_params("thread/start"))
                work = self.start("complete", sandboxPolicy=policy)
                self.assertEqual(self.wait(work)["state"], "terminal")
                thread_start = self.method_params("thread/start")[before]
                self.assertEqual(thread_start["sandbox"], expected_mode)
                turn_start = self.method_params("turn/start")[-1]
                self.assertEqual(turn_start["approvalPolicy"], "on-request")
                self.assertEqual(turn_start["sandboxPolicy"]["type"], policy["type"])
                if "networkAccess" in policy:
                    self.assertEqual(
                        turn_start["sandboxPolicy"]["networkAccess"],
                        policy["networkAccess"],
                    )

        before = len(self.method_params("thread/start"))
        instructed = self.start(
            "complete",
            developerInstructions="Prefer evidence over assumptions.",
        )
        self.assertEqual(self.wait(instructed)["state"], "terminal")
        instructed_start = self.method_params("thread/start")[before]
        self.assertTrue(instructed_start["developerInstructions"].startswith("Workspace policy:"))
        self.assertIn("Approval discipline:", instructed_start["developerInstructions"])
        self.assertIn(
            "Operator-supplied developer instructions:\nPrefer evidence over assumptions.",
            instructed_start["developerInstructions"],
        )

        source = self.start("complete")
        self.assertEqual(self.wait(source)["state"], "terminal")
        resume_count = len(self.method_params("thread/resume"))
        resumed = self.start(
            "complete",
            threadId=source["threadId"],
            sandboxPolicy={"type": "dangerFullAccess"},
        )
        self.assertEqual(self.wait(resumed)["state"], "terminal")
        self.assertEqual(len(self.method_params("thread/resume")), resume_count + 1)
        self.assertNotIn("sandbox", self.method_params("thread/resume")[-1])
        self.assertNotIn("developerInstructions", self.method_params("thread/resume")[-1])

        resumes_before_rejection = len(self.method_params("thread/resume"))
        self.client.call("codex.start", {
            "mode": "work",
            "task": "complete",
            "threadId": source["threadId"],
            "developerInstructions": "Do something different.",
            "sandboxPolicy": {"type": "dangerFullAccess"},
        }, error=True)
        self.assertEqual(len(self.method_params("thread/resume")), resumes_before_rejection)

        before_threads = len(self.method_params("thread/start"))
        before_reviews = len(self.method_params("review/start"))
        review = self.client.call("codex.start", {
            "mode": "review",
            "model": "gpt-6-astra",
            "target": {"type": "uncommittedChanges"},
        })
        self.assertTrue(review["createdThread"])
        self.assertEqual(self.wait(review)["state"], "terminal")
        review_thread_start = self.method_params("thread/start")[before_threads]
        self.assertEqual(review_thread_start["model"], "gpt-6-astra")
        self.assertEqual(review_thread_start["sandbox"], "read-only")
        review_start = self.method_params("review/start")[before_reviews]
        self.assertEqual(set(review_start), {"threadId", "target", "delivery"})

        before_threads = len(self.method_params("thread/start"))
        model_less = self.client.call("codex.start", {
            "mode": "review",
            "target": {"type": "uncommittedChanges"},
        })
        self.assertEqual(self.wait(model_less)["state"], "terminal")
        model_less_start = self.method_params("thread/start")[before_threads]
        self.assertNotIn("model", model_less_start)
        self.assertEqual(model_less_start["sandbox"], "read-only")

        resumes_before_rejection = len(self.method_params("thread/resume"))
        self.client.call("codex.start", {
            "mode": "review",
            "threadId": source["threadId"],
            "model": "gpt-6-astra",
            "target": {"type": "uncommittedChanges"},
        }, error=True)
        self.assertEqual(len(self.method_params("thread/resume")), resumes_before_rejection)

    def test_wait_tracks_review_turn_before_thread_history_catches_up(self):
        source = self.start("complete")
        self.assertEqual(self.wait(source)["state"], "terminal")
        review = self.client.call("codex.start", {
            "mode": "review", "threadId": source["threadId"],
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
        self.client.call("codex.inspect", {
            "threadId": source["threadId"], "turnId": source["turnId"], "detail": "semantic",
        })

        resumed = self.start("idle", threadId=source["threadId"])
        self.client.call("codex.control", {
            "action": "steer", "threadId": resumed["threadId"],
            "expectedTurnId": resumed["turnId"],
            "instruction": "continue",
        })
        self.client.call("codex.control", {
            "action": "interrupt", "threadId": resumed["threadId"], "turnId": resumed["turnId"],
        })
        self.assertEqual(self.wait(resumed)["state"], "terminal")

        review = self.client.call("codex.start", {
            "mode": "review", "threadId": source["threadId"], "target": {"type": "uncommittedChanges"},
        })
        self.assertEqual(self.wait(review)["state"], "terminal")

        self.client.call("command.exec", {"command": ["echo", "coverage"]})
        repl = self.client.call("command.start", {
            "command": ["fixture-repl"], "tty": True, "size": {"rows": 24, "cols": 80},
        })
        self.client.call("command.read", {"processId": repl["processId"], "timeoutMs": 1000})
        self.client.call("command.control", {"action": "resize", "processId": repl["processId"], "rows": 30, "cols": 100})
        self.client.call("command.control", {
            "action": "write", "processId": repl["processId"], "input": "2+2\n", "closeStdin": True,
        })
        self.client.call("command.read", {"processId": repl["processId"], "timeoutMs": 1000})
        quiet = self.client.call("command.start", {"command": ["fixture-quiet"]})
        self.client.call("command.control", {"action": "terminate", "processId": quiet["processId"]})
        self.client.call("command.read", {"processId": quiet["processId"], "timeoutMs": 1000})

        for scenario, answer in [
            ("approval", {"type": "approval", "decision": "approve"}),
            ("file", {"type": "approval", "decision": "decline"}),
            ("permissions", {"type": "permissions", "permissions": {"network": {"enabled": True}}, "scope": "turn"}),
            ("question", {"type": "userInput", "answers": {"format": ["JSON"]}}),
        ]:
            work = self.start(scenario)
            pending = self.wait(work)["pendingActions"][0]
            self.client.call("codex.action.respond", {"requestId": pending["requestId"], **answer})
            self.assertEqual(self.wait(work)["state"], "terminal")

        elicitation = self.start("form")
        pending = self.wait(elicitation)["pendingActions"][0]
        self.assertEqual(pending["kind"], "elicitation")
        self.client.call("codex.action.respond", {
            "type": "elicitation", "requestId": pending["requestId"],
            "action": "accept", "content": {"name": "Operator"},
        })
        self.assertEqual(self.wait(elicitation)["state"], "terminal")

        self.client.call("codex.info", {"queries":[
            {"type":"models"}, {"type":"skills"}, {"type":"usage"},
        ]})

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
