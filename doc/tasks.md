本任务清单采用测试驱动、可演示的小步交付方式。每一步都建立在前一步可运行成果之上，并立即接入主执行路径，避免产生未使用模块或孤立代码。实现过程中以 `reference/mcp-cli/` 的行为和测试为兼容基线，但当前阶段只规划，不编写 Rust 源码。

## Task 1: 建立 Cargo 工程、配置加载与工具过滤基础

**目标：** 创建可重复构建的 `mcp-cli` Rust 工程，并交付可被后续命令直接使用的强类型配置系统。

**实现指导：**

- 创建 Rust 2024 edition 二进制 crate，package 与 binary 名称均为 `mcp-cli`。
- 固定 stable toolchain、精确依赖版本并提交 `Cargo.lock`；先验证 `rmcp = 2.2.0` 与所选 tokio/clap 版本兼容。
- 建立 `src/main.rs`、`config.rs` 及最小模块骨架；此阶段 `main` 至少能加载指定配置并给出成功或结构化失败结果。
- 实现 `McpServersConfig`、stdio/HTTP 枚举配置及 serde 字段映射，兼容 `mcpServers`、`allowedTools`、`disabledTools`。
- 按 CLI 显式路径、`MCP_CONFIG_PATH`、当前目录、用户目录、XDG 目录的优先级查找配置；显式路径缺失时不回退。
- 递归替换所有字符串中的 `${VAR}`；默认严格，`MCP_STRICT_ENV=false/0` 时替换为空并警告。
- 验证每个服务器恰好配置 `command` 或 `url`，并验证字段类型与非空约束。
- 实现大小写不敏感的工具 glob 完整匹配和统一 `is_tool_allowed`/`filter_tools`；`disabledTools` 优先。

**测试要求：**

- 移植 `reference/mcp-cli/tests/config.test.ts`：显式路径、缺失文件、非法 JSON、缺失 `mcpServers`、环境替换、strict/non-strict、空服务器、command/url 冲突、服务器查询与名称列表。
- 移植 `reference/mcp-cli/tests/filter.test.ts`：无过滤、精确匹配、`*`、`?`、大小写、allow/disable 组合和优先级。
- 使用临时目录和进程环境隔离测试；环境变量测试必须串行化或加锁，避免并发污染。

**Demo:** 使用一个同时包含 stdio、HTTP、`${TOKEN}` 和过滤规则的临时 `mcp_servers.json` 运行最小 CLI，能成功加载并打印服务器名称；非法配置返回稳定的非零退出码和明确错误。

## Task 2: 交付结构化错误与稳定输出格式

**目标：** 建立后续所有模块共用的错误边界和输出格式，使 stdout、stderr、颜色和退出码从早期开始可测试。

**实现指导：**

- 创建 `errors.rs`，定义 `CliError`、稳定的错误类型、exit code、message、details、suggestion 和 source chain。
- 实现配置、服务器、工具、JSON、参数、未知选项/子命令、歧义命令、连接与执行错误构造器。
- 创建 `output.rs` 和共享 `ToolInfo` 展示模型，实现服务器列表、搜索结果、服务器详情、工具 schema、工具调用 JSON 和错误文本格式。
- list/info/grep 写 stdout；call 写完整 JSON 到 stdout；错误与诊断写 stderr。
- 仅对应流为 TTY 且未设置 `NO_COLOR` 时使用 ANSI 样式；颜色逻辑不得改变纯文本内容。
- 将 Task 1 的临时错误输出替换为本任务的 `CliError`，确保模块立即投入使用。

**测试要求：**

- 移植 `reference/mcp-cli/tests/output.test.ts`，并按本项目约定验证 call 的完整 JSON 输出。
- 移植 `reference/mcp-cli/tests/errors.test.ts`，覆盖格式、详情、建议和常见原因的建议映射。
- 增加 stdout/stderr 分离、TTY/`NO_COLOR`、JSON 可被重新解析及敏感字段不进入错误信息的测试。

**Demo:** 分别运行有效配置与三种无效配置，展示固定 `Error [TYPE]` 格式、正确退出码及无颜色管道输出；将模拟 call 输出直接通过 `jq` 解析成功。

## Task 3: 完成 clap CLI 与参考兼容的参数诊断

**目标：** 交付四类公开命令的完整语法入口，同时保持参考实现对 AI 代理友好的纠错建议。

**实现指导：**

- 在 `cli.rs` 使用 clap derive 定义全局 `-c/--config`、`-d/--with-descriptions`、`-h/--help`、`-v/--version` 以及 `info`、`grep`、`call` 子命令。
- 无子命令时路由到 list；单个服务器名按兼容规则路由到 info。
- `info`/`call` 同时接受 `server tool` 与 `server/tool`。
- 在 clap 解析前后增加轻量兼容层，识别 `run/execute/exec/invoke`、`list/ls/get/show/describe`、`search/find/query` 等常见别名并给出正确建议。
- 检测 `mcp-cli server tool`、裸 `server/tool`、缺失参数、空 tool、多余参数、未知选项，转换为 Task 2 的结构化错误。
- 将命令暂时接到可测试的 handler 接口；尚未实现网络功能的分支返回明确的内部“未连接”测试替身，而不是遗留未使用解析结构。

**测试要求：**

- 逐项移植 `reference/mcp-cli/tests/cli-errors.test.ts` 中的 22 类错误场景，断言 exit code、stdout 为空、stderr 的类型与建议。
- 增加 help/version、全局选项位置、两种 target 格式和无参数 list 路由测试。
- 使用 `assert_cmd` 以真实子进程运行编译后的 CLI，而不仅测试内部解析函数。

**Demo:** `mcp-cli --help` 和 `mcp-cli --version` 正常；`mcp-cli run server tool` 建议 `call`；`mcp-cli server tool` 同时建议 `call` 与 `info`；合法四类命令均能到达对应 handler。

## Task 4: 实现 rmcp Direct stdio/HTTP 连接、过滤、重试与超时

**目标：** 在禁用 daemon 时交付完整 MCP 客户端能力，后续所有命令和 daemon 均复用此连接层。

**实现指导：**

- 在 `client.rs` 定义统一 `McpConnection` 抽象及 direct 实现，提供 `list_tools`、`call_tool`、`get_instructions`、`close`。
- stdio 使用 `rmcp::transport::TokioChildProcess`；直接设置 executable、args、cwd 和合并环境，不通过 shell 拼接。
- HTTP 使用 `StreamableHttpClientTransport` 的 reqwest 后端，支持配置 headers，并对诊断信息脱敏。
- 映射 rmcp Tool/CallToolResult 为稳定内部模型或 JSON，避免命令层依赖具体 transport 泛型。
- `list_tools` 返回前应用 Task 1 的过滤；`call_tool` 执行前再次检查授权。
- 实现总预算内的指数退避、±25% jitter、最大延迟与瞬态错误分类；认证、配置、JSON 和工具验证错误不重试。
- 用 `tokio::time::timeout` 限制连接和请求；任何路径都安全关闭客户端及 stdio 子进程。
- `MCP_NO_DAEMON=1` 直接选择此实现，为 Task 5/6 提供真实能力。

**测试要求：**

- 移植 `reference/mcp-cli/tests/client.test.ts` 的瞬态错误、非瞬态错误、重试次数、退避和安全关闭测试。
- 创建最小 mock stdio MCP server，验证初始化、列举工具、instructions、调用和 stderr 分流。
- 创建本地 mock Streamable HTTP server，验证 headers、HTTP 调用、429/502 重试、401/403 不重试和 timeout。
- 验证 disabled tool 既不出现在列表中，也不能被按名称调用。

**Demo:** 在 `MCP_NO_DAEMON=1` 下分别连接本地 stdio mock 和 HTTP mock，列出工具并调用一个 echo 工具；暂时断开 HTTP mock 时展示受控重试和最终网络错误。

## Task 5: 交付 list 与 info 的端到端功能

**目标：** 让用户能够发现全部服务器/工具，并检查单个服务器或工具 schema。

**实现指导：**

- 创建并接入 `commands/list.rs`：加载配置，以 `MCP_CONCURRENCY` 为上限并发连接所有服务器，同时获取 tools 与 instructions。
- 单服务器失败转为该服务器的可读错误项，不取消其他服务器任务；结果按服务器名称稳定排序。
- 创建并接入 `commands/info.rs`：仅连接目标服务器；显示 transport、command/URL、instructions、工具、参数和可选描述。
- 指定 tool 时显示完整输入 schema；不存在时列出有限数量可用工具并给出恢复建议。
- 所有连接通过 Task 4 的抽象获取并在 finally/RAII 路径关闭，不在命令层直接构建 transport。

**测试要求：**

- 为有界并发编写可观测 mock 测试，确认同时活跃连接不超过 `MCP_CONCURRENCY`。
- 测试稳定排序、空配置、部分失败、instructions 截断/完整展示、`-d`、未知服务器和未知工具。
- 从 `reference/mcp-cli/tests/integration/cli.test.ts` 移植 list/info 对应场景，并断言 stdout/stderr 与退出码。

**Demo:** 使用两个 mock server（其中一个可配置失败）运行 `mcp-cli`、`mcp-cli -d`、`mcp-cli info server` 和 `mcp-cli info server/tool`；成功服务器始终展示，目标工具 schema 可读。

## Task 6: 交付 grep 与 call，并固定管道语义

**目标：** 完成工具搜索与执行工作流，使 CLI 四个公开命令在 direct 模式全部可用。

**实现指导：**

- 创建并接入 `commands/grep.rs`，实现参考语义的 glob 编译：`*` 不跨 `/`、`**` 可跨 `/`、`?` 匹配单字符、正则特殊字符按字面处理、大小写不敏感。
- grep 并发访问服务器并匹配工具名；收集部分失败警告，结果稳定排序，无结果时打印参考风格提示。
- 创建并接入 `commands/call.rs`：解析两种 target 格式；内联 JSON 优先，否则从非 TTY stdin 读取，空输入为 `{}`。
- 要求调用参数为 JSON object；设置 stdin 大小和读取超时限制。
- 将参数转换为 rmcp 调用模型，完整 CallToolResult 序列化为 JSON stdout；任何 warning/debug 均保持在 stderr。
- tool not found、tool disabled 与业务执行失败使用不同结构化错误和正确退出码。

**测试要求：**

- 完整移植 `reference/mcp-cli/tests/grep.test.ts`，覆盖 `*`、`**`、`***`、`?`、斜杠和正则字符转义。
- 移植 call 相关集成场景：内联 JSON、stdin、空对象、非法 JSON、非 object JSON、未知/禁用工具、工具失败和多内容响应。
- 增加 `mcp-cli call ... | jq` 测试，确认 stdout 只有合法 JSON；grep 部分服务器失败时成功结果仍存在。

**Demo:** `mcp-cli grep "*file*"` 找到多个服务器的工具；内联参数与 heredoc 两种方式调用 echo 工具，结果都可直接经 `jq` 读取。

## Task 7: 实现 Unix daemon worker、IPC 与生命周期

**目标：** 在不改变命令层的情况下，提供可跨 CLI 进程复用 MCP 连接的安全后台 worker。

**实现指导：**

- 用 `cfg(unix)` 创建 `daemon.rs`，通过隐藏的 self-exec 入口启动；Windows 构建不包含 Unix API。
- 为每个服务器创建安全文件名、权限为 `0700` 的用户运行目录、Unix socket 和 `0600` PID 元数据。
- PID 元数据只保存 pid、SHA-256 config hash 和启动时间，不保存凭据。
- 配置通过 stdin 或权限受限的短期文件传给 daemon，避免出现在 `ps` 参数中。
- 定义 serde IPC request/response，支持 `ping`、`listTools`、`callTool`、`getInstructions`、`close`，并校验 request ID。
- 使用换行分帧 codec 和最大帧限制，正确处理拆包、粘包、非法 JSON与客户端中断。
- worker 复用 Task 4 direct 连接；每次有效请求重置 idle timer；SIGINT、SIGTERM、idle 和 close 均执行幂等清理。
- 启动完成前不发布 ready；绑定 socket、连接 MCP 成功和写 PID 的顺序需避免客户端观察半初始化状态。

**测试要求：**

- Unix 单元测试覆盖安全文件名、稳定 config hash、PID 读写、进程存活探测和幂等文件清理。
- IPC 测试覆盖每种请求、并发客户端、拆/粘包、超大帧、非法 JSON、错误响应与 request ID。
- 进程级测试启动 worker，验证 ready、连接复用、idle timeout、SIGTERM 和异常退出后的文件状态。

**Demo:** 手动启动隐藏 daemon 入口后，用测试客户端依次 ping、list、call；多次请求复用同一 mock MCP 进程，设置短 idle timeout 后 worker 自动退出且 socket/PID 消失。

## Task 8: 实现 daemon client 与统一连接管理器

**目标：** 将 daemon 无缝接入四个命令，并保证任何 daemon 故障都可快速回退到 direct 模式。

**实现指导：**

- 创建 `daemon_client.rs`，实现 Unix socket 连接、请求 ID、分帧、响应关联、5 秒请求 timeout 和错误转换。
- 启动时扫描运行目录，清理 PID 不存活的孤儿文件；不得终止无法证明归属本用户/本程序的进程。
- 依次校验 PID、进程存活、config hash、socket 和 ping；配置陈旧时先优雅 close，再 SIGTERM 与清理。
- 无有效 daemon 时 self-spawn Task 7 worker，等待 ready 并 ping；5 秒内失败则返回可回退状态。
- 在 `client.rs` 完成 ConnectionManager：Unix 且 daemon 未禁用时优先 IPC，Windows、`MCP_NO_DAEMON=1` 或任意 daemon 初始化失败时使用 Task 4 direct。
- daemon connection 的 close 仅断开当前 IPC；后台连接由 idle timeout 管理。
- list/info/grep/call 全部只依赖统一获取函数，不包含模式分支。

**测试要求：**

- 集成测试覆盖首次 spawn、后续复用、不同服务器独立 worker、配置 hash 变化重启、孤儿清理、socket 缺失、ping 超时和 direct fallback。
- 验证 `MCP_NO_DAEMON=1` 从不创建运行文件；Windows 条件测试/CI 验证 direct-only 编译与行为。
- 验证 daemon 模式仍执行工具过滤，且输出和 direct 模式一致。

**Demo:** 连续运行两次 `mcp-cli info test`，第二次复用相同 daemon/PID；修改配置后自动替换 worker；模拟 socket 故障时命令仍通过 direct 模式成功。

## Task 9: 完成全局接线、信号处理、端到端验证与发布门禁

**目标：** 形成可发布的单二进制，实现全部环境变量、平台行为和参考兼容性，并以自动化质量门禁收尾。

**实现指导：**

- 完成 `main.rs` 命令分发和顶层错误边界，保证错误只打印一次并返回约定 exit code。
- 完整接入 `MCP_CONFIG_PATH`、`MCP_DEBUG`、`MCP_TIMEOUT`、`MCP_CONCURRENCY`、`MCP_MAX_RETRIES`、`MCP_RETRY_DELAY`、`MCP_STRICT_ENV`、`MCP_NO_DAEMON`、`MCP_DAEMON_TIMEOUT`。
- 完成 SIGINT/SIGTERM 行为、stdout flush、direct 子进程关闭与 daemon 幂等清理。
- 逐项审查 `reference/mcp-cli/tests/`；将尚未覆盖的 `cli-errors.test.ts`、`config.test.ts`、`filter.test.ts`、`output.test.ts`、`errors.test.ts`、`grep.test.ts`、`client.test.ts` 和 `integration/cli.test.ts` 场景补齐。
- 增加 Linux/macOS/Windows CI：Unix 跑 daemon + direct，Windows 跑 direct-only；测试不依赖外部公网 MCP 服务。
- 建立 release profile、许可证/README/示例配置及跨平台构建检查；暂不添加设计范围外的新命令。
- 对设计与实现差异做最终记录，特别检查 call JSON、配置优先级、排序和错误文本兼容性。

**测试要求：**

- 运行完整单元、进程级、stdio/HTTP、Unix daemon 和跨平台条件测试。
- 验证所有命令、两种 target 格式、stdin/inline JSON、部分服务器失败、重试、timeout、配置变化、signals 和每个环境变量。
- 执行并通过以下发布门禁：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
```

- 在 Windows CI 上使用适当 feature/cfg 运行 direct-only 测试；在 Linux/macOS CI 上额外运行 daemon 生命周期测试。

**Demo:** 从干净 checkout 构建 release 二进制，使用同一配置演示 list、info、grep、inline call、stdin call、daemon 复用和 `MCP_NO_DAEMON=1` direct；随后执行全部四条质量命令并展示通过结果。
