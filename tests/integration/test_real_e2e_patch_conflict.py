import asyncio
import os
import shutil
import socket
import subprocess
import tempfile
import time
from pathlib import Path

from orchestrator.langgraph_orchestrator import run_real_chain
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


def test_real_e2e_patch_search_miss_enters_repair():
    async def run_case():
        root = Path(__file__).resolve().parents[2]
        fixture_src = root / "fixtures" / "java-sample"
        fixture_root = Path(tempfile.mkdtemp(prefix="java_fixture_conflict_miss_"))
        shutil.copytree(fixture_src, fixture_root, dirs_exist_ok=True)
        target_file = fixture_root / "src" / "main" / "java" / "com" / "acme" / "NameService.java"

        port = _pick_free_port()
        proc = start_adapter_process(root, "127.0.0.1", port)
        try:
            _wait_for_port("127.0.0.1", port, timeout_s=30.0)
            mvn_cmd = "mvn.cmd" if os.name == "nt" else "mvn"
            result = await run_real_chain(
                project_root=str(fixture_root),
                trace_id="tr_l1_p0_10_miss_001",
                adapter_host="127.0.0.1",
                adapter_port=port,
                patch_target=str(target_file),
                search="__THIS_PATTERN_SHOULD_NOT_EXIST__",
                replace="anything",
                verify_command=mvn_cmd,
                verify_args=["test"],
            )
            assert result["status"] == "error"
            assert result["error_code"] == "PATCHLET_SEARCH_MISS"
            assert result["states"] == ["Analyze", "Plan", "Execute", "Repair"]
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
            if fixture_root.exists():
                shutil.rmtree(fixture_root, ignore_errors=True)

    asyncio.run(run_case())


def test_real_e2e_patch_conflict_enters_repair():
    async def run_case():
        root = Path(__file__).resolve().parents[2]
        fixture_src = root / "fixtures" / "java-sample"
        fixture_root = Path(tempfile.mkdtemp(prefix="java_fixture_conflict_force_"))
        shutil.copytree(fixture_src, fixture_root, dirs_exist_ok=True)
        target_file = fixture_root / "src" / "main" / "java" / "com" / "acme" / "NameService.java"

        port = _pick_free_port()
        proc = start_adapter_process(root, "127.0.0.1", port)
        try:
            _wait_for_port("127.0.0.1", port, timeout_s=30.0)
            mvn_cmd = "mvn.cmd" if os.name == "nt" else "mvn"
            result = await run_real_chain(
                project_root=str(fixture_root),
                trace_id="tr_l1_p0_10_conflict_001",
                adapter_host="127.0.0.1",
                adapter_port=port,
                patch_target=str(target_file),
                verify_command=mvn_cmd,
                verify_args=["test"],
                force_patch_conflict=True,
            )
            assert result["status"] == "error"
            assert result["error_code"] == "PATCHLET_CONFLICT"
            assert result["states"] == ["Analyze", "Plan", "Execute", "Repair"]
        finally:
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
            if fixture_root.exists():
                shutil.rmtree(fixture_root, ignore_errors=True)

    asyncio.run(run_case())
