# mcp-cli

一个轻量、可脚本化的 Rust 命令行工具，用于发现、检查、搜索和调用 [Model Context Protocol（MCP）](https://modelcontextprotocol.io/) 服务器。

`mcp-cli` 同时支持 stdio 与 Streamable HTTP 传输，面向 Shell、自动化脚本和 AI 编码代理。`call` 输出完整 MCP JSON 结果，便于直接交给 `jq` 或其他程序处理。

## 特性

- **单一 Rust 二进制**：启动快，release 构建无需 JavaScript 运行时。
- **双传输支持**：支持子进程 stdio 与 Streamable HTTP MCP 服务器。
- **Shell 友好**：业务结果写入 stdout，错误和诊断写入 stderr。
- **完整 JSON 输出**：工具 Schema 与调用结果保持机器可读，不丢失扩展字段。
- **按需连接**：`info`、`call` 只连接目标服务器；`list` 与 `grep` 使用有界并发。
- **连接复用**：Linux/macOS 默认使用按服务器隔离的 Unix daemon；Windows 自动使用 direct 模式。
- **工具过滤**：通过 `allowedTools` 和 `disabledTools` 控制工具可见性与调用权限。
- **安全配置**：支持环境变量替换、header/env 脱敏、严格配置校验和稳定配置哈希。
- **可靠执行**：支持总超时预算、指数退避、抖动、瞬态错误重试和资源清理。
- **可恢复错误**：结构化错误包含稳定类型、详情、建议和明确退出码。

## 快速开始

### 1. 构建或安装

需要 Rust stable 工具链。仓库提供 `rust-toolchain.toml`，进入目录后 Cargo 会自动使用对应工具链。

在仓库根目录执行：

```bash
cargo build --release
./target/release/mcp-cli --version
```

安装到 Cargo bin 目录：

```bash
cargo install --path . --locked
mcp-cli --version
```

也可以直接运行源码：

```bash
cargo run --bin mcp-cli -- --help
```

### 2. 创建配置

在当前目录创建 `mcp_servers.json`：

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": [
        "-y",
        "@modelcontextprotocol/server-filesystem",
        "."
      ]
    },
    "deepwiki": {
      "url": "https://mcp.deepwiki.com/mcp"
    }
  }
}
```

stdio 示例需要系统中已安装对应命令；上例需要 Node.js 与 `npx`。

### 3. 发现工具

```bash
# 列出全部服务器和工具；没有 list 子命令
mcp-cli

# 包含工具描述
mcp-cli --with-descriptions

# 搜索工具
mcp-cli grep '*file*'
```

### 4. 检查并调用工具

```bash
# 查看服务器详情
mcp-cli info filesystem

# 查看工具 JSON Schema；两种 target 写法等价
mcp-cli info filesystem read_file
mcp-cli info filesystem/read_file

# 使用内联 JSON 调用
mcp-cli call filesystem read_file '{"path":"./README.md"}'

# 或从非 TTY stdin 读取 JSON
printf '{"path":"./README.md"}' | mcp-cli call filesystem/read_file
```

## 命令行

```text
mcp-cli [OPTIONS]                         列出全部服务器和工具
mcp-cli [OPTIONS] SERVER                  显示服务器详情
mcp-cli [OPTIONS] info SERVER             显示服务器详情
mcp-cli [OPTIONS] info SERVER TOOL        输出工具 JSON Schema
mcp-cli [OPTIONS] info SERVER/TOOL        输出工具 JSON Schema
mcp-cli [OPTIONS] grep PATTERN            按 glob 搜索工具
mcp-cli [OPTIONS] call SERVER TOOL [JSON] 调用工具
mcp-cli [OPTIONS] call SERVER/TOOL [JSON] 调用工具
```

### 全局选项

| 选项 | 说明 |
|------|------|
| `-h, --help` | 显示帮助 |
| `-v, --version` | 显示版本 |
| `-d, --with-descriptions` | 在 list、info、grep 中包含工具描述 |
| `-c, --config <PATH>` | 显式指定 `mcp_servers.json` |

`mcp-cli list` 不是有效命令；直接运行 `mcp-cli` 即可列出服务器和工具。

## 使用示例

### 搜索工具

```bash
# `*` 不跨越 `/`，`**` 可以跨越 `/`，匹配不区分大小写
mcp-cli grep '*file*'
mcp-cli grep '*search*' --with-descriptions
```

无匹配结果仍视为成功：

```text
No matching tools found.
```

### 查看 Schema

```bash
mcp-cli info github/search_repositories
```

stdout 是一个紧凑 JSON Schema，而不是附带标签的展示文本，例如：

```json
{"properties":{"query":{"type":"string"}},"required":["query"],"type":"object"}
```

### 调用工具

内联 JSON 优先，不会再读取 stdin：

```bash
mcp-cli call github search_repositories \
  '{"query":"mcp server","per_page":5}'
```

未提供内联 JSON 时：

- 非 TTY stdin：读取完整 stdin。
- TTY、EOF 或仅空白输入：使用空对象 `{}`。
- 顶层值必须是 JSON object。
- 最大输入为 16 MiB。

```bash
# stdin
printf '%s' '{"query":"mcp"}' \
  | mcp-cli call github/search_repositories

# heredoc，适合复杂参数
mcp-cli call server/tool <<'JSON'
{
  "content": "包含单引号和双引号的长文本",
  "enabled": true
}
JSON
```

### 管道与脚本

`call` 将完整 MCP ToolResult 作为单个 JSON 值输出：

```bash
mcp-cli call github search_repositories '{"query":"mcp"}' \
  | jq '.content'
```

脚本可通过退出码区分失败类型：

```bash
if result=$(mcp-cli call filesystem read_file '{"path":"./config.json"}'); then
  printf '%s\n' "$result" | jq '.content'
else
  status=$?
  printf 'mcp-cli failed with exit code %s\n' "$status" >&2
fi
```

## 配置

### 配置格式

配置兼容常见 MCP 客户端使用的 `mcpServers` 结构：

```json
{
  "mcpServers": {
    "local-server": {
      "command": "node",
      "args": ["./server.js"],
      "env": {
        "API_KEY": "${API_KEY}"
      },
      "cwd": "/path/to/project"
    },
    "remote-server": {
      "url": "https://mcp.example.com/mcp",
      "headers": {
        "Authorization": "Bearer ${MCP_TOKEN}"
      }
    }
  }
}
```

每个服务器必须且只能配置一种传输：

- stdio：`command`，可选 `args`、`env`、`cwd`。
- HTTP：绝对 `http://` 或 `https://` URL，可选 `headers`。

### 配置发现顺序

1. `-c/--config <PATH>`
2. `MCP_CONFIG_PATH`
3. `./mcp_servers.json`
4. `~/.mcp_servers.json`
5. `~/.config/mcp/mcp_servers.json`

显式路径不存在或不可读时会立即失败，不会继续回退默认路径。

### 环境变量替换

配置中的所有字符串值支持 `${VAR_NAME}`：

```json
{
  "headers": {
    "Authorization": "Bearer ${MCP_TOKEN}"
  }
}
```

默认严格模式下，缺失变量会返回 `MISSING_ENV_VAR`。设置以下变量可改为警告并替换为空字符串：

```bash
MCP_STRICT_ENV=false mcp-cli
# 或 MCP_STRICT_ENV=0
```

替换后的 env/header 值会登记为秘密，并在错误、debug、stdio stderr 和 daemon 边界进行脱敏。

### 工具过滤

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
      "allowedTools": ["read_*", "list_*", "search_*"],
      "disabledTools": ["delete_*", "write_*"]
    }
  }
}
```

规则：

- `allowedTools` 为空时默认允许全部工具。
- `allowedTools` 仅保留匹配项，支持大小写不敏感的 `*` 与 `?` 完整匹配。
- `disabledTools` 优先于 `allowedTools`。
- 过滤应用于 list、info、grep 和 call；被禁用工具无法通过直接名称绕过。

### 运行时环境变量

| 变量 | 说明 | 默认值 |
|------|------|--------|
| `MCP_CONFIG_PATH` | 配置文件路径 | 未设置 |
| `MCP_DEBUG` | 非空时启用 debug 诊断 | 关闭 |
| `MCP_TIMEOUT` | 单条命令总超时预算，秒 | `1800` |
| `MCP_CONCURRENCY` | list/grep 最大并发服务器数 | `5` |
| `MCP_MAX_RETRIES` | 首次尝试后的最大重试次数 | `3` |
| `MCP_RETRY_DELAY` | 重试基础延迟，毫秒 | `1000` |
| `MCP_STRICT_ENV` | 缺失 `${VAR}` 时失败 | `true` |
| `MCP_NO_DAEMON` | `1` 时强制 direct 模式 | 未设置 |
| `MCP_DAEMON_TIMEOUT` | daemon 空闲超时，秒 | `60` |
| `NO_COLOR` | 存在时禁用 ANSI 样式 | 未设置 |

`MCP_TIMEOUT`、`MCP_CONCURRENCY`、`MCP_RETRY_DELAY` 和 `MCP_DAEMON_TIMEOUT` 必须是正十进制整数；`MCP_MAX_RETRIES` 还允许 `0`，表示禁用重试。非法值返回 `INVALID_RUNTIME_CONFIG`，不会静默使用默认值。

## 输出与退出码

| 通道 | 内容 |
|------|------|
| stdout | list/info/grep 的业务文本；tool Schema 和 call 的 JSON |
| stderr | 结构化错误、warning、debug 与 stdio server 日志 |

错误格式：

```text
Error [ERROR_KIND]: Message
  Details: Optional details
  Suggestion: Recovery action
```

当 MCP 工具结果包含 `isError=true` 时，`TOOL_EXECUTION_FAILED` 的 stderr 详情会先合并 ToolResult 中的 text 错误并规范化为单行，再执行脱敏和 1024 字符限制，随后复用调用前已取得的 input schema。schema 的字符串键和值会在 JSON 转义和大小判断前脱敏，最终序列化结果还会执行一次安全一致性检查；紧凑结果不超过 8 KiB 时完整显示，更大的 schema 改为显示按名称排序的前 20 个顶层参数类型、required 状态和省略数量。若 schema 字段名因脱敏改变或冲突，或最终串仍需改写，则显示安全的不可内联说明，避免把可能丢字段或无效的结果误称为完整 schema。降级场景会建议运行 `mcp-cli info SERVER TOOL`；CLI 不会为诊断再次请求服务器，也不会回显调用参数。

退出码：

| 退出码 | 含义 |
|-------:|------|
| `0` | 成功；批量 list/grep 可包含单服务器失败 |
| `1` | CLI、参数、配置、JSON、服务器或工具错误 |
| `2` | MCP 工具返回业务执行错误 |
| `3` | 网络错误或超时 |
| `4` | 认证或授权错误 |
| `130` / `143` | Unix SIGINT / SIGTERM |

## 连接模型

### daemon 模式

Linux 和 macOS 默认按服务器延迟启动独立 worker，通过私有 Unix socket 复用 MCP 连接：

1. 首次请求校验运行目录和已有元数据。
2. 没有可复用 worker 时，通过受控启动协议创建 daemon。
3. 后续 CLI 请求通过 NDJSON IPC 调用同一 MCP 连接。
4. 配置哈希变化、死进程或 socket 缺失时重建 worker。
5. 空闲超过 `MCP_DAEMON_TIMEOUT` 后自动关闭并清理文件。
6. daemon 的操作性故障可安全回退 direct；安全校验失败会 fail closed。

```bash
MCP_DEBUG=1 mcp-cli info filesystem
MCP_DAEMON_TIMEOUT=120 mcp-cli
```

### direct 模式

Windows 始终使用 direct；其他平台可显式禁用 daemon：

```bash
MCP_NO_DAEMON=1 mcp-cli info filesystem
```

连接范围：

| 命令 | 连接范围 |
|------|----------|
| `mcp-cli` | 并发连接全部服务器 |
| `mcp-cli grep PATTERN` | 并发连接全部服务器 |
| `mcp-cli info SERVER` | 仅目标服务器 |
| `mcp-cli info SERVER TOOL` | 仅目标服务器 |
| `mcp-cli call SERVER TOOL JSON` | 仅目标服务器 |

### 重试与超时

瞬态错误在同一个总 deadline 内重试，包括指定网络错误和 HTTP `429`、`502`、`503`、`504`。退避采用指数增长、10 秒上限与 ±25% 抖动。

配置、JSON、认证 `401/403`、工具验证和业务错误不会重试。清理操作使用独立的短预算，且不会覆盖原始业务错误。

## AI Agent 使用方式

CLI 模式避免一次性把全部 MCP Schema 注入模型上下文。推荐工作流：

1. 发现：`mcp-cli` 或 `mcp-cli grep '*keyword*'`。
2. 检查：`mcp-cli info SERVER/TOOL`。
3. 执行：`mcp-cli call SERVER/TOOL '{...}'`。
4. 使用 JSON parser 读取 call 结果，不依赖人类展示文本。

可加入 Agent 指令：

````markdown
## MCP CLI

使用 `mcp-cli` 按需发现和调用 MCP 工具：

```bash
mcp-cli                              # 列出服务器和工具
mcp-cli grep '*pattern*'             # 搜索工具
mcp-cli info server/tool             # 获取 JSON Schema
mcp-cli call server/tool '{"k":"v"}' # 调用工具
```

调用前先读取 Schema。不要使用不存在的 `mcp-cli list` 子命令。
````

## 开发

### 工具链与依赖

- Rust stable，edition 2024。
- `rmcp = 2.2.0`，启用 client、stdio child process、Streamable HTTP reqwest transport。
- Tokio、clap、serde、reqwest 等依赖均在 `Cargo.toml` 和 `Cargo.lock` 中精确锁定。
- `test-fixtures` feature 用于构建 mock stdio server 和进程级 fixture。

### 常用命令

```bash
# 开发构建
cargo build

# release 构建
cargo build --release --all-features

# 格式检查
cargo fmt --check

# 严格静态检查
cargo clippy --all-targets --all-features -- -D warnings

# 完整测试
cargo test --all-features
```

完整测试覆盖 CLI、配置、过滤、输出、stdio/HTTP、重试、daemon、信号、跨平台条件分支和 37 组 correctness properties。

### 真实 MCP 复测

真实 Streamable HTTP 与 `npx -y 12306-mcp` stdio 的测试流程、预期输出、错误注入和清理步骤见：

- [`doc/test-report.md`](doc/test-report.md)

Spec 与设计文档：

- [`doc/requirements.md`](doc/requirements.md)
- [`doc/design.md`](doc/design.md)
- [`doc/tasks.md`](doc/tasks.md)
- [`.kiro/specs/mcp-cli/`](.kiro/specs/mcp-cli/)

## 项目结构

```text
src/
├── cli.rs                 # CLI 语法与兼容性诊断
├── commands/              # list、info、grep、call
├── config/                # 发现、替换、校验与 canonical hash
├── connection/            # direct manager 与 rmcp adapter
├── daemon/                # Unix IPC、metadata、worker 与安全路径
├── policy/                # 过滤、搜索、重试与脱敏
├── domain.rs              # 共享领域模型
├── error.rs               # 错误分类与退出码
├── output.rs              # stdout/stderr 展示
├── runtime.rs             # deadline、取消与运行时配置
└── main.rs                # 进程边界与命令接线

tests/
├── integration/           # transport 与 daemon 集成测试
├── process/               # 真实 CLI 子进程测试
├── properties/            # 37 组 property-based tests
├── support/               # 测试 fixture 与 fake 实现
└── unit/                  # 跨模块单元测试
```

## 贡献

提交变更前请至少运行：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

行为规范以 [`.kiro/specs/mcp-cli/requirements.md`](.kiro/specs/mcp-cli/requirements.md) 为准。若修改公开行为，请同步更新测试和 `doc/test-report.md`。
