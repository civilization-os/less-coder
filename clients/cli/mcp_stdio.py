import json
import socket
import sys
import uuid
from dataclasses import dataclass
from importlib import metadata as importlib_metadata
from typing import Any


@dataclass
class BridgeConfig:
    adapter_host: str = "127.0.0.1"
    adapter_port: int = 8787
    timeout_ms: int = 30_000


def run_stdio_bridge(config: BridgeConfig) -> int:
    while True:
        req, mode = _read_mcp_message(sys.stdin.buffer)
        if req is None:
            return 0
        resp = handle_mcp_request(req, config)
        if resp is not None:
            _write_mcp_message(sys.stdout.buffer, resp, mode=mode)
            sys.stdout.buffer.flush()


def handle_mcp_request(req: dict[str, Any], config: BridgeConfig) -> dict[str, Any] | None:
    method = req.get("method")
    if not isinstance(method, str):
        return _mcp_error(req.get("id"), -32600, "invalid request: missing method")

    if method == "initialize":
        return {
            "jsonrpc": "2.0",
            "id": req.get("id"),
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "lesscoder", "version": _version()},
            },
        }
    if method == "notifications/initialized":
        return None
    if method == "ping":
        return {"jsonrpc": "2.0", "id": req.get("id"), "result": {}}
    if method == "tools/list":
        return {"jsonrpc": "2.0", "id": req.get("id"), "result": {"tools": _tool_specs()}}
    if method == "tools/call":
        params = req.get("params") or {}
        name = params.get("name")
        arguments = params.get("arguments") or {}
        if not isinstance(name, str):
            return _mcp_error(req.get("id"), -32602, "invalid params: missing tool name")
        if not isinstance(arguments, dict):
            return _mcp_error(req.get("id"), -32602, "invalid params: arguments must be object")
        action = _tool_to_action(name)
        if action is None:
            return _mcp_error(req.get("id"), -32602, f"unknown tool: {name}")
        adapter_resp = _call_adapter(config, action, arguments)
        return {
            "jsonrpc": "2.0",
            "id": req.get("id"),
            "result": {
                "content": [{"type": "text", "text": json.dumps(adapter_resp, ensure_ascii=False)}],
                "isError": adapter_resp.get("status") == "error",
            },
        }
    return _mcp_error(req.get("id"), -32601, f"method not found: {method}")


def _version() -> str:
    try:
        return importlib_metadata.version("lesscoder")
    except importlib_metadata.PackageNotFoundError:
        return "0.1.0"


def _tool_specs() -> list[dict[str, Any]]:
    return [
        _tool("system_health", "Call adapter action system.health"),
        _tool("system_warmup", "Call adapter action system.warmup", required=["project_root"]),
        _tool("repo_map", "Call adapter action repo.map"),
        _tool("symbol_lookup", "Call adapter action symbol.lookup", required=["symbol"]),
        _tool("symbol_resolve", "Call adapter action symbol.resolve", required=["symbol"]),
        _tool("graph_calls", "Call adapter action graph.calls", required=["symbol"]),
        _tool("patch_apply", "Call adapter action patch.apply", required=["target", "search_replace_blocks"]),
    ]


def _tool(name: str, description: str, required: list[str] | None = None) -> dict[str, Any]:
    return {
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": {},
            "required": required or [],
            "additionalProperties": True,
        },
    }


def _tool_to_action(name: str) -> str | None:
    return {
        "system_health": "system.health",
        "system_warmup": "system.warmup",
        "repo_map": "repo.map",
        "symbol_lookup": "symbol.lookup",
        "symbol_resolve": "symbol.resolve",
        "graph_calls": "graph.calls",
        "patch_apply": "patch.apply",
    }.get(name)


def _call_adapter(config: BridgeConfig, action: str, payload: dict[str, Any]) -> dict[str, Any]:
    envelope = {
        "version": "v0",
        "request_id": f"req_{uuid.uuid4().hex[:12]}",
        "trace_id": f"tr_mcp_{uuid.uuid4().hex[:10]}",
        "session_id": None,
        "source": "mcp_stdio_bridge",
        "target": "alsp_adapter",
        "action": action,
        "payload": payload,
        "meta": {"timeout_ms": config.timeout_ms},
    }
    line = json.dumps(envelope, ensure_ascii=False) + "\n"
    try:
        with socket.create_connection((config.adapter_host, config.adapter_port), timeout=config.timeout_ms / 1000.0) as s:
            s.sendall(line.encode("utf-8"))
            raw = _recv_line(s, config.timeout_ms)
            if not raw:
                return {"status": "error", "error": {"code": "COMMON_TIMEOUT", "message": "empty response from adapter"}}
            return json.loads(raw.decode("utf-8"))
    except OSError as exc:
        return {"status": "error", "error": {"code": "COMMON_TIMEOUT", "message": f"adapter connection failed: {exc}"}}
    except json.JSONDecodeError as exc:
        return {"status": "error", "error": {"code": "COMMON_BAD_REQUEST", "message": f"invalid adapter response: {exc}"}}


def _recv_line(sock: socket.socket, timeout_ms: int) -> bytes:
    sock.settimeout(timeout_ms / 1000.0)
    data = bytearray()
    while True:
        chunk = sock.recv(1)
        if not chunk:
            break
        data.extend(chunk)
        if chunk == b"\n":
            break
    return bytes(data)


def _mcp_error(req_id: Any, code: int, message: str) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}


def _read_mcp_message(stream) -> tuple[dict[str, Any] | None, str]:
    content_length = None
    first_line = stream.readline()
    if first_line == b"":
        return None, "framed"

    # Compatibility mode: some clients may send newline-delimited JSON-RPC.
    stripped = first_line.strip()
    if stripped.startswith(b"{") and stripped.endswith(b"}"):
        try:
            return json.loads(stripped.decode("utf-8")), "ndjson"
        except json.JSONDecodeError:
            return None, "ndjson"

    line = first_line
    while True:
        if line == b"":
            return None, "framed"
        if line in (b"\r\n", b"\n"):
            break
        lower = line.lower()
        if lower.startswith(b"content-length:"):
            try:
                content_length = int(line.split(b":", 1)[1].strip())
            except ValueError:
                return None, "framed"
        line = stream.readline()
    if content_length is None or content_length <= 0:
        return None, "framed"
    body = stream.read(content_length)
    if len(body) != content_length:
        return None, "framed"
    try:
        return json.loads(body.decode("utf-8")), "framed"
    except json.JSONDecodeError:
        return None, "framed"


def _write_mcp_message(stream, payload: dict[str, Any], mode: str = "framed") -> None:
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    if mode == "ndjson":
        stream.write(body + b"\n")
        return
    header = f"Content-Length: {len(body)}\r\n\r\n".encode("ascii")
    stream.write(header)
    stream.write(body)
