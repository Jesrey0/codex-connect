#!/usr/bin/env python3
"""Exercise the real MCP/relay/transport stack against a pinned-schema-valid peer.

Run after cargo build -p codex-connect. Requires Python jsonschema.
"""

import base64
import http.client
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

import jsonschema

from support.mcp_client import McpClient, PROTOCOL_VERSION

ROOT = pathlib.Path(__file__).resolve().parents[1]
CONTRACT = json.loads((ROOT / "config/app-server-tool-schemas.json").read_text())
EXPECTED = {
    "status", "host.inspect", "host.apply_patch", "host.view_image", "command.exec",
    "command.start", "command.read", "command.control",
    "codex.start", "codex.wait", "codex.inspect", "codex.query", "codex.act",
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
            "XDG_STATE_HOME": str(cls.workspace / "state"),
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
            cls.client = McpClient(cls.url, request_timeout=60)
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
        arguments.setdefault("model", "fixture-model-1")
        if "threadId" not in arguments and "forkFromThreadId" not in arguments:
            arguments.setdefault("cwd", str(self.workspace))
        return self.client.call("codex.start", {"mode": "work", "task": scenario, **arguments})

    def codex_start(self, arguments, **options):
        arguments = dict(arguments)
        arguments.setdefault("model", "fixture-model-1")
        if "threadId" not in arguments and "forkFromThreadId" not in arguments:
            arguments.setdefault("cwd", str(self.workspace))
        return self.client.call("codex.start", arguments, **options)

    def wait(self, work, **arguments):
        return self.client.call("codex.wait", {
            "threadId": work["threadId"], "turnId": work["turnId"],
            **arguments,
        })

    def inspect_turn(self, work, detail="semantic", after_cursor=None):
        return self.client.call("codex.inspect", {
            "threadId": work["threadId"],
            "turnId": work["turnId"],
            "afterCursor": work["cursor"] if after_cursor is None else after_cursor,
            "detail": detail,
        })

    def follow_next_call(self, next_call):
        self.assertEqual(set(next_call), {"tool", "arguments"})
        return self.client.call(next_call["tool"], next_call["arguments"])

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

    def wait_for_worker(self, predicate, timeout=2.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            status = self.client.call("status")
            for worker in status["workers"]:
                if predicate(worker):
                    return worker
            time.sleep(0.025)
        self.fail("worker was not registered in status")

    def test_catalog_status_runtime_and_origin_boundary(self):
        self.assertEqual(len(self.client.catalog), 13)
        self.assertEqual(set(self.client.tools), EXPECTED)
        discovery = self.client.request("server/discover")
        self.assertNotIn("resources", discovery["capabilities"])
        expected_security = [{"type": "oauth2", "scopes": ["codex-connect:access"]}]
        for tool in self.client.catalog:
            self.assertEqual(tool["securitySchemes"], expected_security)
            self.assertNotIn("securitySchemes", tool.get("_meta", {}))
            self.assertNotIn("ui", tool.get("_meta", {}))
            self.assertNotIn("openai/ui", tool.get("_meta", {}))
        status = self.client.call("status")
        self.assertTrue(status["ready"])
        self.assertEqual(
            set(status),
            {"ready", "defaultCwd", "buildId", "codexRelease", "commands", "workers"},
        )
        self.assertIsInstance(status["commands"], list)
        self.assertIsInstance(status["workers"], list)
        self.assertEqual(status["defaultCwd"], str(self.workspace))
        self.assertEqual(status["codexRelease"], CONTRACT["codexPin"])
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
        with rejected.exception:
            self.assertEqual(rejected.exception.code, 403)
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        self.assertEqual(observer["runtime"], runtime)
        self.assertIsInstance(observer["cursor"], int)
        projection = observer["projection"]
        self.assertEqual(set(projection), {
            "defaultCwd", "usage", "usageError", "usageUpdatedAtMs", "workers", "pendingActions", "notices",
        })
        self.assertEqual(projection["defaultCwd"], str(self.workspace))
        self.assertIsNone(projection["usageError"])
        if projection["usage"] is None:
            with urllib.request.urlopen(
                f"{self.url}/observe/wait/{observer['cursor']}", timeout=2
            ) as response:
                observer = json.load(response)
            self.assertGreater(observer["cursor"], 0)
            projection = observer["projection"]
        self.assertIn("rateLimits", projection["usage"])
        self.assertIsInstance(projection["usageUpdatedAtMs"], int)

    def test_trailing_slash_tools_list_preserves_root_security_schemes(self):
        tools = self.client.request("tools/list", path="/mcp/")["tools"]
        expected_security = [{"type": "oauth2", "scopes": ["codex-connect:access"]}]
        for tool in tools:
            self.assertEqual(tool["securitySchemes"], expected_security)
            self.assertNotIn("securitySchemes", tool.get("_meta", {}))

    def test_bare_host_tool_names_are_not_dispatch_aliases(self):
        for name in ["inspect", "apply_patch", "view_image"]:
            with self.subTest(name=name):
                result = self.client.request("tools/call", {"name": name, "arguments": {}})
                self.assertTrue(result["isError"])
                self.assertIn("unknown tool", result["content"][0]["text"].lower())

    def test_dns_rebinding_validation_precedes_oversized_body_limit(self):
        server = urllib.parse.urlsplit(self.url)
        oversized_content_length = 4 * 1024 * 1024 + 1
        cases = [
            ("disallowed origin", {"Origin": "https://evil.example"}, 403),
            ("disallowed host", {"Host": "evil.example"}, 403),
            ("malformed host", {"Host": "bad host"}, 400),
        ]
        for label, extra_headers, expected_status in cases:
            with self.subTest(label=label):
                connection = http.client.HTTPConnection(
                    server.hostname, server.port, timeout=5,
                )
                headers = {
                    "Host": server.netloc,
                    "Content-Type": "application/json",
                    "Accept": "application/json, text/event-stream",
                    "Content-Length": str(oversized_content_length),
                    **extra_headers,
                }
                try:
                    connection.putrequest("POST", "/mcp", skip_host=True)
                    for name, value in headers.items():
                        connection.putheader(name, value)
                    connection.endheaders()
                    response = connection.getresponse()
                    self.assertEqual(response.status, expected_status)
                finally:
                    connection.close()

    def test_cancelled_codex_start_is_recovered_from_nondestructive_status(self):
        caller = McpClient(self.url)
        caller.request_timeout = 0.1
        with self.assertRaises((TimeoutError, socket.timeout)):
            caller.call("codex.start", {
                "mode": "work", "task": "delayed_start_response",
                "cwd": str(self.project), "model": "fixture-model-1",
            })
        recovered = self.wait_for_worker(
            lambda worker: worker.get("prompt") == "delayed_start_response", timeout=2.0,
        )
        self.assertEqual(recovered["cwd"], str(self.project))
        self.assertEqual(recovered["mode"], "work")
        for _ in range(2):
            same = next(worker for worker in self.client.call("status")["workers"]
                        if worker["turnId"] == recovered["turnId"])
            self.assertEqual(same, recovered)
        result = self.client.call("codex.wait", {
            "threadId": recovered["threadId"], "turnId": recovered["turnId"],
        })
        self.assertEqual(result["state"], "terminal")
        terminal = next(worker for worker in self.client.call("status")["workers"]
                        if worker["turnId"] == recovered["turnId"])
        self.assertEqual(terminal["status"], "completed")
        self.assertEqual(terminal["cwd"], str(self.project))

    def test_observer_wait_is_event_driven_and_projects_live_worker_state(self):
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        cursor = observer["cursor"]
        work = self.start("progress")

        observed_worker = None
        for _ in range(6):
            with urllib.request.urlopen(
                f"{self.url}/observe/wait/{cursor}", timeout=2
            ) as response:
                update = json.load(response)
            self.assertGreater(update["cursor"], cursor)
            cursor = update["cursor"]
            observed_worker = next((
                worker for worker in update["projection"]["workers"]
                if worker["turnId"] == work["turnId"]
            ), None)
            if observed_worker is not None and observed_worker["activityKind"] == "message":
                break

        self.assertIsNotNone(observed_worker)
        self.assertEqual(observed_worker["threadId"], work["threadId"])
        self.assertEqual(observed_worker["activityKind"], "message")
        self.assertIn("Working", observed_worker["activitySummary"])
        self.assertIn("transcriptRevision", observed_worker)
        self.client.call("codex.act", {
            "action":"interrupt", "threadId":work["threadId"], "turnId":work["turnId"]
        })
        self.assertEqual(self.wait(work)["state"], "terminal")
    def test_large_content_search_returns_partial_matches(self):
        path = self.workspace / ("many-matches-" + "p" * 100 + ".txt")
        path.write_text(("needle" + "x" * 994 + "\n") * 1_000)
        try:
            row = self.client.call("host.inspect", {"operations": [
                {"type": "searchContent", "query": "needle", "path": path.name, "maxResults": 1_000},
            ]})["results"][0]
            matches = row["result"]["matches"]
            self.assertTrue(row["result"]["truncated"])
            self.assertGreater(len(matches), 0)
            self.assertLess(len(matches), 1_000)
            self.assertEqual(matches[0]["line"], 1)
        finally:
            path.unlink()

    def test_inspection_uses_default_cwd_and_accepts_absolute_host_paths(self):
        result = self.client.call("host.inspect", {"operations": [
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
        self.assertEqual(fuzzy, {"path": str(self.workspace / "sample.txt"), "kind": "file"})
        escaped = self.client.call("host.inspect", {"operations": [
            {"type": "fuzzyFileSearch", "query": "external", "path": "."},
        ]})
        self.assertEqual(escaped["results"][0]["result"]["files"], [])
        large = self.workspace / "large.txt"
        large.write_bytes(b"x" * (7 * 1024 * 1024))
        large_result = self.client.call("host.inspect", {"operations": [
            {"type": "readText", "path": "large.txt", "startLine": 1, "endLine": 1},
        ]})
        self.assertEqual(large_result["results"][0]["index"], 0)
        self.assertIn("safe fs/readFile transport limit", large_result["results"][0]["error"])
        large_directory = self.workspace / "large-directory"
        large_directory.mkdir()
        suffix = "x" * 240
        for index in range(28_000):
            (large_directory / f"{index:05d}-{suffix}").touch()
        large_directory_result = self.client.call("host.inspect", {"operations": [
            {"type": "readDirectory", "path": "large-directory"},
        ]})
        self.assertIn("directory listing exceeds", large_directory_result["results"][0]["error"])
        healthy = self.client.call("host.inspect", {"operations": [
            {"type": "readText", "path": "sample.txt", "startLine": 1, "endLine": 1},
        ]})
        self.assertEqual(healthy["results"][0]["result"]["text"], "one")
        partial = self.client.call("host.inspect", {"operations": [
            {"type":"readText","path":"/etc/passwd"},
            {"type":"readText","path":"sample.txt","startLine":3,"endLine":3},
        ]})
        self.assertEqual(partial["results"][0]["index"], 0)
        self.assertIn("root:", partial["results"][0]["result"]["text"])
        self.assertEqual(partial["results"][1]["index"], 1)
        self.assertEqual(partial["results"][1]["result"]["text"], "three")
        escaped_fuzzy = self.client.call("host.inspect", {"operations": [
            {"type":"fuzzyFileSearch","query":"passwd","path":"/etc"},
        ]})
        self.assertEqual(escaped_fuzzy["results"][0]["result"]["files"][0]["path"], "/etc/passwd")
        outside_cwd = self.client.call("host.inspect", {
            "cwd":"/etc", "operations":[{"type":"readDirectory","path":"."}],
        })
        self.assertIn("passwd", {entry["fileName"] for entry in outside_cwd["results"][0]["result"]["entries"]})

    def test_request_cwd_applies_consistently_to_paths_patch_and_image(self):
        cwd = str(self.project)
        inspected = self.client.call("host.inspect", {"cwd":cwd,"operations":[
            {"type":"readText","path":"local.txt"},
            {"type":"searchContent","query":"project-local"},
        ]})
        self.assertEqual(inspected["results"][0]["result"]["text"], "project-local")
        self.assertEqual(inspected["results"][1]["result"]["matches"][0]["path"], "local.txt")
        self.client.call("host.apply_patch", {
            "cwd":cwd,
            "patch":"*** Begin Patch\n*** Add File: patch.txt\n+created\n*** End Patch",
        })
        self.assertEqual((self.project / "patch.txt").read_text(), "created\n")
        before_image_reads = sum(
            (entry := json.loads(line))["kind"] == "method" and entry["name"] == "fs/readFile"
            for line in self.coverage_path.read_text().splitlines()
        )
        image = self.client.call("host.view_image", {"cwd":cwd,"path":"pixel.png"})
        self.assertEqual(image["path"], "project/pixel.png")
        self.assertEqual(image["mimeType"], "image/png")
        after_image_reads = sum(
            (entry := json.loads(line))["kind"] == "method" and entry["name"] == "fs/readFile"
            for line in self.coverage_path.read_text().splitlines()
        )
        self.assertEqual(after_image_reads, before_image_reads + 1)
        self.client.call("host.apply_patch", {
            "cwd":cwd,
            "patch":"*** Begin Patch\n*** Add File: ../escape.txt\n+outside-cwd\n*** End Patch",
        })
        self.assertEqual((self.workspace / "escape.txt").read_text(), "outside-cwd\n")

    def test_command_boundaries(self):
        schema = self.client.tools["command.exec"]["inputSchema"]["properties"]
        self.assertNotIn("disableTimeout", schema)
        self.assertNotIn("disableOutputCap", schema)
        self.assertNotIn("timeoutMs", schema)
        self.assertNotIn("outputBytesCap", schema)
        inherited = self.client.call("command.exec", {"command":["fixture-policy"]})
        self.assertIsNone(json.loads(inherited["stdout"]))
        result = self.client.call("command.exec", {"command":["echo","fixture"]})
        self.assertEqual(result["exitCode"],0)
        self.assertNotIn("stdoutBytes", result)
        self.assertNotIn("stderrBytes", result)
        self.assertFalse(result["stdoutMayBeTruncated"])
        self.assertFalse(result["stderrMayBeTruncated"])
        self.assertGreaterEqual(result["durationMs"], 0)
        command_params = self.method_params("command/exec")[-1]
        self.assertEqual(command_params["timeoutMs"], 33000)
        self.assertEqual(command_params["outputBytesCap"], 65536)
        self.assertNotIn("sandboxPolicy", command_params)
        self.assertEqual(self.client.call("command.exec", {
            "command":["echo","fixture"], "cwd":"/etc",
        })["exitCode"], 0)
        for arguments in [
            {"command":[]}, {"command":["echo"],"tty":True},
            {"command":["echo"],"disableTimeout":True},
            {"command":["echo"],"disableOutputCap":True},
            {"command":["echo"],"timeoutMs":70001},
            {"command":["echo"],"outputBytesCap":1},
            {"command":["echo"],"sandboxPolicy":{"type":"externalSandbox"}},
            {"command":["echo"],"sandboxPolicy":{"type":"workspaceWrite","writableRoots":["project"]}},
        ]:
            self.client.call("command.exec",arguments,error=True,validate_input=False)

    def test_command_output_cap_accounts_for_decoded_byte_expansion(self):
        for kind, expected in [("expanded", True), ("exact", True), ("below", False)]:
            with self.subTest(kind=kind):
                result = self.client.call("command.exec", {
                    "command": ["fixture-output-cap", kind],
                })
                self.assertEqual(result["stdoutMayBeTruncated"], expected)
                self.assertEqual(result["stderrMayBeTruncated"], expected)
                if kind == "expanded":
                    self.assertGreater(len(result["stdout"].encode()), 65536)

    def test_persistent_nonpty_streaming_and_host_context(self):
        started = self.client.call("command.start", {
            "command": ["fixture-stream"],
        })
        self.assertIn("processId", started)
        self.assertEqual(set(started), {"processId", "cwd", "output", "readError"})
        self.assertEqual(started["cwd"], str(self.workspace))
        recovered = next(
            command for command in self.client.call("status")["commands"]
            if command["processId"] == started["processId"]
        )
        self.assertEqual(recovered["state"], "running")
        self.assertFalse(recovered["tty"])
        cursor = 0
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
            "processId": started["processId"], "timeoutMs": 50001,
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
        self.assertIsNone(started["readError"])
        prompt = started["output"]
        self.assertEqual(prompt["stdout"], ">>> ")
        self.assertEqual(prompt["stderr"], "")
        quiet = self.follow_next_call(prompt["nextCall"])
        self.assertEqual(quiet["state"], "running")
        self.assertEqual(quiet["wakeReason"], "timeout")
        self.assertEqual(quiet["stdout"], "")
        self.assertFalse(quiet["drained"])
        self.assertEqual(quiet["nextCall"]["arguments"]["afterCursor"], quiet["cursor"])
        self.client.call("command.control", {
            "action": "resize", "processId": started["processId"], "rows": 40, "cols": 120,
        })
        written = self.client.call("command.control", {
            "action": "write", "processId": started["processId"], "input": "2+2\n",
            "afterCursor": prompt["cursor"],
        })
        self.assertTrue(written["written"])
        self.assertIsNone(written["readError"])
        answer = written["output"]
        self.assertEqual(answer["stdout"], "4\n>>> ")
        self.assertEqual(answer["nextCall"]["tool"], "command.read")
        self.assertEqual(answer["nextCall"]["arguments"]["afterCursor"], answer["cursor"])
        closed = self.client.call("command.control", {
            "action": "write", "processId": started["processId"], "closeStdin": True,
            "afterCursor": answer["cursor"],
        })
        self.assertTrue(closed["stdinClosed"])
        exited = closed["output"]
        self.assertEqual(exited["state"], "exited")
        self.assertEqual(exited["exitCode"], 0)
        self.assertIsNone(exited["nextCall"])
        for args in [
            {"action": "write", "processId": started["processId"], "input": "x"},
            {"action": "resize", "processId": started["processId"], "rows": 1, "cols": 1},
            {"action": "terminate", "processId": started["processId"]},
        ]:
            self.client.call("command.control", args, error=True)

    def test_cancelled_command_start_yield_retains_recoverable_handle(self):
        known = {row["processId"] for row in self.client.call("status")["commands"]}
        caller = McpClient(self.url)
        caller.request_timeout = 0.1
        with self.assertRaises((TimeoutError, socket.timeout)):
            caller.call("command.start", {
                "command": ["fixture-quiet"], "cwd": str(self.project), "yieldTimeMs": 10000,
            })
        retained = [row for row in self.client.call("status")["commands"] if row["processId"] not in known]
        self.assertEqual(len(retained), 1)
        command = retained[0]
        self.assertEqual(command["state"], "running")
        self.assertEqual(command["cwd"], str(self.project))
        self.assertIn(command, self.client.call("status")["commands"])
        self.client.call("command.control", {"action": "terminate", "processId": command["processId"]})
        result = self.client.call("command.read", {
            "processId": command["processId"], "afterCursor": 0, "timeoutMs": 1000,
        })
        self.assertTrue(result["drained"])
        self.assertEqual(result["exitCode"], 143)

    def test_command_yield_is_observation_and_validates_before_mutation(self):
        starts_before = len(self.method_params("command/exec"))
        self.client.call("command.start", {
            "command": ["fixture-quiet"], "yieldTimeMs": 10001,
        }, error=True, validate_input=False)
        self.assertEqual(len(self.method_params("command/exec")), starts_before)

        quiet = self.client.call("command.start", {
            "command": ["fixture-quiet"], "yieldTimeMs": 0,
        })
        self.assertIsNone(quiet["readError"])
        self.assertEqual(quiet["output"]["state"], "running")
        self.assertEqual(quiet["output"]["wakeReason"], "timeout")
        self.assertFalse(quiet["output"]["drained"])
        self.assertEqual(quiet["output"]["cursor"], 0)
        writes_before = len(self.method_params("command/exec/write"))
        for invalid in [{"yieldTimeMs": 10001}, {"afterCursor": 999999}]:
            self.client.call("command.control", {
                "action": "write", "processId": quiet["processId"], "input": "no replay\n",
                **invalid,
            }, error=True, validate_input=False)
        self.assertEqual(len(self.method_params("command/exec/write")), writes_before)
        self.assertEqual(next(command for command in self.client.call("status")["commands"]
                              if command["processId"] == quiet["processId"])["state"], "running")
        written = self.client.call("command.control", {
            "action": "write", "processId": quiet["processId"], "input": "once\n",
            "afterCursor": 0, "yieldTimeMs": 0,
        })
        self.assertTrue(written["written"])
        self.assertIsNone(written["readError"])
        self.assertEqual(written["output"]["state"], "running")
        self.assertEqual(written["output"]["wakeReason"], "timeout")
        self.assertEqual(len(self.method_params("command/exec/write")), writes_before + 1)
        self.client.call("command.control", {"action": "terminate", "processId": quiet["processId"]})

    def test_codex_start_schema_matches_fresh_settings_and_upstream_restoration(self):
        schema = jsonschema.Draft202012Validator(self.client.tools["codex.start"]["inputSchema"])
        work = {"mode": "work", "task": "complete", "model": "fixture-model-1"}
        review = {"mode": "review", "target": {"type": "uncommittedChanges"}, "model": "fixture-model-1"}
        for valid in [
            {**work, "cwd": str(self.project)},
            {**work, "cwd": str(self.project), "access": "workspace", "writableRoots": []},
            {**work, "cwd": str(self.project), "access": "full", "effort": "high"},
            {**work, "threadId": "canonical"},
            {**work, "forkFromThreadId": "canonical", "lastTurnId": "source-turn"},
            {**review, "cwd": str(self.project)},
            {**review, "threadId": "canonical"},
        ]:
            with self.subTest(valid=valid):
                schema.validate(valid)
        for invalid in [work, review, {**work, "cwd": str(self.project), "access": "full", "writableRoots": []},
                        {**work, "cwd": str(self.project), "lastTurnId": "orphan"},
                        {**work, "threadId": "canonical", "forkFromThreadId": "canonical"}]:
            with self.subTest(invalid=invalid):
                self.assertFalse(schema.is_valid(invalid))
        for restored in ["threadId", "forkFromThreadId"]:
            for override in [{"cwd": str(self.project)}, {"effort": "high"}, {"access": "full"}, {"writableRoots": []}]:
                with self.subTest(restored=restored, override=override):
                    self.assertFalse(schema.is_valid({**work, restored: "canonical", **override}))
        self.assertFalse(schema.is_valid({**review, "threadId": "canonical", "cwd": str(self.project)}))

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

    def test_modern_command_read_uses_bounded_tool_result(self):
        self.assertEqual(
            self.client.tools["command.read"]["inputSchema"]["properties"]["timeoutMs"]["maximum"],
            43_000,
        )
        quiet = self.client.call("command.start", {"command": ["fixture-quiet"]})
        request_id = next(self.client.ids)
        request_value = {
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/call",
            "params": {
                "name": "command.read",
                "arguments": {"processId": quiet["processId"], "timeoutMs": 1000},
            },
        }
        with self.client.open_request(request_value) as response:
            self.assertEqual(response.headers.get("Content-Type"), "text/event-stream")
            message = McpClient._event_stream_response(
                response.read().decode(), request_id,
            )
        self.assertEqual(message["result"]["resultType"], "complete")
        self.assertEqual(
            message["result"]["structuredContent"]["wakeReason"], "timeout",
        )
        self.client.call("command.control", {
            "action": "terminate", "processId": quiet["processId"],
        })

    def test_only_modern_mcp_is_advertised_and_accepted(self):
        self.assertEqual(
            self.client.request("server/discover")["supportedVersions"],
            [PROTOCOL_VERSION],
        )

        def send(request_value, version):
            request = urllib.request.Request(
                self.url + "/mcp", json.dumps(request_value).encode(),
                headers={
                    "Content-Type": "application/json",
                    "Accept": "application/json, text/event-stream",
                    "MCP-Protocol-Version": version,
                    "Mcp-Method": request_value["method"],
                },
            )
            try:
                response = urllib.request.urlopen(request, timeout=5)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                raw = response.read().decode()
                if response.headers.get("Content-Type", "").startswith("text/event-stream"):
                    return McpClient._event_stream_response(raw, request_value["id"])
                return json.loads(raw)

        for version in ("2025-06-18", "2025-11-25"):
            with self.subTest(version=version):
                request_id = next(self.client.ids)
                request_value = {
                    "jsonrpc": "2.0", "id": request_id, "method": "server/discover",
                    "params": {
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": version,
                            "io.modelcontextprotocol/clientInfo": {
                                "name": "legacy-probe", "version": "1",
                            },
                            "io.modelcontextprotocol/clientCapabilities": {},
                        },
                    },
                }
                initialize = {
                    "jsonrpc": "2.0", "id": next(self.client.ids),
                    "method": "initialize",
                    "params": {
                        "protocolVersion": version,
                        "capabilities": {},
                        "clientInfo": {"name": "legacy-probe", "version": "1"},
                    },
                }
                for candidate in (request_value, initialize):
                    message = send(candidate, version)
                    self.assertIn("error", message, message)
                    self.assertEqual(message["error"]["code"], -32022)
                    self.assertEqual(message["error"]["data"]["supported"], [PROTOCOL_VERSION])

        for headers in ({}, {"MCP-Protocol-Version": "2025-06-18"}):
            with self.subTest(headers=headers):
                request = urllib.request.Request(
                    self.url + "/mcp",
                    json.dumps({
                        "jsonrpc": "2.0", "id": next(self.client.ids),
                        "method": "tools/list",
                    }).encode(),
                    headers={
                        "Content-Type": "application/json",
                        "Accept": "application/json, text/event-stream",
                        "Mcp-Method": "tools/list",
                        **headers,
                    },
                )
                with self.assertRaises(urllib.error.HTTPError) as rejected:
                    urllib.request.urlopen(request, timeout=5)
                with rejected.exception:
                    self.assertEqual(rejected.exception.code, 400)

    def test_persistent_output_retention_is_bounded(self):
        started = self.client.call("command.start", {"command": ["fixture-bounded"]})
        time.sleep(0.2)
        result = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": 0, "timeoutMs": 1000,
        })
        self.assertTrue(result["historyLost"])
        self.assertLessEqual(len(result["stdout"].encode()), 128 * 1024)
        self.assertEqual(result["nextCall"]["tool"], "command.read")
        self.assertEqual(result["nextCall"]["arguments"]["processId"], started["processId"])
        self.assertEqual(result["nextCall"]["arguments"]["afterCursor"], result["cursor"])
        stale = self.client.call("command.read", {
            "processId": started["processId"], "afterCursor": 1, "timeoutMs": 0,
        })
        self.assertTrue(stale["historyLost"])
        self.assertIsNotNone(stale["nextCall"])
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

        self.assertEqual(first["nextCall"], {"tool": "command.read", "arguments": {
            "processId": started["processId"], "afterCursor": first["cursor"], "timeoutMs": 0,
        }})
        second = self.follow_next_call(first["nextCall"])
        self.assertEqual(second["state"], "exited")
        self.assertFalse(second["hasMoreOutput"])
        self.assertTrue(second["drained"])
        self.assertIsNone(second["nextCall"])
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
        turns_before = len(self.method_params("thread/turns/list"))
        work = self.start("no_event")
        result = self.wait(work)
        self.assertEqual(result["state"],"terminal")
        self.assertEqual(result["wakeReason"],"terminal")
        self.assertEqual(result["turn"]["output"][0]["text"],"fixture complete")
        self.assertEqual(len(self.method_params("thread/turns/list")) - turns_before, 1)
        self.client.call("codex.inspect",{"threadId":work["threadId"],"turnId":"missing"},error=True)
        next_work = self.start("idle", threadId=work["threadId"])
        self.assertNotIn("createdThread", next_work)
        idle = self.inspect_turn(next_work)
        self.assertEqual(idle["status"],"inProgress")
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        observed = next(
            turn for turn in observer["projection"]["workers"]
            if turn["turnId"] == next_work["turnId"]
        )
        self.assertEqual(observed["mode"], "work")
        self.assertEqual(observed["model"], work["model"])
        self.assertEqual(observed["effort"], work["effort"])
        self.assertNotIn("serviceTier", observed)
        self.assertEqual(observed["prompt"], "idle")
        self.assertGreater(observed["lastActivityAtMs"], 0)
        self.assertIn(observed["activityKind"], {"turn", "think", "message", "tool", "file", "search", "item", "waiting"})
        self.assertIn("activitySummary", observed)
        self.assertEqual(set(observed["tokenUsage"]), {
            "threadTotalTokens",
            "lastRequestModelContextWindow",
            "lastRequestInputTokens",
            "lastRequestCachedInputTokens",
            "cacheHitPercent",
            "lastModelUsageAtMs",
            "cacheGuaranteedUntilMs",
            "cacheGuaranteeActive",
        })
        snapshot = self.inspect_turn(next_work)
        self.assertEqual(snapshot["turnId"], next_work["turnId"])
        self.assertEqual(snapshot["detail"], "semantic")

    def test_observer_transcript_is_live_and_survives_terminal_cleanup(self):
        work = self.start("progress")
        active = self.transcript(work)
        self.assertEqual(active["threadId"], work["threadId"])
        self.assertEqual(active["turnId"], work["turnId"])
        self.assertEqual(active["status"], "inProgress")
        self.assertEqual(active["context"]["prompt"], "progress")
        self.assertNotIn("developerInstructions", active["context"])
        self.assertEqual(active["pendingActions"], [])
        self.assertEqual(active["activity"]["kind"], "message")
        self.assertIn("Working", active["activity"]["summary"])

        self.client.call("codex.act", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")
        terminal = self.transcript(work)
        self.assertEqual(terminal["status"], "interrupted")
        self.assertTrue(any(
            entry["kind"] == "agent" and entry.get("text") == "fixture complete"
            for entry in terminal["entries"]
        ))

    def test_observer_transcript_projects_operator_action_context(self):
        work = self.start("question")
        pending = self.transcript(work)
        self.assertEqual(pending["context"]["prompt"], "question")
        self.assertEqual(len(pending["pendingActions"]), 1)
        action = pending["pendingActions"][0]
        self.assertEqual(action["kind"], "userInput")
        self.assertTrue(action["isBlocking"])
        self.assertEqual(action["params"]["questions"][0]["question"], "Which output format?")
        self.client.call("codex.act", {
            "action": "respondUserInput",
            "requestId": action["requestId"],
            "answers": {"format": ["JSON"]},
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_observer_transcript_stops_hydrating_after_the_entry_cap(self):
        work = self.start("long_transcript")
        self.assertEqual(self.wait(work)["state"], "terminal")
        before = len(self.method_params("thread/items/list"))
        transcript = self.transcript(work)
        calls = self.method_params("thread/items/list")[before:]
        self.assertTrue(transcript["truncated"])
        self.assertEqual(len(transcript["entries"]), 512)
        self.assertEqual(transcript["entries"][0]["text"], "entry 188")
        self.assertEqual(transcript["entries"][-1]["text"], "entry 699")
        self.assertTrue(all(call["sortDirection"] == "desc" for call in calls))

    def test_observer_transcript_marks_exact_character_bound_as_truncated(self):
        work = self.start("exact_transcript_bound")
        self.assertEqual(self.wait(work)["state"], "terminal")
        transcript = self.transcript(work)
        self.assertTrue(transcript["truncated"])
        self.assertEqual(len(transcript["entries"]), 6)
        self.assertTrue(all(len(entry["text"]) == 32 * 1024 for entry in transcript["entries"]))

    def test_wait_rejects_caller_timeout_before_upstream_reads(self):
        work = self.start("idle")
        reads_before = len(self.method_params("thread/read"))
        turns_before = len(self.method_params("thread/turns/list"))
        for timeout in (0, 1, 40000, 40001):
            with self.subTest(timeout=timeout):
                result = self.client.call("codex.wait", {
                    "threadId": work["threadId"],
                    "turnId": work["turnId"],
                    "timeoutMs": timeout,
                }, error=True, validate_input=False)
                self.assertIn("unknown field `timeoutMs`", result["content"][0]["text"])
        self.assertEqual(len(self.method_params("thread/read")), reads_before)
        self.assertEqual(len(self.method_params("thread/turns/list")), turns_before)
        self.client.call("codex.act", {
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
        observed = next(
            turn for turn in observer["projection"]["workers"]
            if turn["turnId"] == work["turnId"]
        )
        self.assertEqual(observed["status"], "completed")
        result = self.wait(work)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["turn"]["status"], "completed")
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        retained = next(
            turn for turn in observer["projection"]["workers"]
            if turn["turnId"] == work["turnId"]
        )
        self.assertEqual(retained["status"], "completed")
        self.assertGreater(retained["terminalAtMs"], 0)
        unsubscribes = self.wait_for_method_count(
            "thread/unsubscribe", unsubscribes_before + 1,
        )
        self.assertEqual(
            unsubscribes[unsubscribes_before],
            {"threadId": work["threadId"]},
        )
        self.assertEqual(len(self.method_params("thread/unsubscribe")), unsubscribes_before + 1)

    def test_status_workers_and_pending_actions_are_nondestructive(self):
        work = self.start("complete", cwd=str(self.project))
        terminal = self.wait_for_worker(lambda worker: worker["turnId"] == work["turnId"]
                                        and worker["status"] == "completed")
        self.assertEqual(terminal["cwd"], str(self.project))
        self.assertEqual(self.wait(work)["state"], "terminal")
        self.assertEqual(next(worker for worker in self.client.call("status")["workers"]
                              if worker["turnId"] == work["turnId"])["cwd"], str(self.project))

        approval = self.start("approval")
        pending = self.wait(approval)["pendingActions"][0]
        self.assertEqual(pending["type"], "approval")
        approval_worker = next(
            worker
            for worker in self.client.call("status")["workers"]
            if worker["turnId"] == approval["turnId"]
        )
        self.assertEqual(approval_worker["cwd"], str(self.workspace))
        self.client.call("codex.act", {
            "action": "respondApproval", "requestId": pending["requestId"],
            "decision": "approve",
        })
        self.assertEqual(self.wait(approval)["state"], "terminal")

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
                event for event in observer["projection"]["notices"]
                if event["kind"] == "system"
                and event["threadId"] == work["threadId"]
                and "thread unsubscribed" in (event["summary"] or "")
            ]
            if successes:
                self.assertIn("unsubscribed", successes[-1]["summary"])
                break
            time.sleep(0.025)
        else:
            self.fail("unsubscribe success was not exposed through observer notices")

    def test_start_failure_after_thread_load_releases_subscription(self):
        unsubscribes_before = len(self.method_params("thread/unsubscribe"))
        self.codex_start({
            "mode": "work",
            "task": "start_error",
            "access": "full",
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
                event for event in observer["projection"]["notices"]
                if event["kind"] == "error"
                and event["threadId"] == work["threadId"]
                and "thread unsubscribe failed" in (event["summary"] or "")
            ]
            if failures:
                break
            time.sleep(0.025)
        else:
            self.fail("unsubscribe failure was not exposed through observer notices")

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

        self.client.call("codex.act", {
            "action": "interrupt", "threadId": first["threadId"], "turnId": first["turnId"],
        })
        self.assertEqual(self.wait(first)["state"], "terminal")
        time.sleep(0.05)
        self.assertEqual(len(self.method_params("thread/unsubscribe")), unsubscribes_before)

        self.client.call("codex.act", {
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
        review = self.codex_start({
            "mode": "review",
            "target": {"type": "uncommittedChanges"},
            "model": "gpt-6-astra",
        })
        self.assertEqual(self.wait(review)["state"], "terminal")
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer = json.load(response)
        self.assertNotIn(
            f"{review['threadId']}-review-auxiliary",
            [turn["turnId"] for turn in observer["projection"]["workers"]],
        )
        archived = self.client.call("codex.act", {
            "action": "setArchived",
            "threadIds": [review["threadId"]],
            "archived": True,
        })
        self.assertTrue(archived["results"][0].get("archived"))
        restored = self.client.call("codex.act", {
            "action": "setArchived",
            "threadIds": [review["threadId"]],
            "archived": False,
        })
        self.assertTrue(restored["results"][0].get("archived") is False)
        deleted = self.client.call("codex.act", {
            "action": "delete",
            "threadIds": [review["threadId"]],
        })
        self.assertTrue(deleted["results"][0].get("deleted"))
        with urllib.request.urlopen(self.url + "/observe") as response:
            observer_after_delete = json.load(response)
        self.assertNotIn(
            review["threadId"],
            [worker["threadId"] for worker in observer_after_delete["projection"]["workers"]],
        )

    def test_wait_uses_paginated_turn_lookup(self):
        work = self.start("inflate_history")
        result = self.wait(work)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["turnId"], work["turnId"])

    def test_wait_ignores_progress_and_inspect_owns_worker_history(self):
        # Idle lease expiry is covered by
        # test_quiet_wait_reconciles_once_at_lease_expiry_without_rehydrating_items.
        for scenario in ("progress","oversized"):
            work = self.start(scenario)
            result = self.wait(work)
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
            self.client.call("codex.act",{"action":"steer","threadId":work["threadId"],"expectedTurnId":work["turnId"],"instruction":"continue"})
            self.client.call("codex.act",{"action":"interrupt","threadId":work["threadId"],"turnId":work["turnId"]})
            interrupted = self.wait(work)
            self.assertEqual(interrupted["state"],"terminal")
            self.assertEqual(interrupted["wakeReason"],"terminal")
            self.assertEqual(interrupted["turn"]["status"],"interrupted")

    def test_quiet_wait_reconciles_once_at_lease_expiry_without_rehydrating_items(self):
        work = self.start("idle")
        turns_before = len(self.method_params("thread/turns/list"))
        items_before = len(self.method_params("thread/items/list"))
        result = self.wait(work)
        self.assertEqual(result["state"], "active")
        self.assertEqual(result["wakeReason"], "timeout")
        self.assertEqual(len(self.method_params("thread/turns/list")) - turns_before, 1)
        self.assertEqual(len(self.method_params("thread/items/list")) - items_before, 0)
        self.client.call("codex.act", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_terminal_notification_hydrates_items_without_status_polling(self):
        work = self.start("delayed_complete")
        turns_before = len(self.method_params("thread/turns/list"))
        items_before = len(self.method_params("thread/items/list"))
        started = time.monotonic()
        result = self.wait(work)
        self.assertLess(time.monotonic() - started, 5)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["wakeReason"], "terminal")
        self.assertEqual(result["turn"]["output"][0]["text"], "fixture complete")
        self.assertEqual(len(self.method_params("thread/turns/list")) - turns_before, 0)
        self.assertEqual(len(self.method_params("thread/items/list")) - items_before, 1)

    def test_slow_reconciliation_does_not_overrun_wait_lease(self):
        work = self.start("slow_reconcile")
        started = time.monotonic()
        result = self.wait(work)
        elapsed = time.monotonic() - started
        self.assertEqual(result["state"], "active")
        self.assertEqual(result["wakeReason"], "timeout")
        self.assertGreaterEqual(elapsed, 32.5)
        self.assertLess(elapsed, 34.5)
        self.client.call("codex.act", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_fresh_live_wait_reconciles_without_immediate_thread_read(self):
        reads_before = len(self.method_params("thread/read"))
        work = self.start("slow_initial_reconcile")
        started = time.monotonic()
        result = self.wait(work)
        elapsed = time.monotonic() - started
        self.assertEqual(result["state"], "active")
        self.assertEqual(result["wakeReason"], "timeout")
        self.assertEqual(result["turn"]["id"], work["turnId"])
        self.assertEqual(len(self.method_params("thread/read")), reads_before)
        self.assertGreaterEqual(elapsed, 32.5)
        self.assertLess(elapsed, 34.5)
        self.client.call("codex.act", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_fresh_live_wait_tolerates_unflushed_rollout_metadata(self):
        turns_before = len(self.method_params("thread/turns/list"))
        work = self.start("empty_rollout_initial")
        result = self.wait(work)
        self.assertEqual(result["state"], "active")
        self.assertEqual(result["wakeReason"], "timeout")
        self.assertEqual(result["turn"]["id"], work["turnId"])
        self.assertEqual(len(self.method_params("thread/turns/list")) - turns_before, 1)
        self.client.call("codex.act", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_terminal_hydration_retries_unflushed_rollout_metadata(self):
        items_before = len(self.method_params("thread/items/list"))
        work = self.start("empty_rollout_terminal")
        result = self.wait(work)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["turn"]["status"], "completed")
        self.assertEqual(result["turn"]["output"][0]["text"], "fixture complete")
        self.assertGreaterEqual(len(self.method_params("thread/items/list")) - items_before, 2)

    def test_wait_returns_one_canonical_handoff_and_raw_inspect_keeps_full_items(self):
        cases = [
            ("handoff_priority", "final", "agentMessage", "shared terminal content", 5),
            ("handoff_review_duplicate", "review-2", "exitedReviewMode", "shared review content", 4),
        ]
        for scenario, item_id, item_type, text, item_count in cases:
            with self.subTest(scenario=scenario):
                work = self.start(scenario)
                result = self.wait(work)
                self.assertEqual(result["state"], "terminal")
                output = result["turn"]["output"]
                self.assertEqual(len(output), 1)
                self.assertEqual(output[0]["id"], item_id)
                self.assertEqual(output[0]["type"], item_type)
                self.assertEqual(output[0]["text"], text)
                self.assertFalse(output[0]["truncated"])

                inspected = self.inspect_turn(work, detail="raw")
                completed = next(
                    event for event in inspected["events"]
                    if event.get("method") == "turn/completed"
                )
                original_items = completed["params"]["turn"]["items"]
                self.assertEqual(len(original_items), item_count)
                self.assertEqual(
                    sum(item["type"] == "exitedReviewMode" for item in original_items),
                    2,
                )

    def test_result_mode_recovers_clipped_text_after_journal_loss(self):
        work = self.start("handoff_long_history_gap")
        joined = self.wait(work)
        handoff = joined["turn"]["output"][0]
        self.assertEqual(joined["turn"]["selectionIncomplete"], False)
        self.assertEqual(handoff["id"], "long-final")
        self.assertTrue(handoff["truncated"])
        self.assertEqual(len(handoff["text"]), 10_240)

        raw = self.inspect_turn(work, detail="raw")
        self.assertTrue(raw["historyLost"])

        next_call = {"tool": "codex.inspect", "arguments": {
            "threadId": work["threadId"], "turnId": work["turnId"],
            "detail": "result", "textOffset": 0,
        }}
        chunks = []
        while next_call is not None:
            offset = next_call["arguments"]["textOffset"]
            page = self.follow_next_call(next_call)
            self.assertEqual(page["detail"], "result")
            self.assertTrue(page["resultPage"]["selectionComplete"])
            self.assertEqual(page["resultPage"]["item"]["id"], "long-final")
            chunks.append(page["resultPage"]["text"])
            self.assertLessEqual(len(page["resultPage"]["text"]), 10_240)
            next_offset = page["resultPage"]["nextTextOffset"]
            next_call = page["nextCall"]
            if next_offset is None:
                self.assertIsNone(next_call)
            else:
                self.assertEqual(next_offset, offset + len(page["resultPage"]["text"]))
                self.assertEqual(next_call, {"tool": "codex.inspect", "arguments": {
                    "threadId": work["threadId"], "turnId": work["turnId"],
                    "detail": "result", "textOffset": next_offset,
                }})
        self.assertEqual("".join(chunks), "abcdefghij" * 3_000)

        for arguments in [
            {"detail": "result", "afterCursor": 0},
            {"detail": "raw", "textOffset": 1},
        ]:
            with self.subTest(arguments=arguments):
                error = self.client.call("codex.inspect", {
                    "threadId": work["threadId"],
                    "turnId": work["turnId"],
                    **arguments,
                }, error=True, validate_input=False)
                self.assertIn("only valid", error["content"][0]["text"])

    def test_result_search_shrinks_aggregate_oversized_item_pages(self):
        before = len(self.method_params("thread/items/list"))
        work = self.start("handoff_aggregate_oversized_page")
        result = self.client.call("codex.inspect", {
            "threadId": work["threadId"],
            "turnId": work["turnId"],
            "detail": "result",
        })
        self.assertTrue(result["resultPage"]["selectionComplete"])
        self.assertEqual(result["resultPage"]["item"]["id"], "large-99")
        calls = self.method_params("thread/items/list")[before:]
        self.assertGreaterEqual(len(calls), 2)
        self.assertEqual(calls[0]["limit"], 100)
        self.assertEqual(calls[1]["limit"], 50)
        self.assertIsNone(calls[0].get("cursor"))
        self.assertIsNone(calls[1].get("cursor"))

    def test_inspect_next_call_preserves_event_modes_cursors_and_history_loss(self):
        work = self.start("journal_pages")
        self.assertEqual(self.wait(work)["state"], "terminal")
        for detail in ["semantic", "raw"]:
            with self.subTest(detail=detail):
                first = self.inspect_turn(work, detail=detail)
                self.assertTrue(first["historyLost"])
                self.assertTrue(first["hasMore"])
                page = first
                cursors = []
                pages = 0
                while True:
                    pages += 1
                    self.assertLess(pages, 100)
                    self.assertEqual(page["detail"], detail)
                    cursors.extend(event["cursor"] for event in page["events"])
                    if not page["hasMore"]:
                        self.assertIsNone(page["nextCall"])
                        break
                    self.assertEqual(page["nextCall"], {"tool": "codex.inspect", "arguments": {
                        "threadId": work["threadId"], "turnId": work["turnId"],
                        "detail": detail, "afterCursor": page["cursor"],
                    }})
                    continued = self.follow_next_call(page["nextCall"])
                    self.assertGreater(continued["cursor"], page["cursor"])
                    self.assertFalse(continued["historyLost"])
                    page = continued
                self.assertGreater(pages, 1)
                self.assertEqual(cursors, sorted(set(cursors)))
                replay = self.inspect_turn(work, detail=detail)
                self.assertEqual(replay["events"], first["events"])
                self.assertEqual(replay["cursor"], first["cursor"])

    def test_result_continuation_does_not_establish_terminal_output_or_authority(self):
        work = self.start("active_long_result")
        candidate = self.client.call("codex.inspect", {
            "threadId": work["threadId"], "turnId": work["turnId"], "detail": "result",
        })
        self.assertEqual(candidate["status"], "inProgress")
        self.assertTrue(candidate["resultPage"]["hasMoreText"])
        self.assertFalse(candidate["resultPage"]["selectionComplete"])
        self.assertIsNone(candidate["nextCall"])
        self.client.call("codex.act", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["turn"]["status"], "interrupted")

        empty = self.start("complete_without_handoff")
        joined = self.wait(empty)
        self.assertEqual(joined["state"], "terminal")
        self.assertEqual(joined["turn"]["output"], [])
        result = self.client.call("codex.inspect", {
            "threadId": empty["threadId"], "turnId": empty["turnId"], "detail": "result",
        })
        self.assertTrue(result["resultPage"]["selectionComplete"])
        self.assertIsNone(result["resultPage"]["item"])
        self.assertEqual(result["resultPage"]["text"], "")
        self.assertIsNone(result["nextCall"])

    def test_result_mode_shares_budget_with_slow_turn_metadata_lookup(self):
        work = self.start("result_slow_metadata")
        started = time.monotonic()
        result = self.client.call("codex.inspect", {
            "threadId": work["threadId"],
            "turnId": work["turnId"],
            "detail": "result",
        })
        elapsed = time.monotonic() - started
        self.assertGreaterEqual(elapsed, 1.8)
        self.assertLess(elapsed, 5.0)
        self.assertTrue(result["resultPage"]["selectionComplete"])
        self.assertEqual(result["resultPage"]["item"]["id"], "answer")

    def test_wait_surfaces_incomplete_selection_with_and_without_a_candidate(self):
        cases = [
            ("handoff_scan_incomplete_empty", None),
            ("handoff_scan_incomplete_priority", "commentary-511"),
        ]
        for scenario, expected_id in cases:
            with self.subTest(scenario=scenario):
                joined = self.wait(self.start(scenario))
                turn = joined["turn"]
                self.assertEqual(turn["selectionIncomplete"], True)
                if expected_id is None:
                    self.assertEqual(turn["output"], [])
                else:
                    self.assertEqual(len(turn["output"]), 1)
                    self.assertEqual(turn["output"][0]["id"], expected_id)
                    self.assertFalse(turn["output"][0]["truncated"])
                    result = self.client.call("codex.inspect", {
                        "threadId": joined["threadId"],
                        "turnId": joined["turnId"],
                        "detail": "result",
                    })
                    self.assertTrue(result["resultPage"]["selectionComplete"])
                    self.assertEqual(result["resultPage"]["item"]["id"], "older-final")
                    self.assertEqual(result["resultPage"]["text"], "older final")

    def test_oversized_wire_messages_are_contained(self):
        self.client.call("status")
        notification = self.start("wire_oversized")
        notification_result = self.wait(notification)
        self.assertEqual(notification_result["state"], "terminal")
        self.assertEqual(notification_result["wakeReason"], "terminal")
        self.assertTrue(self.inspect_turn(notification)["historyLost"])

        request = self.start("oversized_question")
        request_result = self.wait(request)
        if request_result["state"] != "terminal":
            request_result = self.wait(request)
        self.assertEqual(request_result["state"], "terminal")
        self.assertEqual(request_result["pendingActions"], [])

        status = self.client.call("status")
        self.assertTrue(status["ready"])

    def test_questions_wake_wait_and_preserve_the_same_turn(self):
        work = self.start("delayed_question")
        result = self.wait(work)
        self.assertEqual(result["state"],"active")
        self.assertEqual(result["wakeReason"],"inputRequired")
        pending = result["pendingActions"][0]
        self.assertEqual(pending["type"], "userInput")
        self.assertEqual(pending["questions"][0]["id"],"format")
        self.assertTrue(pending["blocking"])
        request_id = pending["requestId"]
        self.client.call("codex.act",{"action":"respondApproval","requestId":request_id,"decision":"approve"},error=True)
        self.client.call("codex.act",{"action":"respondUserInput","requestId":request_id,"answers":{"wrong":["JSON"]}},error=True)
        self.client.call("codex.act",{"action":"respondUserInput","requestId":request_id,"answers":{"format":["JSON"]}})
        completed = self.wait(work)
        self.assertEqual(completed["state"],"terminal")
        self.assertEqual(completed["wakeReason"],"terminal")
        self.client.call("codex.act",{"action":"respondUserInput","requestId":request_id,"answers":{"format":["JSON"]}},error=True)
        self.assertEqual(self.client.call("codex.wait",{
            "threadId": work["threadId"], "turnId": work["turnId"],
        })["pendingActions"], [])

    def test_nonblocking_question_does_not_wake_join_and_interrupt_cleans_up(self):
        work = self.start("nonblocking")
        result = self.wait(work)
        self.assertEqual(result["state"],"active")
        self.assertEqual(result["wakeReason"],"timeout")
        self.assertFalse(result["pendingActions"][0]["blocking"])
        self.client.call("codex.act",{"action":"interrupt","threadId":work["threadId"],"turnId":work["turnId"]})
        self.assertEqual(self.client.call("codex.wait",{
            "threadId": work["threadId"], "turnId": work["turnId"],
        })["pendingActions"], [])

    def test_typed_approval_permission_and_elicitation_paths(self):
        cases = [
            ("approval",{"action":"respondApproval","decision":"approve"}),
            ("file",{"action":"respondApproval","decision":"decline"}),
            ("permissions",{"action":"respondPermissions","permissions":{"network":{"enabled":True}},"scope":"turn"}),
        ]
        for scenario, answer in cases:
            with self.subTest(scenario=scenario):
                work = self.start(scenario)
                result = self.wait(work)
                self.assertEqual(result["state"],"active")
                self.assertEqual(result["wakeReason"],"actionRequired")
                request_id = result["pendingActions"][0]["requestId"]
                self.client.call("codex.act",{"requestId":request_id,**answer})
                completed = self.wait(work)
                self.assertEqual(completed["state"],"terminal")
                self.assertEqual(completed["wakeReason"],"terminal")

        for scenario, response in [
            ("form", {"disposition": "accept", "content": {"name": "Operator"}}),
            ("openai_form", {"disposition": "accept", "content": {"opaque": True}}),
            ("url", {"disposition": "cancel"}),
        ]:
            with self.subTest(scenario=scenario):
                work = self.start(scenario)
                result = self.wait(work)
                pending = result["pendingActions"][0]
                self.assertEqual(pending["type"], "elicitation")
                self.assertEqual(pending["request"]["mode"], scenario if scenario != "openai_form" else "openai/form")
                self.client.call("codex.act", {
                    "action": "respondElicitation", "requestId": pending["requestId"], **response,
                })
                self.assertEqual(self.wait(work)["state"], "terminal")

    def test_review_and_discovery(self):
        source = self.start("complete")
        source_result = self.wait(source)
        self.assertEqual(source_result["state"],"terminal")
        self.assertEqual(source_result["wakeReason"],"terminal")
        review = self.codex_start({
            "mode":"review", "threadId": source["threadId"], "target":{"type":"uncommittedChanges"},
        })
        self.assertNotIn("createdThread", review)
        self.assertEqual(review["threadId"], source["threadId"])
        result = self.wait(review)
        self.assertEqual(result["state"],"terminal")
        self.assertEqual(result["wakeReason"],"terminal")
        self.assertEqual(result["threadId"], source["threadId"])
        self.assertEqual(result["turnId"], review["turnId"])
        info = self.client.call("codex.query", {"queries":[
            {"type":"models"}, {"type":"skills"}, {"type":"usage"},
        ]})
        self.assertEqual([entry["type"] for entry in info["results"]], ["models", "skills", "usage"])
        self.assertEqual(
            [model["id"] for model in info["results"][0]["result"]["models"]],
            ["fixture-model-1", "fixture-model-2", "fixture-model-3"],
        )
        self.assertEqual(info["results"][0]["result"]["models"][0]["efforts"], ["low", "medium", "high"])
        self.assertIn("roots", info["results"][1]["result"])
        usage = info["results"][2]["result"]
        self.assertFalse(usage["ordinaryUsageAllowed"])
        self.assertEqual(usage["resetCreditsAvailable"], 2)
        self.assertEqual(usage["primary"]["usedPercent"], 100)
        self.assertNotIn("accountId", usage)
        self.client.call("codex.query", {"queries": []}, error=True, validate_input=False)
        self.client.call(
            "codex.query",
            {"queries": [{"type": "usage"}] * 11},
            error=True,
            validate_input=False,
        )

    def test_persisted_thread_query_archive_unarchive_and_delete(self):
        first = self.start("complete")
        second = self.start("complete")
        self.assertEqual(self.wait(first)["state"], "terminal")
        self.assertEqual(self.wait(second)["state"], "terminal")

        listed = self.client.call("codex.query", {"queries": [{
            "type": "threads", "limit": 50,
        }]})["results"][0]["result"]
        self.client.call("codex.query", {"queries": [{"type": "threads"}]})
        self.assertEqual(self.method_params("thread/list")[-1]["limit"], 25)
        listed_ids = {thread["threadId"] for thread in listed["threads"]}
        self.assertIn(first["threadId"], listed_ids)
        self.assertIn(second["threadId"], listed_ids)

        archived = self.client.call("codex.act", {
            "action": "setArchived",
            "threadIds": [first["threadId"], second["threadId"]],
            "archived": True,
        })
        self.assertEqual(
            {row["threadId"] for row in archived["results"] if row.get("archived")},
            {first["threadId"], second["threadId"]},
        )
        archived_list = self.client.call("codex.query", {"queries": [{
            "type": "threads", "archived": True, "limit": 50,
        }]})["results"][0]["result"]
        archived_ids = {thread["threadId"] for thread in archived_list["threads"]}
        self.assertTrue({first["threadId"], second["threadId"]}.issubset(archived_ids))

        restored = self.client.call("codex.act", {
            "action": "setArchived", "threadIds": [first["threadId"]], "archived": False,
        })
        self.assertTrue(restored["results"][0]["archived"] is False)

        deleted = self.client.call("codex.act", {
            "action": "delete", "threadIds": [first["threadId"], second["threadId"]],
        })
        self.assertTrue(all(row.get("deleted") for row in deleted["results"]))

    def test_persisted_thread_mutations_refuse_active_delegated_work(self):
        work = self.start("idle")
        archived = self.client.call("codex.act", {
            "action": "setArchived", "threadIds": [work["threadId"]], "archived": True,
        })
        self.assertIn("active delegated turn", archived["results"][0]["error"])
        deleted = self.client.call("codex.act", {
            "action": "delete", "threadIds": [work["threadId"]],
        })
        self.assertIn("active delegated turn", deleted["results"][0]["error"])
        self.client.call("codex.act", {
            "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
        })
        self.assertEqual(self.wait(work)["state"], "terminal")

    def test_persisted_thread_mutations_fail_closed_when_thread_state_is_unreadable(self):
        missing = "thread-does-not-exist"
        archived = self.client.call("codex.act", {
            "action": "setArchived", "threadIds": [missing], "archived": True,
        })
        self.assertIn("error", archived["results"][0])
        deleted = self.client.call("codex.act", {
            "action": "delete", "threadIds": [missing],
        })
        self.assertIn("error", deleted["results"][0])

    def test_thread_owned_background_terminals_are_queryable_and_terminable(self):
        work = self.start("background_terminal")
        self.assertEqual(self.wait(work)["state"], "terminal")
        queried = self.client.call("codex.query", {"queries": [{
            "type": "backgroundTerminals", "threadId": work["threadId"],
        }]})["results"][0]["result"]
        self.assertEqual(self.method_params("thread/backgroundTerminals/list")[-1]["limit"], 25)
        self.assertEqual(len(queried["terminals"]), 1)
        terminal = queried["terminals"][0]
        self.assertEqual(terminal["processId"], "background-process-1")
        self.assertEqual(terminal["osPid"], 4242)

        terminated = self.client.call("codex.act", {
            "action": "terminateBackgroundTerminal",
            "threadId": work["threadId"],
            "processId": terminal["processId"],
        })
        self.assertTrue(terminated["terminated"])
        empty = self.client.call("codex.query", {"queries": [{
            "type": "backgroundTerminals", "threadId": work["threadId"],
        }]})["results"][0]["result"]
        self.assertEqual(empty["terminals"], [])

    def test_new_thread_policy_projection_and_review_model_routing(self):
        before = len(self.method_params("thread/start"))
        work = self.start("complete")
        self.assertEqual(self.wait(work)["state"], "terminal")
        self.assertEqual(work["model"], "fixture-model-1")
        self.assertEqual(work["effort"], "medium")
        thread_start = self.method_params("thread/start")[before]
        self.assertEqual(thread_start["sandbox"], "workspace-write")
        self.assertEqual(thread_start["approvalPolicy"], "never")
        self.assertNotIn("developerInstructions", thread_start)
        turn_start = self.method_params("turn/start")[-1]
        self.assertNotIn("approvalPolicy", turn_start)
        self.assertEqual(turn_start["sandboxPolicy"]["type"], "workspaceWrite")
        self.assertTrue(turn_start["sandboxPolicy"]["networkAccess"])
        self.assertEqual(turn_start["sandboxPolicy"]["writableRoots"], [])
        self.assertEqual(turn_start["model"], "fixture-model-1")
        self.assertEqual(turn_start["effort"], "medium")

        before = len(self.method_params("thread/start"))
        unrestricted = self.start("complete", access="full")
        self.assertEqual(self.wait(unrestricted)["state"], "terminal")
        full_thread_start = self.method_params("thread/start")[before]
        self.assertEqual(full_thread_start["sandbox"], "danger-full-access")
        self.assertEqual(full_thread_start["approvalPolicy"], "never")
        full_turn_start = self.method_params("turn/start")[-1]
        self.assertNotIn("approvalPolicy", full_turn_start)
        self.assertEqual(full_turn_start["sandboxPolicy"]["type"], "dangerFullAccess")

        before = len(self.method_params("thread/start"))
        configured = self.start(
            "complete",
            model="fixture-model-2",
            effort="high",
        )
        self.assertEqual(self.wait(configured)["state"], "terminal")
        self.assertEqual(configured["model"], "fixture-model-2")
        self.assertEqual(configured["effort"], "high")
        configured_thread_start = self.method_params("thread/start")[before]
        self.assertEqual(configured_thread_start["model"], "fixture-model-2")
        configured_turn_start = self.method_params("turn/start")[-1]
        self.assertEqual(configured_turn_start["model"], "fixture-model-2")
        self.assertEqual(configured_turn_start["effort"], "high")

        for hidden in [
            {"sandboxPolicy": {"type": "readOnly"}},
            {"developerInstructions": "Do something different."},
            {"serviceTier": "priority"},
        ]:
            self.client.call(
                "codex.start",
                {"mode": "work", "task": "complete", **hidden},
                error=True,
                validate_input=False,
            )
        self.client.call(
            "codex.start",
            {"mode": "work", "task": "complete", "access": "invalid"},
            error=True,
            validate_input=False,
        )

        source = self.start("complete")
        self.assertEqual(self.wait(source)["state"], "terminal")
        resume_count = len(self.method_params("thread/resume"))
        resumed = self.start("complete", threadId=source["threadId"])
        self.assertEqual(self.wait(resumed)["state"], "terminal")
        self.assertEqual(resumed["model"], source["model"])
        self.assertEqual(resumed["effort"], source["effort"])
        self.assertEqual(len(self.method_params("thread/resume")), resume_count + 1)
        self.assertNotIn("sandbox", self.method_params("thread/resume")[-1])
        self.assertNotIn("cwd", self.method_params("thread/resume")[-1])
        self.assertNotIn("developerInstructions", self.method_params("thread/resume")[-1])
        resumed_turn_start = self.method_params("turn/start")[-1]
        for override in ["approvalPolicy", "sandboxPolicy"]:
            self.assertNotIn(override, resumed_turn_start)
        self.assertEqual(resumed_turn_start["model"], source["model"])
        self.assertEqual(resumed_turn_start["effort"], source["effort"])

        for incompatible in [
            {"access": "full"},
            {"model": "fixture-model-2"},
            {"effort": "high"},
            {"cwd": str(ROOT)},
        ]:
            self.client.call(
                "codex.start",
                {
                    "mode": "work",
                    "task": "complete",
                    "threadId": source["threadId"],
                    **incompatible,
                },
                error=True,
                validate_input=False,
            )

        expired = self.start("expire_thread")
        self.assertEqual(self.wait(expired)["state"], "terminal")
        # Cold source: no live usage telemetry, so no cache metrics exist.
        cold = self.inspect_turn(expired)
        self.assertIsNone(cold["currentActivity"]["tokenUsage"]["lastModelUsageAtMs"])
        self.assertIsNone(cold["currentActivity"]["tokenUsage"]["cacheGuaranteedUntilMs"])
        self.assertIsNone(cold["currentActivity"]["tokenUsage"]["cacheGuaranteeActive"])
        # Stale fork first, while the source is provably untouched since expiry.
        forks_before = len(self.method_params("thread/fork"))
        forked = self.codex_start({
            "mode": "work",
            "task": "complete",
            "forkFromThreadId": expired["threadId"],
        })
        self.assertNotEqual(forked["threadId"], expired["threadId"])
        self.assertEqual(self.wait(forked)["state"], "terminal")
        fork_call = self.method_params("thread/fork")[forks_before]
        self.assertEqual(fork_call, {
            "threadId": expired["threadId"],
            "excludeTurns": True,
        })
        fork_summary = self.client.call("codex.query", {"queries": [{
            "type": "thread", "threadId": forked["threadId"],
        }]})["results"][0]["result"]
        self.assertEqual(fork_summary["forkedFromThreadId"], expired["threadId"])
        # Stale cache never gates native resume: the expired thread still reaches upstream.
        resumes_before_expired = len(self.method_params("thread/resume"))
        stale_resumed = self.codex_start({
            "mode": "work",
            "task": "complete",
            "threadId": expired["threadId"],
            "model": expired["model"],
        })
        self.assertEqual(self.wait(stale_resumed)["state"], "terminal")
        self.assertEqual(len(self.method_params("thread/resume")), resumes_before_expired + 1)
        # Native permission checks still hold on stale threads.
        stale_rejected = len(self.method_params("thread/resume"))
        self.client.call(
            "codex.start",
            {
                "mode": "work",
                "task": "complete",
                "threadId": expired["threadId"],
                "model": "fixture-model-2",
            },
            error=True,
            validate_input=False,
        )
        self.assertEqual(len(self.method_params("thread/resume")), stale_rejected)

        before_threads = len(self.method_params("thread/start"))
        before_reviews = len(self.method_params("review/start"))
        review = self.codex_start({
            "mode": "review",
            "model": "gpt-6-astra",
            "target": {"type": "uncommittedChanges"},
        })
        self.assertNotIn("createdThread", review)
        self.assertEqual(self.wait(review)["state"], "terminal")
        review_thread_start = self.method_params("thread/start")[before_threads]
        self.assertEqual(review_thread_start["model"], "gpt-6-astra")
        self.assertEqual(review_thread_start["sandbox"], "read-only")
        review_start = self.method_params("review/start")[before_reviews]
        self.assertEqual(set(review_start), {"threadId", "target", "delivery"})

        before_threads = len(self.method_params("thread/start"))
        self.client.call("codex.start", {
            "mode": "review", "cwd": str(self.workspace),
            "target": {"type": "uncommittedChanges"},
        }, error=True, validate_input=False)
        self.assertEqual(len(self.method_params("thread/start")), before_threads)

        resumes_before_rejection = len(self.method_params("thread/resume"))
        self.codex_start({
            "mode": "review",
            "threadId": source["threadId"],
            "model": "gpt-6-astra",
            "target": {"type": "uncommittedChanges"},
        }, error=True, validate_input=False)
        self.assertEqual(len(self.method_params("thread/resume")), resumes_before_rejection)

    def test_workspace_writable_roots_forwarding_and_lifecycle_wire_defaults(self):
        roots = [str(self.workspace / "selected-one"), str(self.outside_path / "selected-two")]
        for access in [None, "workspace"]:
            with self.subTest(access=access):
                arguments = {"cwd": str(self.project), "writableRoots": roots}
                if access is not None:
                    arguments["access"] = access
                work = self.start("complete", **arguments)
                self.assertEqual(self.wait(work)["state"], "terminal")
                policy = self.method_params("turn/start")[-1]["sandboxPolicy"]
                self.assertEqual(policy, {
                    "type": "workspaceWrite", "writableRoots": roots,
                    "networkAccess": True, "excludeSlashTmp": False, "excludeTmpdirEnvVar": False,
                })
                self.assertEqual(work["cwd"], str(self.project))
                self.assertEqual(self.method_params("thread/start")[-1]["cwd"], str(self.project))
                self.assertEqual(self.method_params("turn/start")[-1]["cwd"], str(self.project))

                # Wire evidence only: upstream persistence is not exercised by this peer.
                resumed = self.start("complete", threadId=work["threadId"])
                self.assertEqual(self.wait(resumed)["state"], "terminal")
                self.assertNotIn("sandboxPolicy", self.method_params("turn/start")[-1])
                self.assertNotIn("sandbox", self.method_params("thread/resume")[-1])
                forked = self.start("complete", forkFromThreadId=work["threadId"], lastTurnId=work["turnId"])
                self.assertEqual(self.wait(forked)["state"], "terminal")
                self.assertNotIn("sandboxPolicy", self.method_params("turn/start")[-1])
                self.assertNotIn("sandbox", self.method_params("thread/fork")[-1])
                self.assertEqual(resumed["cwd"], str(self.project))
                self.assertEqual(forked["cwd"], str(self.project))

        for arguments in [{}, {"writableRoots": []}, {"access": "workspace", "writableRoots": []}]:
            with self.subTest(arguments=arguments):
                work = self.start("complete", **arguments)
                self.assertEqual(self.wait(work)["state"], "terminal")
                policy = self.method_params("turn/start")[-1]["sandboxPolicy"]
                self.assertEqual(policy["writableRoots"], [])
                self.assertTrue(policy["networkAccess"])

    def test_writable_roots_invalid_inputs_fail_before_upstream_mutation(self):
        source = self.start("complete")
        self.assertEqual(self.wait(source)["state"], "terminal")
        before = {method: len(self.method_params(method)) for method in [
            "thread/start", "thread/resume", "thread/fork", "turn/start", "review/start",
        ]}
        base = {"mode": "work", "task": "complete", "cwd": str(self.project), "model": source["model"]}
        for root in ["relative/path", "../sibling", "", "~/project"]:
            result = self.client.call("codex.start", {**base, "writableRoots": [str(self.project), root]},
                                      error=True, validate_input=False)
            self.assertIn("writableRoots must contain absolute paths", result["content"][0]["text"])

        schema = jsonschema.Draft202012Validator(self.client.tools["codex.start"]["inputSchema"])
        for roots in [[], [str(self.outside_path)]]:
            for arguments in [
                {**base, "access": "full", "writableRoots": roots},
                {"mode": "review", "cwd": str(self.project), "model": source["model"],
                 "target": {"type": "uncommittedChanges"}, "writableRoots": roots},
                {"mode": "review", "threadId": source["threadId"], "model": source["model"],
                 "target": {"type": "uncommittedChanges"}, "writableRoots": roots},
                {"mode": "work", "task": "complete", "model": source["model"],
                 "threadId": source["threadId"], "writableRoots": roots},
                {"mode": "work", "task": "complete", "model": source["model"],
                 "forkFromThreadId": source["threadId"], "writableRoots": roots},
            ]:
                with self.subTest(arguments=arguments):
                    self.assertFalse(schema.is_valid(arguments))
                    result = self.client.call("codex.start", arguments, error=True, validate_input=False)
                    message = result["content"][0]["text"]
                    self.assertIn("writableRoots", message)
                    if arguments["mode"] == "work" and ("threadId" in arguments or "forkFromThreadId" in arguments):
                        self.assertIn("not guaranteed", message)
        for roots in ["/project", [123]]:
            result = self.client.call("codex.start", {**base, "writableRoots": roots}, error=True, validate_input=False)
            self.assertTrue(result["isError"])
        for method, count in before.items():
            self.assertEqual(len(self.method_params(method)), count)

    def test_start_requires_explicit_cwd_and_model_without_mutating_existing_threads(self):
        before_threads = len(self.method_params("thread/start"))
        for arguments in [
            {"mode": "work", "task": "complete", "model": "fixture-model-1"},
            {"mode": "work", "task": "complete", "cwd": str(self.project)},
            {"mode": "review", "target": {"type": "uncommittedChanges"}, "model": "fixture-model-1"},
            {"mode": "review", "target": {"type": "uncommittedChanges"}, "cwd": str(self.project)},
        ]:
            self.client.call("codex.start", arguments, error=True, validate_input=False)
        self.assertEqual(len(self.method_params("thread/start")), before_threads)

        source = self.start("complete", cwd=str(self.project))
        self.assertEqual(self.wait(source)["state"], "terminal")
        self.assertEqual(source["cwd"], str(self.project))
        before_resume = len(self.method_params("thread/resume"))
        before_fork = len(self.method_params("thread/fork"))
        for arguments in [
            {"mode": "work", "task": "complete", "threadId": source["threadId"]},
            {"mode": "work", "task": "complete", "forkFromThreadId": source["threadId"]},
            {"mode": "review", "threadId": source["threadId"], "target": {"type": "uncommittedChanges"}},
        ]:
            self.client.call("codex.start", arguments, error=True, validate_input=False)
        for arguments in [
            {"mode": "work", "task": "complete", "threadId": source["threadId"],
             "model": "fixture-model-2"},
            {"mode": "work", "task": "complete", "forkFromThreadId": source["threadId"],
             "model": "fixture-model-2"},
            {"mode": "review", "threadId": source["threadId"],
             "target": {"type": "uncommittedChanges"}, "model": "fixture-model-2"},
        ]:
            self.client.call("codex.start", arguments, error=True, validate_input=False)
        self.assertEqual(len(self.method_params("thread/resume")), before_resume)
        self.assertEqual(len(self.method_params("thread/fork")), before_fork)
        resumed = self.start("complete", threadId=source["threadId"])
        self.assertEqual(self.wait(resumed)["state"], "terminal")
        self.assertEqual(resumed["cwd"], str(self.project))
        self.assertEqual(resumed["model"], source["model"])
        forked = self.codex_start({"mode": "work", "task": "complete",
                                   "forkFromThreadId": source["threadId"]})
        self.assertEqual(self.wait(forked)["state"], "terminal")
        self.assertEqual(forked["cwd"], str(self.project))
        self.assertEqual(forked["model"], source["model"])

    def test_concurrent_projects_and_cwd_fair_retention(self):
        first = self.start("idle", cwd=str(self.workspace))
        second = self.start("idle", cwd=str(self.project))
        status = self.client.call("status")
        self.assertEqual(status["defaultCwd"], str(self.workspace))
        active = {worker["turnId"]: worker for worker in status["workers"]}
        self.assertEqual(active[first["turnId"]]["cwd"], str(self.workspace))
        self.assertEqual(active[second["turnId"]]["cwd"], str(self.project))
        for work in (first, second):
            self.client.call("codex.act", {
                "action": "interrupt", "threadId": work["threadId"], "turnId": work["turnId"],
            })
            self.assertEqual(self.wait(work)["state"], "terminal")

        sentinel = self.start("complete", cwd=str(self.project))
        self.assertEqual(self.wait(sentinel)["state"], "terminal")
        for _ in range(34):
            work = self.start("complete", cwd=str(self.workspace))
            self.assertEqual(self.wait(work)["state"], "terminal")
        retained = self.client.call("status")["workers"]
        self.assertTrue(any(worker["turnId"] == sentinel["turnId"]
                            and worker["cwd"] == str(self.project) for worker in retained))
        self.assertLessEqual(sum(worker["status"] != "inProgress" for worker in retained), 32)

        command = self.client.call("command.start", {
            "command": ["fixture-exit"], "cwd": str(self.project),
        })
        self.client.call("command.read", {"processId": command["processId"], "timeoutMs": 1000})
        for _ in range(34):
            started = self.client.call("command.start", {
                "command": ["fixture-exit"], "cwd": str(self.workspace),
            })
            self.client.call("command.read", {"processId": started["processId"], "timeoutMs": 1000})
        commands = self.client.call("status")["commands"]
        self.assertTrue(any(row["processId"] == command["processId"]
                            and row["cwd"] == str(self.project) for row in commands))
        self.assertLessEqual(len(commands), 32)

    def test_wait_tracks_review_turn_before_thread_history_catches_up(self):
        source = self.start("complete")
        self.assertEqual(self.wait(source)["state"], "terminal")
        review = self.codex_start({
            "mode": "review", "threadId": source["threadId"],
            "target": {"type": "custom", "instructions": "delayed_visibility"},
        })
        result = self.wait(review)
        self.assertEqual(result["state"], "terminal")
        self.assertEqual(result["wakeReason"], "terminal")
        self.assertEqual(result["turnId"], review["turnId"])
        self.assertEqual(result["turn"]["status"], "failed")

    def test_contract_surface_is_completely_reachable_and_schema_valid(self):
        inspected = self.client.call("host.inspect", {"operations": [
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
        self.client.call("codex.act", {
            "action": "steer", "threadId": resumed["threadId"],
            "expectedTurnId": resumed["turnId"],
            "instruction": "continue",
        })
        self.client.call("codex.act", {
            "action": "interrupt", "threadId": resumed["threadId"], "turnId": resumed["turnId"],
        })
        self.assertEqual(self.wait(resumed)["state"], "terminal")

        review = self.codex_start({
            "mode": "review", "threadId": source["threadId"], "target": {"type": "uncommittedChanges"},
        })
        self.assertEqual(self.wait(review)["state"], "terminal")

        forked = self.codex_start({
            "mode": "work", "task": "complete", "forkFromThreadId": source["threadId"],
        })
        self.assertEqual(self.wait(forked)["state"], "terminal")
        self.client.call("codex.query", {"queries": [
            {"type": "threads", "limit": 10},
            {"type": "thread", "threadId": forked["threadId"]},
        ]})

        lifecycle = self.start("complete")
        self.assertEqual(self.wait(lifecycle)["state"], "terminal")
        self.client.call("codex.act", {
            "action": "setArchived", "threadIds": [lifecycle["threadId"]], "archived": True,
        })
        self.client.call("codex.act", {
            "action": "setArchived", "threadIds": [lifecycle["threadId"]], "archived": False,
        })
        self.client.call("codex.act", {
            "action": "delete", "threadIds": [lifecycle["threadId"]],
        })

        background = self.start("background_terminal")
        self.assertEqual(self.wait(background)["state"], "terminal")
        terminals = self.client.call("codex.query", {"queries": [{
            "type": "backgroundTerminals", "threadId": background["threadId"],
        }]})["results"][0]["result"]["terminals"]
        self.client.call("codex.act", {
            "action": "terminateBackgroundTerminal",
            "threadId": background["threadId"],
            "processId": terminals[0]["processId"],
        })

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
            ("approval", {"action": "respondApproval", "decision": "approve"}),
            ("file", {"action": "respondApproval", "decision": "decline"}),
            ("permissions", {"action": "respondPermissions", "permissions": {"network": {"enabled": True}}, "scope": "turn"}),
            ("question", {"action": "respondUserInput", "answers": {"format": ["JSON"]}}),
        ]:
            work = self.start(scenario)
            pending = self.wait(work)["pendingActions"][0]
            self.client.call("codex.act", {"requestId": pending["requestId"], **answer})
            self.assertEqual(self.wait(work)["state"], "terminal")

        elicitation = self.start("form")
        pending = self.wait(elicitation)["pendingActions"][0]
        self.assertEqual(pending["type"], "elicitation")
        self.client.call("codex.act", {
            "action": "respondElicitation", "requestId": pending["requestId"],
            "disposition": "accept", "content": {"name": "Operator"},
        })
        self.assertEqual(self.wait(elicitation)["state"], "terminal")

        self.client.call("codex.query", {"queries":[
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
        self.assertEqual(set(persistent), {"processId", "cwd", "output", "readError"})
        try:
            self.client.call("command.exec",{"command":["disconnect"]},error=True)
        except (urllib.error.URLError, ConnectionError):
            pass
        self.assertNotEqual(self.process.wait(timeout=5),0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
