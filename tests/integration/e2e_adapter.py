import os
import subprocess
import threading
from pathlib import Path

_BUILD_LOCK = threading.Lock()
_BUILT = False


def _adapter_binary_path(repo_root: Path) -> Path:
    exe = "alsp_adapter.exe" if os.name == "nt" else "alsp_adapter"
    return repo_root / "engine" / "rust" / "alsp_adapter" / "target" / "debug" / exe


def _ensure_adapter_built(repo_root: Path) -> None:
    global _BUILT
    if _adapter_binary_path(repo_root).exists():
        _BUILT = True
        return
    if _BUILT:
        return
    with _BUILD_LOCK:
        if _BUILT:
            return
        manifest = repo_root / "engine" / "rust" / "alsp_adapter" / "Cargo.toml"
        subprocess.run(
            ["cargo", "build", "--manifest-path", str(manifest), "--bin", "alsp_adapter", "--quiet"],
            cwd=str(repo_root),
            check=True,
        )
        _BUILT = True


def start_adapter_process(repo_root: Path, host: str, port: int) -> subprocess.Popen:
    _ensure_adapter_built(repo_root)
    adapter_bin = _adapter_binary_path(repo_root)
    if not adapter_bin.exists():
        raise FileNotFoundError(f"adapter binary not found: {adapter_bin}")
    env = os.environ.copy()
    env["ALSP_ADAPTER_ADDR"] = f"{host}:{port}"
    return subprocess.Popen([str(adapter_bin)], cwd=str(repo_root), env=env)
