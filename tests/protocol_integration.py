#!/usr/bin/env python3
"""Exercise the real MCP/relay/transport stack against a pinned-schema-valid peer.

Run after cargo build -p codex-connect. Requires Python jsonschema.
"""

import base64
import json
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
EXPECTED = {
    "codexConnect.status", "codexConnect.inspect", "apply_patch", "view_image", "command.exec",
    "codexConnect.work.start", "codexConnect.work.read", "codexConnect.work.wait",
    "codexConnect.work.steer", "codexConnect.work.interrupt", "codexConnect.pendingActions.list",
    "codexConnect.approval.respond", "codexConnect.permissions.respond",
    "codexConnect.elicitation.respond", "codexConnect.userInput.respond",
    "codexConnect.review", "model.list", "skills.list", "codexConnect.usage",
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
        ], stdout=cls.log, stderr=cls.log)
        try:
            for _ in range(200):
                if cls.process.poll() is not None:
                    raise RuntimeError("fixture backend exited")
                try:
                    with urllib.request.urlopen(cls.url + "/healthz", timeout=1):
                        break
                except urllib.error.URLError:
                    time.sleep(0.025)
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
        return self.client.call("codexConnect.work.start", {"task": scenario, **arguments})

    def wait(self, work, timeout=1000, **arguments):
        return self.client.call("codexConnect.work.wait", {
            "threadId": work["threadId"], "turnId": work["turnId"], "afterCursor": work["cursor"],
            "timeoutMs": timeout, **arguments,
        })

    def test_catalog_and_status_are_canonical(self):
        self.assertEqual(len(self.client.catalog), 19)
        self.assertEqual(set(self.client.tools), EXPECTED)
        wait_timeout = self.client.tools["codexConnect.work.wait"]["inputSchema"]["properties"]["timeoutMs"]
        self.assertEqual(wait_timeout["default"], 60000)
        self.assertEqual(wait_timeout["maximum"], 120000)
        status = self.client.call("codexConnect.status")
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
        result = self.client.call("codexConnect.inspect", {"operations": [
            {"type": "readText", "path": "sample.txt", "startLine": 2, "endLine": 2},
            {"type": "searchContent", "query": "two", "maxResults": 1},
            {"type": "searchNames", "query": "sample", "maxResults": 1},
            {"type": "metadata", "path": "sample.txt"},
            {"type": "readDirectory", "path": "."},
            {"type": "fuzzyFileSearch", "query": "smp", "path": "."},
        ]})
        self.assertEqual(result["results"][0]["result"]["text"], "two")
        self.assertEqual(set(result["results"][3]["result"]), {
            "createdAtMs", "isDirectory", "isFile", "isSymlink", "modifiedAtMs",
        })
        self.assertTrue(result["results"][3]["result"]["isFile"])
        self.assertIn(
            "sample.txt",
            {entry["fileName"] for entry in result["results"][4]["result"]["entries"]},
        )
        fuzzy = result["results"][5]["result"]["files"][0]
        self.assertEqual(fuzzy["path"], "sample.txt")
        self.assertEqual(fuzzy["match_type"], "file")
        self.assertEqual(fuzzy["score"], 100)
        escaped = self.client.call("codexConnect.inspect", {"operations": [
            {"type": "fuzzyFileSearch", "query": "external", "path": "."},
        ]})
        self.assertEqual(escaped["results"][0]["result"]["files"], [])
        large = self.scope / "large.txt"
        large.write_bytes(b"x" * (7 * 1024 * 1024))
        large_result = self.client.call("codexConnect.inspect", {"operations": [
            {"type": "readText", "path": "large.txt", "startLine": 1, "endLine": 1},
        ]})
        self.assertEqual(large_result["results"][0]["index"], 0)
        self.assertIn("safe fs/readFile transport limit", large_result["results"][0]["error"])
        large_directory = self.scope / "large-directory"
        large_directory.mkdir()
        suffix = "x" * 240
        for index in range(28_000):
            (large_directory / f"{index:05d}-{suffix}").touch()
        large_directory_result = self.client.call("codexConnect.inspect", {"operations": [
            {"type": "readDirectory", "path": "large-directory"},
        ]})
        self.assertIn("directory listing exceeds", large_directory_result["results"][0]["error"])
        healthy = self.client.call("codexConnect.inspect", {"operations": [
            {"type": "readText", "path": "sample.txt", "startLine": 1, "endLine": 1},
        ]})
        self.assertEqual(healthy["results"][0]["result"]["text"], "one")
        partial = self.client.call("codexConnect.inspect", {"operations": [
            {"type":"readText","path":"/etc/passwd"},
            {"type":"readText","path":"sample.txt","startLine":3,"endLine":3},
        ]})
        self.assertEqual(partial["results"][0]["index"], 0)
        self.assertIn("outside the configured scope root", partial["results"][0]["error"])
        self.assertEqual(partial["results"][1]["index"], 1)
        self.assertEqual(partial["results"][1]["result"]["text"], "three")
        escaped_fuzzy = self.client.call("codexConnect.inspect", {"operations": [
            {"type":"fuzzyFileSearch","query":"etc","path":"/etc"},
        ]})
        self.assertIn("outside the configured scope root", escaped_fuzzy["results"][0]["error"])
        self.client.call("codexConnect.inspect", {
            "cwd":"/etc", "operations":[{"type":"readDirectory","path":"."}],
        }, error=True)

    def test_request_cwd_applies_consistently_to_paths_patch_and_image(self):
        cwd = str(self.project)
        inspected = self.client.call("codexConnect.inspect", {"cwd":cwd,"operations":[
            {"type":"readText","path":"local.txt"},
            {"type":"searchContent","query":"project-local"},
            {"type":"searchNames","query":"local"},
        ]})
        self.assertEqual(inspected["results"][0]["result"]["text"], "project-local")
        self.assertEqual(inspected["results"][1]["result"]["matches"][0]["path"], "project/local.txt")
        self.assertEqual(inspected["results"][2]["result"]["paths"], ["project/local.txt"])
        self.client.call("apply_patch", {
            "cwd":cwd,
            "patch":"*** Begin Patch\n*** Add File: patch.txt\n+created\n*** End Patch",
        })
        self.assertEqual((self.project / "patch.txt").read_text(), "created\n")
        image = self.client.call("view_image", {"cwd":cwd,"path":"pixel.png"})
        self.assertEqual(image["path"], "project/pixel.png")
        self.assertEqual(image["mimeType"], "image/png")
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

    def test_authoritative_completion_without_events_and_unknown_turn(self):
        work = self.start("no_event")
        result = self.wait(work)
        self.assertEqual(result["state"],"terminal")
        self.assertEqual(result["wakeReason"],"terminal")
        self.assertEqual(result["turn"]["output"][0]["text"],"fixture complete")
        self.client.call("codexConnect.work.wait",{"threadId":work["threadId"],"turnId":"missing","timeoutMs":0},error=True)
        next_work = self.start("idle",threadId=work["threadId"])
        self.assertFalse(next_work["createdThread"])
        idle = self.wait(next_work,timeout=0)
        self.assertEqual(idle["state"],"active")
        self.assertEqual(idle["wakeReason"],"timeout")
        self.assertEqual(self.client.call("codexConnect.work.read",{"threadId":work["threadId"]})["turnCount"],2)

    def test_wait_ignores_progress_until_lease_expiry_and_preserves_journal(self):
        for scenario in ("idle","progress","oversized"):
            work = self.start(scenario)
            result = self.wait(work,timeout=100)
            self.assertEqual(result["state"],"active")
            self.assertEqual(result["wakeReason"],"timeout")
            if scenario == "progress":
                self.assertEqual(result["events"][0]["method"],"item/agentMessage/delta")
            if scenario == "oversized":
                self.assertTrue(result["events"][0]["truncated"])
            self.client.call("codexConnect.work.steer",{"threadId":work["threadId"],"expectedTurnId":work["turnId"],"instruction":"continue"})
            self.client.call("codexConnect.work.interrupt",{"threadId":work["threadId"],"turnId":work["turnId"]})
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

        status = self.client.call("codexConnect.status")
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
        self.client.call("codexConnect.approval.respond",{"requestId":request_id,"decision":"approve"},error=True)
        self.client.call("codexConnect.userInput.respond",{"requestId":request_id,"answers":{"wrong":["JSON"]}},error=True)
        self.client.call("codexConnect.userInput.respond",{"requestId":request_id,"answers":{"format":["JSON"]}})
        completed = self.wait(work)
        self.assertEqual(completed["state"],"terminal")
        self.assertEqual(completed["wakeReason"],"terminal")
        self.client.call("codexConnect.userInput.respond",{"requestId":request_id,"answers":{"format":["JSON"]}},error=True)
        actions = self.client.call("codexConnect.pendingActions.list",{"threadId":work["threadId"]})["actions"]
        self.assertEqual(actions,[])

    def test_nonblocking_question_does_not_wake_join_and_interrupt_cleans_up(self):
        work = self.start("nonblocking")
        result = self.wait(work,timeout=100)
        self.assertEqual(result["state"],"active")
        self.assertEqual(result["wakeReason"],"timeout")
        self.assertFalse(result["pendingActions"][0]["isBlocking"])
        self.client.call("codexConnect.work.interrupt",{"threadId":work["threadId"],"turnId":work["turnId"]})
        self.assertEqual(self.client.call("codexConnect.pendingActions.list",{"threadId":work["threadId"]})["actions"],[])

    def test_typed_approval_permission_and_elicitation_wire_responses(self):
        cases = [
            ("approval","codexConnect.approval.respond",{"decision":"approve"}),
            ("file","codexConnect.approval.respond",{"decision":"decline"}),
            ("permissions","codexConnect.permissions.respond",{"permissions":{"network":{"enabled":True}},"scope":"turn"}),
            ("form","codexConnect.elicitation.respond",{"action":"accept","content":{"name":"Ada"}}),
            ("url","codexConnect.elicitation.respond",{"action":"accept"}),
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
        review = self.client.call("codexConnect.review",{
            "threadId": source["threadId"], "target":{"type":"uncommittedChanges"},
        })
        self.assertFalse(review["createdThread"])
        self.assertEqual(review["threadId"], source["threadId"])
        result = self.wait(review)
        self.assertEqual(result["state"],"terminal")
        self.assertEqual(result["wakeReason"],"terminal")
        self.assertEqual(result["threadId"], source["threadId"])
        self.assertEqual(result["turnId"], review["turnId"])
        self.client.call("model.list")
        self.client.call("skills.list")
        self.client.call("codexConnect.usage")

    def test_z_disconnect_exits_backend_for_service_recovery(self):
        self.start("question")
        try:
            self.client.call("command.exec",{"command":["disconnect"]},error=True)
        except (urllib.error.URLError, ConnectionError):
            pass
        self.assertNotEqual(self.process.wait(timeout=5),0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
