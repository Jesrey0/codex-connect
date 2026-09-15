"""Small Streamable HTTP client used by protocol and live smoke tests."""

import itertools
import json
import urllib.request

import jsonschema


class McpClient:
    def __init__(self, url):
        self.url = url.rstrip("/")
        self.ids = itertools.count(1)
        self.session = None
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "codex-connect-smoke", "version": ""}})
        self.request("notifications/initialized", notification=True)
        self.catalog = self.request("tools/list")["tools"]
        self.tools = {t["name"]: t for t in self.catalog}

    def request(self, method, params=None, notification=False):
        value = {"jsonrpc": "2.0", "method": method}
        if not notification:
            value["id"] = next(self.ids)
        if params is not None:
            value["params"] = params
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-06-18"}
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        request = urllib.request.Request(self.url + "/mcp", json.dumps(value).encode(), headers)
        with urllib.request.urlopen(request, timeout=40) as response:
            self.session = response.headers.get("Mcp-Session-Id", self.session)
            raw = response.read().decode()
        if not raw:
            return None
        if raw.startswith("event:") or raw.startswith("data:"):
            data = [json.loads(line[5:].strip()) for line in raw.splitlines() if line.startswith("data:")]
            value = next(x for x in data if x.get("id") == value.get("id"))
        else:
            value = json.loads(raw)
        assert "error" not in value, value
        return value["result"]

    def call(self, name, arguments=None, error=False, validate_input=True):
        arguments = arguments or {}
        if validate_input:
            jsonschema.Draft202012Validator(self.tools[name]["inputSchema"]).validate(arguments)
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        if error:
            assert result.get("isError"), result
            return result
        assert not result.get("isError"), result
        content = result["structuredContent"]
        jsonschema.Draft202012Validator(self.tools[name]["outputSchema"]).validate(content)
        text = [x["text"] for x in result.get("content", []) if x["type"] == "text"]
        assert all(len(t) < 1024 for t in text), "structured JSON duplicated in text"
        return content
