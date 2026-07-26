# 需求文档

## 简介

`mcp-cli` 是参考 TypeScript/Bun 实现的 Rust 等价版本，用于从命令行发现、检查、搜索和调用 Model Context Protocol（MCP）服务器工具。本功能以单一 `mcp-cli` 二进制提供 stdio 与 Streamable HTTP 传输，在 Linux 和 macOS 上优先复用 Unix daemon 连接，在 Windows 或禁用 daemon 时使用 direct 连接，并保持配置格式、命令语义、输出流、错误分类和自动化脚本兼容性。

## 术语表

- **MCP_CLI**：本需求定义的 Rust 命令行系统及其 `mcp-cli` 可执行文件。
- **MCP_Server**：通过 MCP 协议向 MCP_CLI 提供 instructions 和工具的进程或 HTTP 服务。
- **MCP_Tool**：由 MCP_Server 公布并可通过名称和 JSON 参数调用的操作。
- **Tool_Schema**：描述 MCP_Tool 输入参数的 JSON Schema。
- **Tool_Result**：MCP_Tool 调用返回的完整 MCP 结果对象。
- **Server_Configuration**：`mcpServers` 中一个服务器名称对应的配置对象。
- **Configuration_File**：采用 JSON 格式并包含顶层 `mcpServers` 对象的配置文件。
- **Configuration_Loader**：MCP_CLI 中负责查找、读取、替换和验证 Configuration_File 的组件。
- **Explicit_Configuration_Path**：由 `-c/--config` 或 `MCP_CONFIG_PATH` 指定的配置路径。
- **Stdio_Configuration**：包含 `command`，并可包含 `args`、`env` 和 `cwd` 的 Server_Configuration。
- **HTTP_Configuration**：包含 `url`，并可包含 `headers` 的 Server_Configuration。
- **Tool_Filter**：根据 `allowedTools` 和 `disabledTools` 判定 MCP_Tool 可见性与可调用性的组件。
- **Tool_Filter_Pattern**：大小写不敏感、完整匹配工具名称且支持 `*` 与 `?` 的 glob 模式；`*` 匹配任意数量字符，`?` 匹配一个字符。
- **Search_Pattern**：`grep` 使用的大小写不敏感 glob 模式；`*` 不跨越 `/`，`**` 可跨越 `/`，`?` 匹配一个字符，其他正则特殊字符按字面量处理。
- **Connection_Manager**：为命令选择 Daemon_Connection 或 Direct_Connection 的组件。
- **Direct_Connection**：由当前 MCP_CLI 进程直接建立并在命令结束时关闭的 MCP 连接。
- **Daemon_Worker**：Linux 或 macOS 上为一个 MCP_Server 持有一个可复用 MCP 连接的后台进程。
- **Daemon_Connection**：MCP_CLI 通过 Unix socket 与 Daemon_Worker 建立的 IPC 连接。
- **Runtime_Directory**：`${TMPDIR:-/tmp}/mcp-cli-<uid>/` 形式的当前用户 daemon 运行目录。
- **Config_Hash**：对规范化 Server_Configuration 稳定序列化后计算的 SHA-256 摘要，daemon 元数据至少保存 128 bit 十六进制摘要。
- **IPC_Request**：带请求 ID 的换行分隔 JSON daemon 请求，类型为 `ping`、`listTools`、`callTool`、`getInstructions` 或 `close`。
- **IPC_Response**：带相同请求 ID、成功数据或稳定错误代码的换行分隔 JSON daemon 响应。
- **Transient_Error**：`ECONNREFUSED`、`ECONNRESET`、`ETIMEDOUT`、`EPIPE`、`ENETUNREACH`、`EHOSTUNREACH`、`EAI_AGAIN` 或 HTTP 429、502、503、504。
- **Non_Transient_Error**：配置错误、JSON 错误、schema 验证错误、HTTP 401/403、参数错误或明确的工具业务错误。
- **Total_Timeout_Budget**：由 `MCP_TIMEOUT` 指定的单次命令连接、重试、等待和请求总时限，默认 1800 秒。
- **Retry_Limit**：由 `MCP_MAX_RETRIES` 指定的首次尝试之后的最大重试次数，默认 3，值 0 表示不重试。
- **Retry_Base_Delay**：由 `MCP_RETRY_DELAY` 指定的重试基础延迟，默认 1000 毫秒。
- **Concurrency_Limit**：由 `MCP_CONCURRENCY` 指定的同时处理服务器数量，默认 5。
- **Daemon_Idle_Timeout**：由 `MCP_DAEMON_TIMEOUT` 指定的 daemon 空闲时限，默认 60 秒。
- **Structured_Error**：包含稳定错误类型、消息、可选 Details 和可选 Suggestion 的用户可见错误。
- **Diagnostic_Output**：警告、debug 信息和 MCP_Server 子进程 stderr，不属于命令业务结果。
- **TTY**：与交互式终端连接的标准输入或标准输出流。
- **Semantic_Equivalence**：两个 JSON 或配置值忽略对象键顺序后具有相同类型、键和值。
- **IPC_Max_Frame_Size**：单个 IPC_Request 或 IPC_Response 的最大 UTF-8 编码长度，固定为 1 MiB。
- **Call_Input_Max_Size**：`call` 从内联参数或 stdin 接受的最大 UTF-8 编码长度，固定为 16 MiB。

## 需求

### 需求 1：公开命令与目标语法

**用户故事：** 作为命令行用户，我希望使用稳定且兼容的命令语法发现和操作 MCP 工具，以便从 Shell 或 AI 代理完成 MCP 工作流。

#### 验收标准

1. WHEN 用户不提供子命令或服务器名称, THE MCP_CLI SHALL 列出配置中的全部 MCP_Server 及获准显示的 MCP_Tool。
2. WHEN 用户仅提供一个有效服务器名称, THE MCP_CLI SHALL 执行对应 MCP_Server 的 `info` 行为。
3. WHEN 用户执行 `info <server>`, THE MCP_CLI SHALL 仅查询目标 MCP_Server 并显示传输信息、instructions、工具名称和工具参数。
4. WHEN 用户执行 `info <server> <tool>`, THE MCP_CLI SHALL 显示目标 MCP_Tool 的完整 Tool_Schema。
5. WHEN 用户执行 `info <server>/<tool>`, THE MCP_CLI SHALL 产生与 `info <server> <tool>` 相同的业务结果。
6. WHEN 用户执行 `grep <pattern>`, THE MCP_CLI SHALL 在所有获准显示的 MCP_Tool 名称中应用 Search_Pattern。
7. WHEN 用户执行 `call <server> <tool> <json>`, THE MCP_CLI SHALL 使用内联 JSON object 调用目标 MCP_Tool。
8. WHEN 用户执行 `call <server>/<tool> <json>`, THE MCP_CLI SHALL 产生与 `call <server> <tool> <json>` 相同的调用请求。
9. WHEN 用户提供 `-h/--help`, THE MCP_CLI SHALL 向 stdout 输出公开命令、公开选项和参数语法并以退出码 0 结束。
10. WHEN 用户提供 `-v/--version`, THE MCP_CLI SHALL 向 stdout 输出 MCP_CLI 版本并以退出码 0 结束。
11. WHERE 用户提供 `-d/--with-descriptions`, THE MCP_CLI SHALL 在 list、info 和 grep 的工具条目中包含可用工具描述。
12. WHEN 用户提供未知命令、常见错误别名、未知选项、空的 `server/`、缺失参数、多余位置参数或歧义的 `server tool`, THE MCP_CLI SHALL 返回带有效命令建议的 Structured_Error。

### 需求 2：配置路径发现与 JSON 解析

**用户故事：** 作为使用多种 MCP 客户端配置的用户，我希望 MCP_CLI 按确定顺序加载兼容配置，以便复用现有服务器定义。

#### 验收标准

1. WHERE `-c/--config <path>` 存在, THE Configuration_Loader SHALL 将该路径作为第一优先级 Explicit_Configuration_Path。
2. WHERE `-c/--config <path>` 不存在且 `MCP_CONFIG_PATH` 已设置, THE Configuration_Loader SHALL 将 `MCP_CONFIG_PATH` 值作为 Explicit_Configuration_Path。
3. WHEN Explicit_Configuration_Path 不存在或不可读取, THE Configuration_Loader SHALL 返回 `CONFIG_NOT_FOUND` 或 `CONFIG_READ_ERROR` 且不搜索默认路径。
4. WHEN Explicit_Configuration_Path 未设置, THE Configuration_Loader SHALL 依次搜索 `<cwd>/mcp_servers.json`、`~/.mcp_servers.json` 和 `~/.config/mcp/mcp_servers.json`。
5. IF 默认搜索路径均不存在, THEN THE Configuration_Loader SHALL 返回列出全部已搜索路径的 `CONFIG_NOT_FOUND` 错误。
6. WHEN Configuration_File 包含非法 JSON, THE Configuration_Loader SHALL 返回包含文件路径和 JSON 位置的 `INVALID_CONFIG` 错误。
7. WHEN Configuration_File 缺少对象类型的 `mcpServers`, THE Configuration_Loader SHALL 返回 `INVALID_CONFIG` 错误。
8. WHEN Configuration_File 包含有效的 `mcpServers`, THE Configuration_Loader SHALL 保留全部服务器名称并按名称提供确定性顺序。
9. THE Configuration_Loader SHALL 接受 Claude Desktop、VS Code 和 Gemini 使用的 `mcpServers`、`allowedTools` 与 `disabledTools` 字段名称。
10. WHEN Configuration_Loader 对有效 Server_Configuration 执行规范化序列化并重新解析, THE Configuration_Loader SHALL 产生 Semantic_Equivalence 的 Server_Configuration。

### 需求 3：环境变量替换与服务器配置验证

**用户故事：** 作为配置维护者，我希望配置值安全地引用环境变量并在连接前得到完整验证，以便尽早发现配置错误。

#### 验收标准

1. WHEN Configuration_File 的任意字符串节点包含 `${VAR_NAME}` 且 `VAR_NAME` 已定义, THE Configuration_Loader SHALL 使用对应环境变量值替换每个占位符。
2. WHILE `MCP_STRICT_ENV` 未设置、为 `true` 或为 `1`, WHEN 任意引用的环境变量缺失, THE Configuration_Loader SHALL 返回不包含环境变量实际值的 `MISSING_ENV_VAR` 错误。
3. WHILE `MCP_STRICT_ENV` 为 `false` 或 `0`, WHEN 任意引用的环境变量缺失, THE Configuration_Loader SHALL 使用空字符串替换缺失值并向 stderr 写入变量名称警告。
4. WHEN Server_Configuration 为 `null` 或非对象值, THE Configuration_Loader SHALL 返回标识服务器名称的 `INVALID_SERVER_CONFIG` 错误。
5. WHEN Server_Configuration 仅包含非空 `command`, THE Configuration_Loader SHALL 将 Server_Configuration 识别为 Stdio_Configuration。
6. WHEN Server_Configuration 仅包含可解析的 HTTP 或 HTTPS `url`, THE Configuration_Loader SHALL 将 Server_Configuration 识别为 HTTP_Configuration。
7. IF Server_Configuration 同时包含 `command` 和 `url`, THEN THE Configuration_Loader SHALL 返回 `INVALID_SERVER_CONFIG` 错误。
8. IF Server_Configuration 同时缺少 `command` 和 `url`, THEN THE Configuration_Loader SHALL 返回 `INVALID_SERVER_CONFIG` 错误。
9. IF `command` 为空字符串、`args` 不是字符串数组、`env` 不是字符串映射、`headers` 不是字符串映射或过滤字段不是字符串数组, THEN THE Configuration_Loader SHALL 返回包含字段名称的 `INVALID_SERVER_CONFIG` 错误。
10. WHEN Stdio_Configuration 定义 `env`, THE Connection_Manager SHALL 将配置环境与当前进程环境合并并以配置值覆盖同名父环境值。

### 需求 4：工具过滤与授权一致性

**用户故事：** 作为服务器配置维护者，我希望同一过滤策略同时约束发现和调用，以便被禁用的工具无法通过其他命令绕过。

#### 验收标准

1. WHEN Tool_Filter_Pattern 包含 `*`, THE Tool_Filter SHALL 将 `*` 解释为任意数量字符并对完整工具名称进行匹配。
2. WHEN Tool_Filter_Pattern 包含 `?`, THE Tool_Filter SHALL 将 `?` 解释为恰好一个字符并对完整工具名称进行匹配。
3. THE Tool_Filter SHALL 以大小写不敏感方式匹配 Tool_Filter_Pattern 与 MCP_Tool 名称。
4. WHEN MCP_Tool 名称命中任一 `disabledTools` 模式, THE Tool_Filter SHALL 将 MCP_Tool 判定为禁用。
5. WHERE `allowedTools` 包含至少一个模式, WHEN MCP_Tool 名称未命中任何 `allowedTools` 模式, THE Tool_Filter SHALL 将 MCP_Tool 判定为未授权。
6. WHERE `allowedTools` 为空或未配置, WHEN MCP_Tool 名称未命中 `disabledTools`, THE Tool_Filter SHALL 将 MCP_Tool 判定为授权。
7. WHEN list、info 或 grep 接收 MCP_Tool 集合, THE MCP_CLI SHALL 从业务输出移除 Tool_Filter 判定为禁用或未授权的 MCP_Tool。
8. IF call 的目标 MCP_Tool 被 Tool_Filter 判定为禁用或未授权, THEN THE MCP_CLI SHALL 在发送 MCP 调用前返回 `TOOL_DISABLED` 错误。
9. WHEN 同一工具集合分别经过展示过滤和调用授权检查, THE Tool_Filter SHALL 对每个工具产生相同的允许或拒绝结论。

### 需求 5：stdio 与 Streamable HTTP 传输

**用户故事：** 作为 MCP 用户，我希望本地进程服务器和远程 HTTP 服务器具有一致的命令能力，以便使用同一 CLI 操作两类服务。

#### 验收标准

1. WHEN Connection_Manager 连接 Stdio_Configuration, THE MCP_CLI SHALL 直接以 `command` 作为可执行文件并以 `args` 作为参数启动子进程。
2. WHERE Stdio_Configuration 包含 `cwd`, THE MCP_CLI SHALL 将子进程工作目录设置为配置的 `cwd`。
3. THE MCP_CLI SHALL 在不经过 Shell 字符串拼接的情况下启动 stdio 子进程。
4. WHEN Connection_Manager 连接 HTTP_Configuration, THE MCP_CLI SHALL 使用 MCP Streamable HTTP 客户端连接配置的 `url`。
5. WHERE HTTP_Configuration 包含 `headers`, THE MCP_CLI SHALL 将配置 headers 附加到 Streamable HTTP 请求。
6. WHEN 任一传输建立 MCP 连接, THE MCP_CLI SHALL 完成 MCP initialize 和 initialized 生命周期后再列举或调用工具。
7. WHEN MCP_Server 提供 instructions, THE MCP_CLI SHALL 通过统一连接接口向 info 命令提供 instructions。
8. WHEN Direct_Connection 正常完成、失败或被取消, THE MCP_CLI SHALL 关闭 MCP 会话以及已启动的 stdio 子进程。
9. WHEN stdio MCP_Server 写入 stderr, THE MCP_CLI SHALL 将带服务器名称前缀的内容转发到 MCP_CLI stderr 而不写入 stdout。

### 需求 6：Daemon 模式选择与 Direct 回退

**用户故事：** 作为频繁调用 MCP 工具的 Unix 用户，我希望复用服务器连接并在 daemon 不可用时自动降级，以便兼顾性能与可靠性。

#### 验收标准

1. WHILE MCP_CLI 运行于 Linux 或 macOS 且 `MCP_NO_DAEMON` 不为 `1`, THE Connection_Manager SHALL 优先获取目标 MCP_Server 的 Daemon_Connection。
2. WHILE `MCP_NO_DAEMON` 为 `1`, THE Connection_Manager SHALL 仅创建 Direct_Connection 且不创建 daemon 运行文件。
3. WHILE MCP_CLI 运行于 Windows, THE Connection_Manager SHALL 仅创建 Direct_Connection。
4. WHEN 有效 Daemon_Worker 的 PID、Config_Hash、socket 和 ping 均通过校验, THE Connection_Manager SHALL 复用对应 Daemon_Worker。
5. IF Daemon_Worker 不存在、进程已终止、socket 缺失、Config_Hash 不匹配或 ping 失败, THEN THE Connection_Manager SHALL 将对应 daemon 状态判定为无效。
6. WHEN daemon 启动、ready 或 ping 未在 5 秒内成功, THE Connection_Manager SHALL 在当前命令中回退到 Direct_Connection。
7. WHEN daemon IPC 请求未在 5 秒内完成, THE Connection_Manager SHALL 在当前命令中回退到 Direct_Connection。
8. WHEN Connection_Manager 检测到 PID 对应进程已终止的孤儿记录, THE Connection_Manager SHALL 删除对应 PID 文件和 socket 文件。
9. WHEN 命令关闭 Daemon_Connection, THE Connection_Manager SHALL 仅关闭当前 IPC 客户端连接并保留 Daemon_Worker 的 MCP 连接。
10. WHEN direct 模式与 daemon 模式处理相同的服务器响应, THE MCP_CLI SHALL 产生相同的 stdout 业务内容和退出码。

### 需求 7：Daemon 生命周期、IPC 与配置变更

**用户故事：** 作为 Unix 用户，我希望后台连接按服务器隔离、安全复用并自动清理，以便多次 CLI 调用不会留下陈旧资源。

#### 验收标准

1. WHEN MCP_CLI 为服务器创建 Daemon_Worker, THE MCP_CLI SHALL 为该 MCP_Server 创建独立后台进程和独立 Unix socket。
2. WHEN MCP_CLI 创建 Runtime_Directory, THE MCP_CLI SHALL 将 Runtime_Directory 权限设置为 `0700`。
3. WHEN Daemon_Worker 写入 PID 元数据, THE Daemon_Worker SHALL 将 PID 文件权限设置为 `0600`。
4. THE Daemon_Worker SHALL 在 PID 元数据中仅保存进程 ID、Config_Hash 和启动时间。
5. WHEN Daemon_Worker 接收有效 IPC_Request, THE Daemon_Worker SHALL 返回请求 ID 相同的 IPC_Response。
6. WHEN Unix socket 连续接收拆分帧或多个粘连帧, THE Daemon_Worker SHALL 按换行边界分别解析每个完整 IPC_Request。
7. IF IPC_Request 不是有效 JSON、缺少请求 ID或包含未知请求类型, THEN THE Daemon_Worker SHALL 返回稳定 IPC 错误响应且保持 worker 可服务状态。
8. IF IPC_Request 或 IPC_Response 超过 IPC_Max_Frame_Size, THEN THE Daemon_Worker SHALL 拒绝对应帧并关闭对应 IPC 客户端连接。
9. WHEN Daemon_Worker 完成有效 IPC_Request, THE Daemon_Worker SHALL 重置 Daemon_Idle_Timeout 计时器。
10. WHILE 连续无有效请求的时间达到 Daemon_Idle_Timeout, THE Daemon_Worker SHALL 关闭 MCP 连接并删除对应 socket 与 PID 文件。
11. WHEN Daemon_Worker 接收 SIGINT、SIGTERM 或显式 `close` IPC_Request, THE Daemon_Worker SHALL 幂等关闭 MCP 连接并删除对应 socket 与 PID 文件。
12. WHEN Connection_Manager 检测到 Config_Hash 变化, THE Connection_Manager SHALL 停止陈旧 Daemon_Worker并为新配置建立新连接。
13. WHEN Daemon_Worker 未完成 MCP 连接、socket 绑定和 PID 元数据原子发布, THE Daemon_Worker SHALL 不公布 ready 状态。

### 需求 8：重试、退避与总超时

**用户故事：** 作为调用不稳定远程服务的用户，我希望瞬态故障得到受控重试且命令不超过总时限，以便获得可预测的恢复行为。

#### 验收标准

1. WHEN 连接或请求因 Transient_Error 失败且已执行重试次数小于 Retry_Limit, THE MCP_CLI SHALL 在 Total_Timeout_Budget 内重试对应操作。
2. IF 连接或请求因 Non_Transient_Error 失败, THEN THE MCP_CLI SHALL 立即返回对应错误且不重试。
3. WHEN 执行编号为 `attempt` 且从 0 开始的重试等待, THE MCP_CLI SHALL 使用 `min(Retry_Base_Delay × 2^attempt, 10秒)` 作为抖动前延迟。
4. WHEN 生成重试抖动, THE MCP_CLI SHALL 将实际延迟限制在抖动前延迟的 75% 至 125% 范围内。
5. WHILE Total_Timeout_Budget 的剩余时间不足以完成等待并发起下一次尝试, THE MCP_CLI SHALL 停止重试并返回 `TIMEOUT` 错误。
6. WHEN 命令运行时间达到 Total_Timeout_Budget, THE MCP_CLI SHALL 取消未完成的连接、等待和请求并返回退出码 3。
7. WHERE `MCP_MAX_RETRIES` 为 `0`, THE MCP_CLI SHALL 对首次失败不执行重试。
8. WHERE `MCP_DEBUG` 未启用, THE MCP_CLI SHALL 不输出重试诊断。
9. WHERE `MCP_DEBUG` 已启用, WHEN MCP_CLI 安排重试, THE MCP_CLI SHALL 向 stderr 输出尝试编号、错误分类和延迟且不输出凭据值。

### 需求 9：list 与 info 行为

**用户故事：** 作为工具使用者，我希望获得确定且完整的服务器与工具信息，以便选择正确的调用目标。

#### 验收标准

1. WHEN list 查询多个 MCP_Server, THE MCP_CLI SHALL 按服务器名称排序输出服务器结果。
2. WHEN list 获取服务器工具, THE MCP_CLI SHALL 按工具名称排序每个服务器的获准工具。
3. IF list 无法连接一个 MCP_Server, THEN THE MCP_CLI SHALL 输出该服务器的可读失败项并继续处理其他 MCP_Server。
4. WHEN list 的至少一个 MCP_Server 成功, THE MCP_CLI SHALL 保留全部成功服务器结果而不因其他服务器失败取消命令。
5. WHEN `info <server>` 指定的服务器不存在, THE MCP_CLI SHALL 返回包含可用服务器名称建议的 `SERVER_NOT_FOUND` 错误。
6. WHEN `info <server> <tool>` 指定的获准工具不存在, THE MCP_CLI SHALL 返回包含目标服务器可用工具名称建议的 `TOOL_NOT_FOUND` 错误。
7. WHEN `info <server>` 成功, THE MCP_CLI SHALL 仅创建目标 MCP_Server 的连接。
8. WHEN `info <server> <tool>` 成功, THE MCP_CLI SHALL 输出可重新解析为 JSON Schema 的完整 Tool_Schema 表示。

### 需求 10：grep 搜索行为

**用户故事：** 作为拥有多个 MCP 服务器的用户，我希望跨服务器按 glob 搜索工具，以便快速定位能力。

#### 验收标准

1. WHEN Search_Pattern 包含单个 `*`, THE MCP_CLI SHALL 将单个 `*` 匹配为不包含 `/` 的任意字符序列。
2. WHEN Search_Pattern 包含连续 `**`, THE MCP_CLI SHALL 将连续 `**` 匹配为可包含 `/` 的任意字符序列。
3. WHEN Search_Pattern 包含 `?`, THE MCP_CLI SHALL 将 `?` 匹配为一个字符。
4. WHEN Search_Pattern 包含正则特殊字符且特殊字符不是 glob 操作符, THE MCP_CLI SHALL 按字面量匹配对应字符。
5. THE MCP_CLI SHALL 以大小写不敏感方式匹配 Search_Pattern。
6. WHEN grep 找到多个结果, THE MCP_CLI SHALL 按服务器名称和工具名称稳定排序输出结果。
7. IF grep 无法连接一个 MCP_Server, THEN THE MCP_CLI SHALL 向 stderr 输出对应警告并继续搜索其他 MCP_Server。
8. WHEN grep 未找到匹配工具, THE MCP_CLI SHALL 向 stdout 输出无结果提示并以退出码 0 结束。

### 需求 11：call 输入、执行与 JSON 输出

**用户故事：** 作为脚本或 AI 代理，我希望通过内联 JSON 或 stdin 调用工具并获得纯 JSON 结果，以便可靠接入管道。

#### 验收标准

1. WHERE call 同时存在内联 JSON 和可读 stdin, THE MCP_CLI SHALL 仅使用内联 JSON 作为调用参数。
2. WHERE call 不存在内联 JSON且 stdin 不是 TTY, THE MCP_CLI SHALL 从 stdin 读取调用参数直到 EOF。
3. WHERE call 不存在内联 JSON且 stdin 是 TTY, THE MCP_CLI SHALL 使用空 JSON object 作为调用参数。
4. WHEN call 读取到零字节或仅空白字符输入, THE MCP_CLI SHALL 使用空 JSON object 作为调用参数。
5. IF call 输入不是有效 JSON, THEN THE MCP_CLI SHALL 返回包含解析位置的 `INVALID_JSON` 错误。
6. IF call 输入是有效 JSON但顶层值不是 object, THEN THE MCP_CLI SHALL 返回 `INVALID_ARGUMENTS` 错误。
7. IF call 输入超过 Call_Input_Max_Size, THEN THE MCP_CLI SHALL 在发送 MCP 调用前返回 `INPUT_TOO_LARGE` 错误。
8. WHEN call 参数通过解析、大小和 Tool_Filter 校验, THE MCP_CLI SHALL 仅连接目标 MCP_Server并发送一次工具调用尝试。
9. WHEN MCP_Tool 调用成功, THE MCP_CLI SHALL 将完整 Tool_Result 序列化为单个有效 JSON 值并写入 stdout。
10. WHEN stdout 中的 Tool_Result JSON 被解析后重新序列化并再次解析, THE MCP_CLI SHALL 保持 Tool_Result 的 Semantic_Equivalence。
11. IF MCP_Tool 返回明确的业务执行错误, THEN THE MCP_CLI SHALL 返回退出码 2 且不把 Structured_Error 写入 stdout。
12. WHEN call 产生警告、debug 或传输诊断, THE MCP_CLI SHALL 仅将 Diagnostic_Output 写入 stderr。
13. WHEN MCP_Tool 返回 `isError=true` 且 Tool_Result 包含非空 text content, THE MCP_CLI SHALL 先以单空格合并全部 text content 并将换行与控制字符归一为单行，再通过 Redactor 处理该最终文本，最后将可见文本限制为 1024 个字符并在 `TOOL_EXECUTION_FAILED` 的 stderr 详情中优先展示；若不存在可用 text content，则显示稳定的 `isError=true` 说明。
14. WHEN MCP_Tool 返回 `isError=true`, THE MCP_CLI SHALL 复用调用前已获得的 Tool_Schema，并在 JSON 转义或大小判断前通过 Redactor 递归处理 schema 的字符串键和值：紧凑序列化不超过 8 KiB 时在 stderr 详情中完整展示，超过 8 KiB 时展示按名称排序的前 20 个顶层参数类型与 required 状态、省略数量和完整 schema 的 `info` 建议；若 schema 键因脱敏改变或冲突，或最终序列化结果仍会触发 Redactor，则不得把可能丢字段或被改写的结果称为完整 schema，而应显示安全的不可内联说明与 `info` 建议；该诊断不得额外请求 MCP_Server。

### 需求 12：结构化错误、退出码与恢复建议

**用户故事：** 作为人类用户或自动化代理，我希望错误具有稳定分类、正确流向和可操作建议，以便判断失败原因并恢复。

#### 验收标准

1. WHEN MCP_CLI 展示 Structured_Error, THE MCP_CLI SHALL 使用首行 `Error [ERROR_TYPE]: message` 格式。
2. WHERE Structured_Error 包含详情, THE MCP_CLI SHALL 使用后续 `  Details: ...` 行展示详情。
3. WHERE Structured_Error 包含恢复建议, THE MCP_CLI SHALL 使用后续 `  Suggestion: ...` 行展示建议。
4. WHEN MCP_CLI 展示 Structured_Error, THE MCP_CLI SHALL 仅向 stderr 写入 Structured_Error。
5. WHEN 命令成功, THE MCP_CLI SHALL 以退出码 0 结束。
6. WHEN 参数、配置、服务器、工具或输入验证失败, THE MCP_CLI SHALL 以退出码 1 结束。
7. WHEN MCP_Tool 业务执行失败, THE MCP_CLI SHALL 以退出码 2 结束。
8. WHEN 网络连接或 Total_Timeout_Budget 失败, THE MCP_CLI SHALL 以退出码 3 结束。
9. WHEN HTTP 401 或 403 表示认证或授权失败, THE MCP_CLI SHALL 以退出码 4 结束。
10. WHEN 顶层处理一个失败, THE MCP_CLI SHALL 恰好展示一次对应 Structured_Error。
11. WHEN 错误类型对应未知服务器、未知工具、错误命令、配置缺失、认证失败或网络失败, THE MCP_CLI SHALL 提供针对对应错误类型的恢复建议。

### 需求 13：输出流、颜色与调试信息

**用户故事：** 作为终端用户和脚本作者，我希望业务输出与诊断严格分流且颜色可预测，以便同一命令同时适用于交互和管道。

#### 验收标准

1. WHEN list、info 或 grep 产生成功业务结果, THE MCP_CLI SHALL 将人类可读文本写入 stdout。
2. WHEN call 产生成功业务结果, THE MCP_CLI SHALL 将不含诊断前后缀的完整 JSON 写入 stdout。
3. WHEN MCP_CLI 产生警告或 debug 信息, THE MCP_CLI SHALL 将带 `[mcp-cli]` 前缀的信息写入 stderr。
4. WHILE 目标输出流为 TTY 且 `NO_COLOR` 未设置, THE MCP_CLI SHALL 允许对该输出流添加 ANSI 样式。
5. WHILE 目标输出流不是 TTY 或 `NO_COLOR` 已设置, THE MCP_CLI SHALL 输出不含 ANSI 转义序列的文本。
6. WHERE `MCP_DEBUG` 已启用, THE MCP_CLI SHALL 增加 stderr 诊断且保持 stdout 业务内容和退出码不变。
7. WHERE `MCP_DEBUG` 未启用, THE MCP_CLI SHALL 抑制 debug 诊断且保留警告和 Structured_Error。

### 需求 14：并发、排序与资源关闭

**用户故事：** 作为配置多个服务器的用户，我希望批量命令受控并发且部分故障互不影响，以便获得快速、确定和完整的结果。

#### 验收标准

1. WHEN list 或 grep 同时处理多个 MCP_Server, THE MCP_CLI SHALL 将活跃服务器任务数限制为 Concurrency_Limit。
2. IF `MCP_CONCURRENCY` 不是大于零的整数, THEN THE MCP_CLI SHALL 返回标识环境变量名称的 `INVALID_RUNTIME_CONFIG` 错误。
3. WHEN list 或 grep 的一个服务器任务失败, THE MCP_CLI SHALL 保持其他已启动或待启动服务器任务可继续执行。
4. WHEN list 或 grep 收集异步结果, THE MCP_CLI SHALL 在输出前按命令定义的排序键稳定排序。
5. WHEN info 或 call 执行, THE MCP_CLI SHALL 将同时连接的 MCP_Server 数量限制为 1。
6. WHEN 命令正常结束、失败、达到 Total_Timeout_Budget 或接收取消信号, THE MCP_CLI SHALL 关闭当前命令拥有的全部 Direct_Connection 和 IPC 客户端句柄。

### 需求 15：平台行为与信号处理

**用户故事：** 作为 Linux、macOS 或 Windows 用户，我希望公开命令在支持平台上保持一致并按平台能力管理 daemon，以便跨平台使用相同脚本。

#### 验收标准

1. WHILE MCP_CLI 运行于 Linux, THE MCP_CLI SHALL 支持 direct 模式和 Unix daemon 模式的全部公开命令。
2. WHILE MCP_CLI 运行于 macOS, THE MCP_CLI SHALL 支持 direct 模式和 Unix daemon 模式的全部公开命令。
3. WHILE MCP_CLI 运行于 Windows, THE MCP_CLI SHALL 支持 direct 模式的全部公开命令且不要求 Unix socket API。
4. WHEN direct 模式的 MCP_CLI 在 Linux 或 macOS 接收 SIGINT, THE MCP_CLI SHALL 完成资源关闭并以退出码 130 结束。
5. WHEN direct 模式的 MCP_CLI 在 Linux 或 macOS 接收 SIGTERM, THE MCP_CLI SHALL 完成资源关闭并以退出码 143 结束。
6. WHEN Daemon_Worker 在 Linux 或 macOS 接收 SIGINT 或 SIGTERM, THE Daemon_Worker SHALL 在退出前删除自身 PID 文件和 socket 文件。
7. WHEN 相同配置、服务器响应和公开命令在受支持平台执行, THE MCP_CLI SHALL 保持命令语法、stdout 数据格式和 Structured_Error 格式一致。

### 需求 16：安全边界与敏感信息保护

**用户故事：** 作为包含访问令牌和本地命令的配置维护者，我希望凭据、IPC 和进程启动受到边界保护，以便降低信息泄露和本地注入风险。

#### 验收标准

1. THE MCP_CLI SHALL 对服务器名称进行编码或哈希后再生成 socket 与 PID 文件名。
2. IF 服务器名称包含路径分隔符、父目录标记或控制字符, THEN THE MCP_CLI SHALL 阻止服务器名称改变 Runtime_Directory 之外的文件路径。
3. WHEN MCP_CLI 向 Daemon_Worker 传递替换后的 Server_Configuration, THE MCP_CLI SHALL 使用 stdin 或当前用户可读的短期文件而不使用明文命令行参数。
4. WHEN 短期配置文件完成传递或 daemon 启动失败, THE MCP_CLI SHALL 删除短期配置文件。
5. THE MCP_CLI SHALL 从日志、Structured_Error、PID 元数据和 debug 输出中脱敏 Authorization、Cookie、配置 `env` 值与配置 `headers` 值。
6. IF Runtime_Directory、socket 或 PID 文件是指向非预期目标的符号链接, THEN THE MCP_CLI SHALL 拒绝使用对应路径并返回安全错误。
7. WHEN Connection_Manager 清理 daemon 进程, THE Connection_Manager SHALL 仅向能够验证属于当前用户和 MCP_CLI 的进程发送终止信号。
8. WHEN HTTP 请求失败, THE MCP_CLI SHALL 在错误详情中保留状态码和目标服务器名称且移除认证 header 值。
9. WHEN stdio MCP_Server 启动, THE MCP_CLI SHALL 将配置参数作为独立进程参数传递而不执行 Shell 展开。

### 需求 17：可测试性与兼容性门禁

**用户故事：** 作为维护者，我希望核心逻辑、进程行为和平台差异能够自动验证，以便持续保持与参考实现的功能等价。

#### 验收标准

1. THE MCP_CLI SHALL 将配置查找、环境替换、Tool_Filter、Search_Pattern、错误分类、退避计算和输出格式实现为可独立测试的确定性逻辑。
2. WHEN 测试提供固定随机源和时钟, THE MCP_CLI SHALL 产生可重复的重试延迟、超时结果和 daemon 空闲结果。
3. WHEN 测试使用 mock stdio MCP_Server, THE MCP_CLI SHALL 可验证 initialize、instructions、工具列举、工具调用、stderr 分流和安全关闭。
4. WHEN 测试使用本地 mock Streamable HTTP MCP_Server, THE MCP_CLI SHALL 可验证 headers、工具操作、Transient_Error、Non_Transient_Error 和 Total_Timeout_Budget。
5. WHILE 测试运行于 Linux 或 macOS, THE MCP_CLI SHALL 可验证 daemon 首次启动、跨 CLI 复用、Config_Hash 变化、并发 IPC、孤儿清理、空闲退出和 direct 回退。
6. WHILE 测试运行于 Windows, THE MCP_CLI SHALL 可验证全部公开命令以 direct 模式编译和运行。
7. WHEN 进程级测试执行公开命令, THE MCP_CLI SHALL 允许分别断言 stdout、stderr 和退出码。
8. WHEN 测试重复执行相同 list、info 或 grep 场景, THE MCP_CLI SHALL 产生字节级相同的无颜色业务输出。
9. WHEN 发布门禁执行 `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features` 和 `cargo build --release --all-features`, THE MCP_CLI SHALL 使四个命令均以退出码 0 完成。
10. WHEN 参考实现的配置、过滤、输出、错误、grep、client、CLI 错误或集成测试场景适用于本需求, THE MCP_CLI SHALL 通过对应的 Rust 移植测试。
