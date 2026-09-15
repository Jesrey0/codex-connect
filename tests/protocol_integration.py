#!/usr/bin/env python3
"""Exercise the real MCP/relay/transport stack against a pinned-schema-valid peer.

Run after cargo build -p codex-connect. Requires Python jsonschema.
"""

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
        (cls.scope / "sample.txt").write_text("one\ntwo\nthree\n")
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
        ]})
        self.assertEqual(result["results"][0]["result"]["text"], "two")
        self.client.call("codexConnect.inspect", {"operations": [{"type":"readText","path":"/etc/passwd"}]}, error=True)
        self.client.call("apply_patch", {"patch":"*** Begin Patch\n*** Add File: patch.txt\n+created\n*** End Patch"})
        self.assertEqual((self.scope / "patch.txt").read_text(), "created\n")

    def test_command_boundaries(self):
        result = self.client.call("command.exec", {"command":["echo","fixture"]})
        self.assertEqual(result["exitCode"],0)
        for arguments in [
            {"command":[]}, {"command":["echo"],"tty":True},
            {"command":["echo"],"disableTimeout":True},
            {"command":["echo"],"disableOutputCap":True},
            {"command":["echo"],"timeoutMs":300001},
            {"command":["echo"],"sandboxPolicy":{"type":"externalSandbox"}},
            {"command":["echo"],"cwd":"/etc"},
        ]:
            self.client.call("command.exec",arguments,error=True,validate_input=False)

    def test_authoritative_completion_without_events_and_unknown_turn(self):
        work = self.start("no_event")
        result = self.wait(work)
        self.assertEqual(result["state"],"completed")
        self.assertEqual(result["turn"]["output"][0]["text"],"fixture complete")
        self.client.call("codexConnect.work.wait",{"threadId":work["threadId"],"turnId":"missing","timeoutMs":0},error=True)
        next_work = self.start("idle",threadId=work["threadId"])
        self.assertFalse(next_work["createdThread"])
        self.assertEqual(self.wait(next_work,timeout=0)["state"],"timeout")
        self.assertEqual(self.client.call("codexConnect.work.read",{"threadId":work["threadId"]})["turnCount"],2)

    def test_wait_progress_timeout_oversized_and_steer(self):
        for scenario, expected in [("idle","timeout"),("progress","progress"),("oversized","progress")]:
            work = self.start(scenario)
            result = self.wait(work,timeout=100)
            self.assertEqual(result["state"],expected)
            if scenario == "oversized":
                self.assertTrue(result["events"][0]["truncated"])
            self.client.call("codexConnect.work.steer",{"threadId":work["threadId"],"expectedTurnId":work["turnId"],"instruction":"continue"})
            self.client.call("codexConnect.work.interrupt",{"threadId":work["threadId"],"turnId":work["turnId"]})
            self.assertEqual(self.wait(work)["turn"]["status"],"interrupted")

    def test_questions_wake_wait_and_preserve_the_same_turn(self):
        work = self.start("delayed_question")
        result = self.wait(work,timeout=3000)
        self.assertEqual(result["state"],"waitingForInput")
        pending = result["pendingActions"][0]
        self.assertEqual(pending["params"]["questions"][0]["id"],"format")
        self.assertTrue(pending["isBlocking"])
        request_id = pending["requestId"]
        self.client.call("codexConnect.approval.respond",{"requestId":request_id,"decision":"approve"},error=True)
        self.client.call("codexConnect.userInput.respond",{"requestId":request_id,"answers":{"wrong":["JSON"]}},error=True)
        self.client.call("codexConnect.userInput.respond",{"requestId":request_id,"answers":{"format":["JSON"]}})
        self.assertEqual(self.wait(work)["state"],"completed")
        self.client.call("codexConnect.userInput.respond",{"requestId":request_id,"answers":{"format":["JSON"]}},error=True)
        actions = self.client.call("codexConnect.pendingActions.list",{"threadId":work["threadId"]})["actions"]
        self.assertEqual(actions,[])

    def test_nonblocking_question_is_progress_and_interrupt_cleans_up(self):
        work = self.start("nonblocking")
        result = self.wait(work)
        self.assertEqual(result["state"],"progress")
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
                self.assertEqual(result["state"],"waitingForAction")
                request_id = result["pendingActions"][0]["requestId"]
                self.client.call(responder,{"requestId":request_id,**answer})
                self.assertEqual(self.wait(work)["state"],"completed")

    def test_review_and_discovery(self):
        review = self.client.call("codexConnect.review",{"target":{"type":"uncommittedChanges"}})
        self.assertEqual(self.wait(review)["state"],"completed")
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
