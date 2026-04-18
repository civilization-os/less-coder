from pathlib import Path


def get_adapter_binary_path() -> str | None:
    candidate = Path(__file__).resolve().parent / "bin" / "alsp_adapter.exe"
    if candidate.exists():
        return str(candidate)
    return None
