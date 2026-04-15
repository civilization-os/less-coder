# lesscoder

面向本地 AI 自主编程流程的 Code-Native Engine。

当前仓库已经打通端到端闭环：

`Analyze -> Plan -> Execute -> Verify -> Done`

## 核心模块

- `ALSP`（Rust）：仓库骨架、符号定位、LSP 降级
- `ALSP_ADAPTER`（Rust）：本地协议服务层（TCP）
- `Patchlet`（Rust）：原子 Search/Replace 补丁、备份与回滚
- `Orchestrator`（Python）：基于 LangGraph 的状态编排与 Repair 路由
- `CLI`（`lesscoder`）：任务执行、trace 查询、服务启动

## 环境要求

- Python `3.11+`
- Java `17+`
- Maven `3.9+`
- Rust toolchain（当前 `lesscoder server` 仍依赖）

## 安装

发布后可直接：

```bash
pip install lesscoder
npm i -g @lesscoder/cli
```

源码开发安装：

```bash
pip install -e .
```

说明：npm 包是 CLI 包装层，实际仍调用 Python 运行时。

## 运行

```bash
lesscoder warmup
lesscoder server --host 127.0.0.1 --port 8787
lesscoder run --project-root fixtures/java-sample
lesscoder trace --trace-id <trace_id>
```

说明：`lesscoder server` 启动前会自动执行内部 warmup。

## MCP 配置

先启动服务：

```bash
lesscoder server --host 127.0.0.1 --port 8787
```

然后在 MCP 客户端中配置 `lesscoder` 进程。示例（常见 `mcpServers` 格式）：

```json
{
  "mcpServers": {
    "lesscoder": {
      "command": "lesscoder",
      "args": ["server", "--host", "127.0.0.1", "--port", "8787"],
      "env": {
        "LESSCODER_HOME": "C:/absolute/path/to/less-coder"
      }
    }
  }
}
```

说明：

- MCP 客户端启动目录不固定时，建议设置 `LESSCODER_HOME`。
- 若 `8787` 端口被占用，需同步修改 server 参数与客户端配置。
- 当前 adapter 监听 `127.0.0.1:<port>`，协议为本地 protocol v0。

## 快速验证

```bash
pytest -q tests/integration
```

## 语言支持状态

- Java：当前可用
- Go / JavaScript / TypeScript / C / C++：下一阶段扩展

## 文档导航

- 项目总指南：`PROJECT_GUIDE.md`
- 文档总览：`docs/README.md`
- Java 运行手册：`docs/Java_Runtime_Guide.md`
- 协议文档：`docs/local_protocol_v0.md`
- 阶段记录：`WORKLOG/README.md`
