# lesscoder

Code-Native Engine for local AI-assisted coding workflows.

This repository provides an end-to-end loop:

`Analyze -> Plan -> Execute -> Verify -> Done`

## Core Modules

- `ALSP` (Rust): repository map, symbol lookup, LSP fallback
- `ALSP_ADAPTER` (Rust): local protocol service over TCP
- `Patchlet` (Rust): atomic Search/Replace patch apply with backup and rollback
- `Orchestrator` (Python): LangGraph-based pipeline and repair routing
- `CLI` (`lesscoder`): run tasks, query trace, start server

## Requirements

- Python `3.11+`
- Java `17+`
- Maven `3.9+`
- Rust toolchain (development/source mode only)

## Install

After package publish:

```bash
pip install lesscoder
npm i -g @civilization/lesscoder
```

From source (development):

```bash
pip install -e .
```

The npm package is a CLI wrapper that invokes the Python runtime.

Windows x64 install path now uses hybrid adapter resolution:
- prefer prebuilt adapter from architecture package:
  - PyPI: `lesscoder-adapter-win-x64`
  - npm: `@civilization/lesscoder-adapter-win32-x64` (optional dependency)
- fallback to cache and GitHub Release download when architecture package is unavailable

`lesscoder server` runtime mode:
- Dev mode: if local Rust manifest exists, use `cargo run`.
- Installed mode: if no manifest, resolve adapter binary from:
  - `LESSCODER_ADAPTER_BIN`
  - installed architecture package binary (Windows x64)
  - bundled binary (if packaged)
  - local cache `~/.lesscoder/adapter/...`
  - GitHub Release auto-download (default repo: `civilization-os/less-coder`)
  - checksum verify via `lesscoder_adapter_manifest.json` when available

## Run

```bash
lesscoder warmup --project-root /abs/path/to/your/repo
lesscoder server --host 127.0.0.1 --port 8787
lesscoder run --project-root /abs/path/to/your/repo
lesscoder trace --trace-id <trace_id>
lesscoder release-dry-run --project-root /abs/path/to/your/repo --tag v0.1.0
```

`warmup` requires explicit path parameters (`--project-root` or `--manifest-path`).
`server` can start without project path, then accept `system.warmup(project_root=...)` later.

## MCP Setup

Start the service first:

```bash
lesscoder server --host 127.0.0.1 --port 8787
```

When server starts, it prints:
- a ready-to-copy MCP config JSON snippet
- HTTP inspect endpoints:
  - `http://127.0.0.1:8787/health`
  - `http://127.0.0.1:8787/methods`

Then configure your MCP client to launch `lesscoder` as a local server process.
Example config (common `mcpServers` format):

```json
{
  "mcpServers": {
    "lesscoder": {
      "command": "lesscoder",
      "args": ["mcp", "--adapter-host", "127.0.0.1", "--adapter-port", "8787"]
    }
  }
}
```

Notes:

- `lesscoder mcp` is stdio MCP mode (for OpenCode / IDE MCP clients).
- project activation is explicit via `system.warmup` payload (`project_root`/`path`).
- If port `8787` is already occupied, change it in both server args and client settings.
- Current adapter endpoint is `127.0.0.1:<port>` with local protocol v0.
- Browser inspection endpoints are best-effort diagnostics; MCP calls still use local protocol v0.
- You can override adapter binary path with `LESSCODER_ADAPTER_BIN`.

## Quick Validation

```bash
pytest -q tests/integration
```

## Language Status

- Java: available now
- Go / JavaScript / TypeScript / C / C++: planned next

## Documentation

- Project guide: `PROJECT_GUIDE.md`
- Docs index: `docs/README.md`
- Java runtime guide: `docs/Java_Runtime_Guide.md`
- Local protocol: `docs/local_protocol_v0.md`
- Worklog index: `WORKLOG/README.md`
