"""Small Streamable HTTP client used by protocol and live smoke tests."""

import itertools
import http.client
import json
import urllib.request

import jsonschema


class McpClient:
    def __init__(self, url, request_timeout=40):
        self.url = url.rstrip("/")
        self.request_timeout = request_timeout
        self.ids = itertools.count(1)
        self.session = None
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "codex-connect-smoke", "version": ""}})
        self.request("notifications/initialized", notification=True)
        self.catalog = self.request("tools/list")["tools"]
        self.tools = {t["name"]: t for t in self.catalog}

    def request(self, method, params=None, notification=False, path="/mcp"):
        value = {"jsonrpc": "2.0", "method": method}
        if not notification:
            value["id"] = next(self.ids)
        if params is not None:
            value["params"] = params
        response = self.open_request(value, path=path)
        with response:
            try:
                raw = response.read().decode()
            except http.client.IncompleteRead as error:
                raise ConnectionError("MCP response stream ended before completion") from error
            content_type = response.headers.get("Content-Type", "")
        if not raw:
            return None
        if content_type.lower().startswith("text/event-stream"):
            value = self._event_stream_response(raw, value.get("id"))
        else:
            value = json.loads(raw)
        assert "error" not in value, value
        return value["result"]

    def open_request(self, value, path="/mcp"):
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream", "MCP-Protocol-Version": "2025-06-18"}
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        request = urllib.request.Request(self.url + path, json.dumps(value).encode(), headers)
        response = urllib.request.urlopen(request, timeout=self.request_timeout)
        self.session = response.headers.get("Mcp-Session-Id", self.session)
        return response

    @staticmethod
    def _event_stream_response(raw, request_id):
        for frame in raw.replace("\r\n", "\n").split("\n\n"):
            data = []
            for line in frame.splitlines():
                if not line or line.startswith(":"):
                    continue
                field, separator, value = line.partition(":")
                if separator and value.startswith(" "):
                    value = value[1:]
                if field == "data":
                    data.append(value)
            payload = "\n".join(data).strip()
            if payload:
                message = json.loads(payload)
                if message.get("id") == request_id:
                    return message
        raise AssertionError(f"MCP SSE response did not contain request id {request_id!r}")

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
