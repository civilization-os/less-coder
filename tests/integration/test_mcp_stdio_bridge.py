import json
from io import BytesIO

from clients.cli.mcp_stdio import BridgeConfig, _read_mcp_message, _write_mcp_message, handle_mcp_request


def test_initialize_returns_capabilities():
    req = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}
    resp = handle_mcp_request(req, BridgeConfig())
    assert resp is not None
    assert resp["id"] == 1
    assert "result" in resp
    assert "capabilities" in resp["result"]


def test_tools_list_contains_system_health():
    req = {"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}
    resp = handle_mcp_request(req, BridgeConfig())
    assert resp is not None
    tools = resp["result"]["tools"]
    names = [t["name"] for t in tools]
    assert "system_health" in names
    assert "project_activate" in names
    assert "system_warmup" in names
    activate = next(t for t in tools if t["name"] == "project_activate")
    assert "project_root" in activate["inputSchema"]["properties"]
    warmup = next(t for t in tools if t["name"] == "system_warmup")
    assert "project_root" in warmup["inputSchema"]["properties"]
    symbol_lookup = next(t for t in tools if t["name"] == "symbol_lookup")
    assert "symbol" in symbol_lookup["inputSchema"]["properties"]
    symbol_lookup_fuzzy = next(t for t in tools if t["name"] == "symbol_lookup_fuzzy")
    assert "symbol" in symbol_lookup_fuzzy["inputSchema"]["properties"]
    assert "limit" in symbol_lookup_fuzzy["inputSchema"]["properties"]
    graph_calls = next(t for t in tools if t["name"] == "graph_calls")
    assert "language" in graph_calls["inputSchema"]["properties"]


def test_tools_call_unknown_returns_error():
    req = {
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {"name": "nope", "arguments": {}},
    }
    resp = handle_mcp_request(req, BridgeConfig())
    assert resp is not None
    assert "error" in resp
    assert resp["error"]["code"] == -32602


def test_notification_initialized_no_response():
    req = {"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}
    resp = handle_mcp_request(req, BridgeConfig())
    assert resp is None


def test_tools_call_wraps_adapter_response(monkeypatch):
    def fake_call(_cfg, action, payload):
        return {"status": "ok", "action": action, "payload": payload}

    from clients.cli import mcp_stdio

    monkeypatch.setattr(mcp_stdio, "_call_adapter", fake_call)
    req = {
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {"name": "system_health", "arguments": {}},
    }
    resp = handle_mcp_request(req, BridgeConfig())
    assert resp is not None
    content = resp["result"]["content"][0]["text"]
    parsed = json.loads(content)
    assert parsed["status"] == "ok"
    assert parsed["action"] == "system.health"


def test_tools_call_symbol_lookup_fuzzy_maps_to_adapter_action(monkeypatch):
    def fake_call(_cfg, action, payload):
        return {"status": "ok", "action": action, "payload": payload}

    from clients.cli import mcp_stdio

    monkeypatch.setattr(mcp_stdio, "_call_adapter", fake_call)
    req = {
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {"name": "symbol_lookup_fuzzy", "arguments": {"symbol": "Name", "limit": 10}},
    }
    resp = handle_mcp_request(req, BridgeConfig())
    assert resp is not None
    content = resp["result"]["content"][0]["text"]
    parsed = json.loads(content)
    assert parsed["status"] == "ok"
    assert parsed["action"] == "symbol.lookup.fuzzy"


def test_tools_call_project_activate_maps_to_adapter_action(monkeypatch):
    def fake_call(_cfg, action, payload):
        return {"status": "ok", "action": action, "payload": payload}

    from clients.cli import mcp_stdio

    monkeypatch.setattr(mcp_stdio, "_call_adapter", fake_call)
    req = {
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {"name": "project_activate", "arguments": {"project_root": "C:/work/project"}},
    }
    resp = handle_mcp_request(req, BridgeConfig())
    assert resp is not None
    content = resp["result"]["content"][0]["text"]
    parsed = json.loads(content)
    assert parsed["status"] == "ok"
    assert parsed["action"] == "project.activate"


def test_read_mcp_message_supports_ndjson():
    stream = BytesIO(b'{"jsonrpc":"2.0","id":1,"method":"ping"}\n')
    req, mode = _read_mcp_message(stream)
    assert mode == "ndjson"
    assert req is not None
    assert req["method"] == "ping"


def test_write_mcp_message_ndjson_mode():
    out = BytesIO()
    _write_mcp_message(out, {"jsonrpc": "2.0", "id": 1, "result": {}}, mode="ndjson")
    payload = out.getvalue().decode("utf-8").strip()
    assert payload.startswith("{")
    parsed = json.loads(payload)
    assert parsed["id"] == 1
