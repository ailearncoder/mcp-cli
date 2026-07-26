# Requirements: mcp-cli (Rust)

## 项目概述

将 `reference/mcp-cli`（TypeScript/Bun 实现的轻量级 MCP CLI 工具）完整移植为 Rust 实现，保持功能等价。该工具用于与 MCP (Model Context Protocol) 服务器交互，为 AI 编码代理提供工具发现和调用能力。

## 功能需求

### FR-1: CLI 命令

| 命令 | 说明 | 输出 |
|------|------|------|
| `mcp-cli` | 列出所有服务器及其工具 | 人类可读文本 → stdout |
| `mcp-cli info <server>` | 显示服务器详情（工具列表、参数） | 人类可读文本 → stdout |
| `mcp-cli info <server> <tool>` | 显示工具 schema | 人类可读文本 → stdout |
| `mcp-cli grep <pattern>` | 按 glob 模式搜索工具 | 人类可读文本 → stdout |
| `mcp-cli call <server> <tool>` | 调用工具（从 stdin 读取 JSON） | 原始 JSON → stdout |
| `mcp-cli call <server> <tool> <json>` | 调用工具（内联 JSON 参数） | 原始 JSON → stdout |

**格式支持**：
- 空格分隔：`mcp-cli info server tool`
- 斜杠分隔：`mcp-cli info server/tool`

**选项**：
- `-h, --help` — 显示帮助信息
- `-v, --version` — 显示版本号
- `-d, --with-descriptions` — 包含工具描述
- `-c, --config <path>` — 指定配置文件路径

### FR-2: 配置文件

**格式**：`mcp_servers.json`，兼容 Claude Desktop / VS Code / Gemini 配置。

```json
{
  "mcpServers": {
    "local-server": {
      "command": "node",
      "args": ["./server.js"],
      "env": { "API_KEY": "${API_KEY}" },
      "cwd": "/path/to/directory"
    },
    "remote-server": {
      "url": "https://mcp.example.com",
      "headers": { "Authorization": "Bearer ${TOKEN}" }
    }
  }
}
```

**配置文件搜索顺序**：
1. `MCP_CONFIG_PATH` 环境变量 或 `-c/--config` 参数
2. `./mcp_servers.json`（当前目录）
3. `~/.mcp_servers.json`
4. `~/.config/mcp/mcp_servers.json`

**环境变量替换**：
- 支持 `${VAR_NAME}` 语法
- 严格模式（默认）：缺失变量报错
- 非严格模式（`MCP_STRICT_ENV=false`）：缺失变量用空字符串替代并警告

**服务器验证**：
- 必须有 `command`（stdio）或 `url`（HTTP），不能同时有两者
- 空配置或 null 值报错

### FR-3: 工具过滤

每个服务器可配置 `allowedTools` 和 `disabledTools`：
- `allowedTools`：仅允许匹配的工具（支持 glob：`*`, `?`）
- `disabledTools`：排除匹配的工具
- `disabledTools` 优先于 `allowedTools`
- 过滤应用于所有操作（info, grep, call）

### FR-4: MCP 传输协议

- **stdio**：通过子进程的 stdin/stdout 通信
- **HTTP (Streamable HTTP)**：通过 HTTP 连接远程服务器

### FR-5: 连接池（Daemon 模式）

- 每个 MCP server 一个独立 daemon 进程
- 通过 Unix socket 进行 IPC
- 空闲超时自动关闭（默认 60 秒）
- 配置变更自动检测（config hash）并重建连接
- 孤儿 daemon 自动清理
- 可通过 `MCP_NO_DAEMON=1` 禁用

### FR-6: 重试机制

- 指数退避 + 抖动（jitter）
- 瞬态错误自动重试：`ECONNREFUSED`, `ETIMEDOUT`, `ECONNRESET`, HTTP 502/503/504/429
- 非瞬态错误立即失败：配置错误、401/403、验证错误
- 尊重总超时预算（`MCP_TIMEOUT`）
- 默认最多 3 次重试，基础延迟 1000ms

### FR-7: 错误处理

结构化错误信息，包含：
- 错误类型（`[ERROR_TYPE]`）
- 错误消息
- 详情（Details）
- 恢复建议（Suggestion）

错误始终输出到 stderr。

### FR-8: 并发控制

- `list` 和 `grep` 命令并行连接所有服务器
- 默认并发数 5（`MCP_CONCURRENCY`）
- `call` 命令仅连接目标服务器

## 非功能需求

### NFR-1: 性能
- 单二进制文件，启动快
- Daemon 模式避免重复启动 MCP 服务器的延迟

### NFR-2: 可靠性
- 连接失败不影响其他服务器
- 优雅关闭（SIGINT/SIGTERM）

### NFR-3: 可用性
- 结构化错误信息便于人类和 AI 代理理解
- 终端颜色输出（尊重 `NO_COLOR` 环境变量和 TTY 检测）

## 技术决策

| 项目 | 选择 | 理由 |
|------|------|------|
| 语言 | Rust | 高性能、单二进制、内存安全 |
| 异步运行时 | tokio | rmcp SDK 依赖，生态成熟 |
| CLI 解析 | clap (derive macros) | 开发效率高，自动生成帮助 |
| MCP SDK | rmcp 2.x stable | 官方实现，3.4M+月下载 |
| 二进制名称 | `mcp-cli` | 与参考实现一致 |

### rmcp features

```toml
rmcp = { version = "2.2", features = [
    "client",
    "transport-child-process",
    "transport-streamable-http-client-reqwest",
] }
```

## 环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `MCP_CONFIG_PATH` | 配置文件路径 | (无) |
| `MCP_DEBUG` | 启用调试输出 | `false` |
| `MCP_TIMEOUT` | 请求超时（秒） | `1800` (30分钟) |
| `MCP_CONCURRENCY` | 并行处理的服务器数 | `5` |
| `MCP_MAX_RETRIES` | 最大重试次数（0=禁用） | `3` |
| `MCP_RETRY_DELAY` | 基础重试延迟（毫秒） | `1000` |
| `MCP_STRICT_ENV` | 缺失 `${VAR}` 时报错 | `true` |
| `MCP_NO_DAEMON` | 禁用 daemon 连接池 | `false` |
| `MCP_DAEMON_TIMEOUT` | Daemon 空闲超时（秒） | `60` |

## 平台支持

| 平台 | Daemon 模式 | Direct 模式 |
|------|-------------|-------------|
| Linux | ✅ | ✅ |
| macOS | ✅ | ✅ |
| Windows | ❌（无 Unix socket） | ✅ |

## 测试策略

- **对照参考实现**：将 TypeScript 测试用例逐一移植为 Rust 测试
- **单元测试**：config 加载/验证、工具过滤、glob 匹配、输出格式化、错误消息生成、瞬态错误检测
- **集成测试**：CLI 错误处理（22 个 edge case）、端到端命令执行（需要真实或 mock MCP server）

## 参考实现

- 源码路径：`reference/mcp-cli/`
- 版本：0.3.0
- 许可证：MIT
