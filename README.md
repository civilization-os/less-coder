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
- Rust toolchain (currently required for `lesscoder server`)

## Install

After package publish:

```bash
pip install lesscoder
npm i -g @lesscoder/cli
```

From source (development):

```bash
pip install -e .
```

The npm package is a CLI wrapper that invokes the Python runtime.

## Run

```bash
lesscoder warmup
lesscoder server --host 127.0.0.1 --port 8787
lesscoder run --project-root fixtures/java-sample
lesscoder trace --trace-id <trace_id>
```

`lesscoder server` performs internal warmup automatically before serving.

## MCP Setup

Start the service first:

```bash
lesscoder server --host 127.0.0.1 --port 8787
```

Then configure your MCP client to launch `lesscoder` as a local server process.
Example config (common `mcpServers` format):

```json
{
  "mcpServers": {
    "lesscoder": {
      "command": "lesscoder",
      "args": ["server", "--host", "127.0.0.1", "--port", "8787"],
      "env": {
        "LESSCODER_HOME": "/absolute/path/to/less-coder"
      }
    }
  }
}
```

Notes:

- Use `LESSCODER_HOME` when the client may start from an arbitrary working directory.
- If port `8787` is already occupied, change it in both server args and client settings.
- Current adapter endpoint is `127.0.0.1:<port>` with local protocol v0.

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
