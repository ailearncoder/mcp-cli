# AGENTS.md

本文件适用于仓库根目录及其全部子目录，供自动化编码代理在修改 `mcp-cli` 时遵循。

## 项目概述

`mcp-cli` 是一个 Rust 2024 edition 的 MCP 命令行客户端，目标是以单一二进制提供：

- stdio 与 Streamable HTTP MCP 传输。
- `list`（无子命令）、`info`、`grep`、`call` 工作流。
- Linux/macOS Unix daemon 连接复用。
- Windows 和 `MCP_NO_DAEMON=1` direct 模式。
- 稳定 stdout/stderr、错误类型、退出码和 JSON 管道语义。
- 配置发现、环境替换、工具过滤、重试、超时、脱敏和资源清理。

当前 package 与 public binary 均名为 `mcp-cli`。依赖版本精确锁定在 `Cargo.toml` 和 `Cargo.lock`。

## 事实来源与优先级

发生冲突时按以下顺序判断：

1. [`.kiro/specs/mcp-cli/requirements.md`](.kiro/specs/mcp-cli/requirements.md)：公开行为的权威规范。
2. [`.kiro/specs/mcp-cli/design.md`](.kiro/specs/mcp-cli/design.md)：架构、安全和组件边界。
3. 现有自动化测试：可执行行为契约和回归基线。
4. [`README.md`](README.md) 与 [`doc/`](doc/)：用户指南、设计摘要和复测流程。
5. `reference/mcp-cli/`：参考实现，仅用于兼容性研究，不覆盖当前 Rust Spec。

`reference/` 已被 Git 忽略。除非用户明确要求，否则不要修改、格式化、移动或提交其中内容，也不要把其 Bun/TypeScript 专属行为直接复制到 Rust 实现。

## 仓库结构

```text
src/
├── cli.rs                 # 公开语法、target 解析和兼容性诊断
├── commands/              # list、info、grep、call handlers
├── config/                # 配置发现、替换、校验和 canonical hash
├── connection/            # connection manager、direct 与 rmcp adapter
├── daemon/                # Unix IPC、codec、metadata、paths、worker
├── policy/                # tool filter、search glob、retry、redaction
├── domain.rs              # transport-independent 领域模型
├── error.rs               # ErrorKind、CliError、retry class、exit code
├── output.rs              # 文本/JSON输出和 diagnostics
├── runtime.rs             # runtime env、deadline、clock、cancellation
├── lib.rs                 # 公共模块边界
└── main.rs                # 进程边界、signals、dispatch 和最终渲染

tests/
├── unit/                  # 跨模块单元测试
├── integration/           # stdio/HTTP/direct/daemon transport 测试
├── process/               # 真实 mcp-cli 子进程测试
├── properties/            # 37 组 correctness properties
└── support/               # fake clock、mock connection/HTTP、temp fixtures
```

## 必须保持的公开行为

### CLI

- 无参数 `mcp-cli` 执行 list；不存在公开的 `mcp-cli list` 子命令。
- 仅提供 `SERVER` 与 `info SERVER` 行为等价。
- `info SERVER TOOL` 与 `info SERVER/TOOL` 等价。
- `call SERVER TOOL [JSON]` 与 `call SERVER/TOOL [JSON]` 等价。
- `call` 有 inline JSON 时不得读取 stdin；非 TTY stdin 缺省读取，TTY/EOF/空白使用 `{}`。
- call 参数顶层必须是 JSON object，最大 16 MiB。
- 常见错误别名、歧义 target、缺失/多余参数必须保持结构化诊断和可执行建议。
- 隐藏 daemon 入口不得出现在 public help，也不得作为公开命令接受。

### 输出与退出码

- list、server info、grep 输出人类可读文本到 stdout。
- tool info 输出一个完整、紧凑 JSON Schema，加一个尾随换行。
- call 输出完整 MCP ToolResult JSON，不只提取 text content，不丢失未知扩展字段。
- errors、warnings、debug 和 stdio server logs 只写 stderr。
- `NO_COLOR` 存在时禁用 stdout/stderr ANSI 样式；非 TTY 默认无样式。
- 结果按服务器、工具和参数名称稳定排序。
- 退出码保持：`0` 成功、`1` 客户端/配置、`2` 工具业务错误、`3` 网络/超时、`4` 认证；Unix signals 使用 `130`/`143`。
- 顶层错误只渲染一次；cleanup error 不得覆盖原始 operation error。

### 配置

配置优先级必须是：

1. `-c/--config`
2. `MCP_CONFIG_PATH`
3. `<cwd>/mcp_servers.json`
4. `~/.mcp_servers.json`
5. `~/.config/mcp/mcp_servers.json`

其他不变量：

- 显式路径缺失或不可读时不得回退默认路径。
- 配置最大 16 MiB，非法 JSON 报告安全的路径、行和列。
- 每个 server 必须且只能有 `command` 或 `url`。
- stdio 允许 `args`、`env`、`cwd`；HTTP 允许 `headers`。
- URL 必须是带 host 的绝对 HTTP/HTTPS URL。
- `${VAR}` 只替换 JSON 字符串值，不替换 keys，不对替换结果再次递归展开。
- strict env 默认开启；`MCP_STRICT_ENV=false/0` 才使用空值并警告。
- env/header secrets 必须登记并跨输出、错误、stdio 和 daemon 边界脱敏。
- `disabledTools` 优先于 `allowedTools`，过滤同时约束可见性和调用授权。

### 连接、重试与资源

- 命令层只能依赖 transport-independent connection/domain 抽象；rmcp 类型应限制在 adapter 内。
- stdio 必须直接传 executable 与 args，不通过 shell 拼接。
- 配置 env 覆盖同名父进程 env，且传入顺序保持确定性。
- HTTP protocol-managed headers 不允许由用户配置覆盖。
- 所有连接、list、instructions、call、retry sleep 共享同一个命令绝对 deadline。
- `MCP_MAX_RETRIES=0` 合法并表示不重试；其他数值边界严格校验，不静默回退。
- 只重试规范定义的 transient network/HTTP 类；配置、JSON、401/403、授权和 business errors 不重试。
- 所有成功、失败、超时和 cancellation 路径都必须关闭连接并释放 registry、task 与 stdio child。
- direct batch 必须遵守 `MCP_CONCURRENCY`，隔离单服务器失败并保持成功结果。

### daemon 与 IPC

- Unix 默认优先 daemon；Windows 始终 direct。
- 每个 server 使用稳定 hash `ServerId`，不得直接把服务器名用于 socket/PID 路径。
- runtime directory、metadata、socket 和删除操作必须防 symlink、错误 owner/type 和路径逃逸。
- 替换后的 server config 通过 daemon stdin bootstrap 传递，不得放入 argv。
- IPC 使用严格、受限、增量 NDJSON codec；request/response frame 上限为 1 MiB。
- request ID、operation shape、response correlation 和 unknown fields 必须严格验证。
- config hash 变化、死 PID、socket 缺失和 stale metadata 要安全重建/清理。
- operational daemon failure 可按设计 fallback direct；security 或 non-transient failure 必须 fail closed。
- idle、signal、close 和竞争 shutdown trigger 必须汇聚到幂等 ordered cleanup。

## 实现规则

- 先读取相关实现、测试和 Spec，再修改代码；不要根据参考实现猜测 Rust 行为。
- 优先做最小、局部、可验证的修改，不做与任务无关的重构。
- 保持模块依赖方向；不要让 command/config/output 层直接耦合 rmcp transport 泛型。
- 使用现有 `CliError`、`ErrorKind`、`CommandContext`、deadline、diagnostics 和 redaction 边界，不引入平行机制。
- 用户可见 message、details、suggestion、error kind 和 exit code 是兼容接口，修改时必须同步测试。
- 输出必须确定化；优先使用 `BTreeMap` 或显式排序，不依赖 hash iteration order。
- Unix-only 代码必须用 `cfg(unix)` 隔离，Windows build 不得引用 Unix API。
- IPC/config serde 模型保持严格；安全边界优先使用 `deny_unknown_fields` 和大小上限。
- 不使用 `unsafe` 绕过 ownership、process 或 filesystem 安全约束，除非 Spec 明确需要并有集中审查与测试。
- 新增依赖前先确认标准库和现有依赖不能解决；依赖必须精确 pin，并同步 `Cargo.lock`。
- 不把 secrets、用户配置、真实 token、替换后的 header/env 或 daemon bootstrap 内容写入日志、测试快照和提交。
- 测试默认不得访问公网、真实用户 MCP 配置或真实用户 daemon 目录；使用 `tests/support` fixture 和隔离 temp runtime。

## 测试工作流

### 变更前

1. 定位对应 requirement、实现模块和现有测试 target。
2. 确认行为属于 portable、Unix daemon、Windows direct、transport、process 或 property 哪一层。
3. 优先运行最小相关测试，避免只依赖全套测试定位失败。

常用定向命令：

```bash
cargo test --test config
cargo test --test commands
cargo test --test output
cargo test --test runtime_retry
cargo test --test http_transport
cargo test --test direct_retry
cargo test --all-features --test stdio_transport
cargo test --all-features --test transport_contract
cargo test --all-features --test cli_end_to_end
cargo test --all-features --test daemon_linux
cargo test --all-features --test signals
```

property test 复现时保留 CI seed 和 case 数：

```bash
PROPTEST_RNG_SEED=<seed> PROPTEST_CASES=<cases> \
  cargo test --locked --all-features --test <failing-target> -- --test-threads=1
```

### 提交前最低门禁

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
```

修改 dependency、feature、平台条件或 CI 时，额外运行：

```bash
cargo check --locked --all-targets --all-features
```

CI 在 Linux、macOS 和 Windows 分别覆盖 portable/direct 行为，并在 Unix 运行 daemon、IPC、signal 和安全测试。不要因为本机平台不执行某个 target 就删除或弱化对应测试。

## 文档同步

行为变更时同步：

- 用户使用、配置或命令变化：`README.md`。
- Spec 行为变化：`.kiro/specs/mcp-cli/requirements.md`，必要时同步 design/tasks。
- 复测流程或真实 MCP 结论变化：`doc/test-report.md`。
- 架构摘要变化：`doc/design.md`。

不要把动态日期、短期 token 或本地绝对路径写成永久行为断言。真实 MCP 测试必须说明外部服务和 npm package 可能变化。

## Git 与文件范围

- `reference/` 和 `target/` 已在根 `.gitignore` 中忽略，不得强制加入 Git。
- 不提交临时 MCP config、Authorization header、测试 token、daemon socket/metadata、proptest 临时产物或本地日志。
- 暂存时优先指定明确路径，提交前检查 `git status --short` 和 staged diff。
- 不修改 Git config，不跳过 hooks，不使用破坏性 reset/clean/force push。
- 只有用户明确要求时才创建 commit 或 push；不得直接 push 到 main/master，除非用户明确要求。

## 完成标准

任务完成前确认：

1. 修改满足用户请求和对应 requirements。
2. 没有改变无关公开行为、输出格式、排序、错误、退出码或安全边界。
3. 相关定向测试通过。
4. `cargo fmt --check` 通过。
5. 对代码变更运行严格 Clippy 和适当范围测试；提交/发布前运行完整门禁。
6. 资源、child process、daemon artifact 和 temp fixture 已清理。
7. 文档与行为一致，没有无效链接、占位符或过期命令。
8. `git status` 只包含预期文件，`reference/` 和 `target/` 仍被忽略。
