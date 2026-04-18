import json
import socket
import time
import uuid
from pathlib import Path

from tests.integration.e2e_adapter import start_adapter_process


def _pick_free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def _wait_for_port(host: str, port: int, timeout_s: float = 20.0) -> None:
    end = time.time() + timeout_s
    while time.time() < end:
        try:
            with socket.create_connection((host, port), timeout=0.5):
                return
        except OSError:
            time.sleep(0.2)
    raise TimeoutError(f"adapter server not ready on {host}:{port}")


def _call_adapter(host: str, port: int, action: str, payload: dict, trace_id: str, session_id: str | None):
    envelope = {
        "version": "v0",
        "request_id": f"req_{uuid.uuid4().hex[:12]}",
        "trace_id": trace_id,
        "session_id": session_id,
        "source": "pytest",
        "target": "alsp_adapter",
        "action": action,
        "payload": payload,
        "meta": {"timeout_ms": 30000},
    }
    with socket.create_connection((host, port), timeout=5.0) as s:
        s.sendall((json.dumps(envelope, ensure_ascii=False) + "\n").encode("utf-8"))
        raw = b""
        while not raw.endswith(b"\n"):
            part = s.recv(4096)
            if not part:
                break
            raw += part
    return json.loads(raw.decode("utf-8"))


def test_stateless_calls_work_with_project_root_in_each_request():
    root = Path(__file__).resolve().parents[2]
    project_root = root / "fixtures" / "java-sample"
    port = _pick_free_port()
    proc = start_adapter_process(root, "127.0.0.1", port)
    try:
        _wait_for_port("127.0.0.1", port, timeout_s=30.0)
        trace_id = "tr_stateless_001"

        warmup = _call_adapter(
            "127.0.0.1",
            port,
            "system.warmup",
            {"project_root": str(project_root)},
            trace_id=trace_id,
            session_id="sess_a",
        )
        assert warmup["status"] == "ok"

        lookup = _call_adapter(
            "127.0.0.1",
            port,
            "symbol.lookup",
            {"symbol": "normalizeName", "project_root": str(project_root)},
            trace_id=trace_id,
            session_id="sess_b",
        )
        assert lookup["status"] == "ok"
        assert lookup["data"]["symbol"] == "normalizeName"
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except Exception:
                proc.kill()
