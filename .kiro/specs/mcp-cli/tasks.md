# mcp-cli（Rust）实施计划

## 概述

按 `requirements.md` 与 `design.md` 实现单一 Rust 二进制 `mcp-cli`。任务以可增量编译的依赖顺序推进：crate 脚手架 → 纯领域逻辑 → 配置 → CLI/输出 → 连接/传输 → 命令 → daemon → 安全与跨平台 → 集成与发布门禁。每个任务都要求产出可编译代码、自动化测试或构建配置；设计中的 37 个 correctness properties 各自对应一个独立的 `proptest` 测试目标。

## 任务

- [x] 1. 建立 Rust crate、模块边界与测试支撑
  - [x] 1.1 创建 Cargo 工程与可构建二进制
    - 创建 Rust 2024 edition 的 `Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`、`src/main.rs` 与 `src/lib.rs`，package/binary 均命名为 `mcp-cli`；精确锁定 rmcp 2.x、Tokio、clap、serde、serde_json、thiserror、sha2、url、reqwest、proptest、assert_cmd 等版本与所需 feature。
    - 建立 design 指定的 `config/`、`policy/`、`connection/`、`commands/`、`daemon/` 模块树；Unix 模块使用 `cfg(unix)` 隔离，Windows 初始构建不得引用 Unix API。
    - 预期产物：`cargo check --all-targets --all-features` 可通过的最小 crate 和已提交 lockfile。
    - _需求：17.9_

  - [x] 1.2 定义共享领域模型与可注入边界
    - 在 `src/lib.rs` 及领域模块中定义 `CommandSpec`、`ServerDefinition`、`TransportConfig`、`ToolInfo`、`ToolResult`、`PerServer<T>`、`CommandOutcome`、`McpConnection` 等设计接口；外部依赖通过 trait 注入，不让 rmcp 类型泄漏到命令层。
    - 预期产物：后续配置、命令、连接和输出模块可共同引用的稳定 Rust API。
    - _需求：5.6, 5.7, 17.1_

  - [x] 1.3 创建自动化测试支撑 crate 与 fixture 二进制骨架
    - 创建 `tests/support/`、`src/bin/mock_stdio_server.rs`（仅测试 feature 构建）和共享 fake clock、fixed jitter、mock connection、stdout/stderr 捕获工具；保证测试不访问公网或真实用户 daemon 目录。
    - 预期产物：单元、property、传输、daemon 和进程级测试可复用的测试 API 与可构建 fixture。
    - _需求：17.2, 17.3, 17.4, 17.7_

- [x] 2. 实现纯领域策略、运行时、错误分类与脱敏
  - [x] 2.1 实现 RuntimeConfig、Deadline 与取消上下文
    - 在 `src/runtime.rs` 严格解析 `MCP_TIMEOUT`、`MCP_CONCURRENCY`、`MCP_MAX_RETRIES`、`MCP_RETRY_DELAY`、`MCP_STRICT_ENV`、`MCP_NO_DAEMON`、`MCP_DAEMON_TIMEOUT`、`MCP_DEBUG`，实现绝对 deadline、局部 timeout cap、`Clock` 与 cancellation 注入。
    - 预期产物：无 I/O 的运行时配置解析器和可由 fake clock 驱动的命令上下文。
    - _需求：6.1, 6.2, 6.3, 7.10, 8.5, 8.6, 8.7, 8.8, 8.9, 14.2, 17.2_

  - [x] 2.2 实现重试分类、指数退避与预算执行器
    - 在 `src/policy/retry.rs` 实现 `ErrorClass`、需求规定的 errno/HTTP 状态分类、`RetryPolicy`、饱和指数退避、7500..=12500 basis-point jitter，以及在同一总 deadline 内执行 attempt 的 `retry`。
    - 预期产物：可注入时钟和随机源、可记录 attempt trace 的确定性重试组件。
    - _需求：8.1, 8.2, 8.3, 8.4, 8.5, 8.7, 17.1, 17.2_

  - [x]* 2.3 编写 Property 18：重试分类与次数
    - 在 `tests/properties/property_18_retry_count.rs` 生成结果序列与 Retry_Limit，验证非瞬态/Auth/Business 首次失败立即返回，瞬态最多执行 `1 + Retry_Limit` 次，limit 0 不重试。
    - **Property 18：重试分类与次数**
    - **Validates: Requirements 8.1, 8.2, 8.7**

  - [x]* 2.4 编写 Property 19：退避、抖动与预算边界
    - 在 `tests/properties/property_19_backoff_budget.rs` 生成 base、attempt、jitter 与剩余预算，验证 10 秒上限、75%..=125% 区间及预算不足时不 sleep/不启动下一 attempt。
    - **Property 19：退避、抖动与预算边界**
    - **Validates: Requirements 8.3, 8.4, 8.5**

  - [x]* 2.5 编写 Property 33：非法并发配置总被拒绝
    - 在 `tests/properties/property_33_invalid_concurrency.rs` 生成零、负数、非数字、溢出和尾随字符，验证返回包含变量名的 `INVALID_RUNTIME_CONFIG` 且不回退默认值。
    - **Property 33：非法并发配置总被拒绝**
    - **Validates: Requirements 14.2**

  - [x]* 2.6 编写 Property 37：固定时钟和随机源产生可重复 trace
    - 在 `tests/properties/property_37_repeatable_trace.rs` 对同一 retry/idle 输入执行两次，比较 attempt、delay、deadline transition、shutdown decision 与 diagnostics trace。
    - **Property 37：固定时钟和随机源产生可重复 trace**
    - **Validates: Requirements 17.2**

  - [x] 2.7 实现 ToolFilter 完整匹配策略
    - 在 `src/policy/tool_filter.rs` 实现大小写不敏感、锚定全名的 `*`/`?` matcher，以及 disabled 优先的 `is_allowed` 与稳定 `filter`；`?` 按 Unicode scalar 匹配。
    - 预期产物：展示与调用可共用的唯一授权判断入口。
    - _需求：4.1, 4.2, 4.3, 4.4, 4.5, 4.6, 4.7, 4.8, 4.9, 17.1_

  - [x]* 2.8 编写 Property 9：Tool_Filter glob 与授权公式
    - 在 `tests/properties/property_09_tool_filter.rs` 使用独立参考 matcher 生成工具名及 allow/disable patterns，验证授权公式与 glob 语义。
    - **Property 9：Tool_Filter glob 与授权公式**
    - **Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5, 4.6**

  - [x] 2.9 实现 SearchMatcher glob 语义
    - 在 `src/policy/search_glob.rs` 实现大小写不敏感的完整匹配：单 `*` 不跨 `/`、连续 `**` 可跨 `/`、`?` 匹配一个非 `/` Unicode scalar、其他正则字符按字面量处理。
    - 预期产物：可独立编译并复用的搜索 matcher。
    - _需求：10.1, 10.2, 10.3, 10.4, 10.5, 17.1_

  - [x]* 2.10 编写 Property 11：Search_Pattern 语义
    - 在 `tests/properties/property_11_search_pattern.rs` 生成 UTF-8 名称和 glob，与独立参考算法比较完整匹配结果。
    - **Property 11：Search_Pattern 语义**
    - **Validates: Requirements 10.1, 10.2, 10.3, 10.4, 10.5**

  - [x] 2.11 实现稳定错误模型、分类与恢复建议
    - 在 `src/error.rs` 定义全部公开 `ErrorKind`、message/details/suggestion、source 保留、重试类别和退出码映射；为未知命令、配置、server/tool、认证、网络等错误实现类型相关构造器。
    - 预期产物：所有层可返回、顶层可唯一渲染的 `CliError`。
    - _需求：1.12, 2.3, 2.5, 2.6, 3.2, 3.4, 3.7, 3.8, 3.9, 8.2, 9.5, 9.6, 11.5, 11.6, 11.7, 11.11, 12.5, 12.6, 12.7, 12.8, 12.9, 12.10, 12.11_

  - [x]* 2.12 编写 Property 29：错误类别到退出码的总映射
    - 在 `tests/properties/property_29_exit_codes.rs` 穷举公开 ErrorKind，验证 client/tool/network/auth 映射分别稳定为 1/2/3/4。
    - **Property 29：错误类别到退出码的总映射**
    - **Validates: Requirements 12.6, 12.7, 12.8, 12.9**

  - [x]* 2.13 编写 Property 30：要求恢复建议的错误具有类型相关建议
    - 在 `tests/properties/property_30_suggestions.rs` 生成相关错误上下文，验证六类错误均有非空且类型匹配的建议。
    - **Property 30：要求恢复建议的错误具有类型相关建议**
    - **Validates: Requirements 12.11**

  - [x] 2.14 实现 SecretSet、流式 Redactor 与 DiagnosticSink
    - 在 `src/policy/redact.rs` 实现 env/header/Authorization/Cookie secret 注册、跨 chunk 脱敏和安全 flush；实现 warning/debug/server stderr 的 `[mcp-cli]` 与 server 前缀策略，debug 开关不得改变业务输出。
    - 预期产物：错误、HTTP、stdio 与 daemon 可共用的无泄密诊断通道。
    - _需求：5.9, 8.8, 8.9, 11.12, 13.3, 13.6, 13.7, 16.5, 16.8_

  - [x]* 2.15 编写 Property 20：重试诊断契约
    - 在 `tests/properties/property_20_retry_diagnostics.rs` 生成 retry、错误类和 secrets，验证 debug 信息包含 attempt/class/delay、不含 secret，关闭 debug 时无该诊断。
    - **Property 20：重试诊断契约**
    - **Validates: Requirements 8.8, 8.9**

  - [x]* 2.16 补齐纯领域逻辑示例测试
    - 在 `tests/unit/runtime_retry.rs` 覆盖全部瞬态 errno/status、401/403、5029 误匹配防护、运行变量上下界和 deadline 饱和运算；运行该测试目标验证核心策略。
    - 预期产物：`cargo test --test runtime_retry` 通过。
    - _需求：8.1, 8.2, 8.6, 14.2, 17.1_

- [x] 3. 实现配置发现、替换、验证与规范化
  - [x] 3.1 实现配置路径发现与有界 JSON 读取
    - 在 `src/config/discover.rs`、`src/config/mod.rs` 实现 CLI path > `MCP_CONFIG_PATH` > cwd/home/XDG 的确定顺序；显式路径失败禁止回退，默认失败列全路径，JSON syntax 错误保留 path/line/column。
    - 预期产物：返回 `LoadedConfig` 前半管线及稳定配置读取错误。
    - _需求：2.1, 2.2, 2.3, 2.4, 2.5, 2.6, 2.7_

  - [x] 3.2 实现递归环境变量替换
    - 在 `src/config/substitute.rs` 仅替换 JSON 字符串值中的 `${VAR_NAME}`，不替换 key、不二次展开；strict 模式汇总缺失变量，non-strict 模式替换为空并逐个唯一变量告警。
    - 预期产物：纯替换函数、缺失变量集合和注册 secrets。
    - _需求：3.1, 3.2, 3.3_

  - [x]* 3.3 编写 Property 5：已定义环境变量的一次递归替换
    - 在 `tests/properties/property_05_env_substitution.rs` 生成嵌套 JSON 与完整 env map，验证字符串节点替换、其他节点不变且环境值不二次展开。
    - **Property 5：已定义环境变量的一次递归替换**
    - **Validates: Requirements 3.1**

  - [x]* 3.4 编写 Property 6：缺失环境变量策略完备且不泄密
    - 在 `tests/properties/property_06_missing_env.rs` 生成缺失引用和 secrets，验证 strict 错误与 non-strict 空串/warning 行为均不泄密。
    - **Property 6：缺失环境变量策略完备且不泄密**
    - **Validates: Requirements 3.2, 3.3**

  - [x] 3.5 实现服务器配置强类型验证
    - 在 `src/config/validate.rs` 从 `RawServerConfig` 精确验证 `mcpServers`、command/url 互斥、HTTP(S) URL、args/env/headers/filter 类型与空值；产出按名称排序的 `BTreeMap<String, ServerDefinition>`。
    - 预期产物：不可表示非法状态的 stdio/HTTP 配置模型及字段路径错误。
    - _需求：2.7, 2.8, 2.9, 3.4, 3.5, 3.6, 3.7, 3.8, 3.9_

  - [x]* 3.6 编写 Property 7：服务器配置分类与字段错误定位
    - 在 `tests/properties/property_07_server_validation.rs` 生成有效配置及单字段类型突变，验证唯一 transport 分类和 Details 字段定位。
    - **Property 7：服务器配置分类与字段错误定位**
    - **Validates: Requirements 3.5, 3.6, 3.9**

  - [x] 3.7 实现 stdio 环境合并纯函数
    - 在 `src/config/validate.rs` 或 `src/connection/direct.rs` 提供父环境与配置 env 的确定性合并，配置值覆盖父值，供进程启动唯一调用。
    - 预期产物：无 shell 展开、可独立测试的环境 map。
    - _需求：3.10, 5.1, 16.9_

  - [x]* 3.8 编写 Property 8：stdio 环境合并右侧覆盖
    - 在 `tests/properties/property_08_env_merge.rs` 生成两个环境 map，验证并集和值覆盖规则。
    - **Property 8：stdio 环境合并右侧覆盖**
    - **Validates: Requirements 3.10**

  - [x] 3.9 实现 canonical JSON、ConfigHash 与 ServerId
    - 在 `src/config/canonical.rs` 递归排序对象 key、保持数组和标量，计算完整 SHA-256 配置摘要与固定长度 lowercase hex `ServerId`。
    - 预期产物：稳定序列化、配置变更检测和安全文件名输入。
    - _需求：2.8, 2.10, 6.4, 6.5, 7.4, 7.12, 16.1, 16.2_

  - [x]* 3.10 编写 Property 4：配置规范化保留语义与顺序
    - 在 `tests/properties/property_04_canonical_config.rs` 生成有效配置与 key permutations，验证 server 排序、round trip 语义等价及相同语义同 hash。
    - **Property 4：配置规范化保留语义与顺序**
    - **Validates: Requirements 2.8, 2.10**

  - [x]* 3.11 移植配置兼容性单元测试
    - 在 `tests/unit/config.rs` 覆盖参考 `config.test.ts` 的适用场景，并补充显式不可读不回退、全部默认路径、line/column、strict/non-strict、null/array、command/url、非法字段和 Claude/VS Code/Gemini 字段。
    - 预期产物：`cargo test --test config` 通过且环境变量测试隔离/串行。
    - _需求：2.1–2.10, 3.1–3.9, 17.10_

- [x] 4. 实现 CLI 语法、Presenter 与输出流契约
  - [x] 4.1 实现 clap 元数据与兼容 CLI parser
    - 在 `src/cli.rs` 实现无参数 List、单 server Info、info/call 两种 target、`-c`、`-d`、help/version、非 UTF-8 路径保留，以及未知选项、别名、空 target、缺参、多参和歧义语法的稳定建议；隐藏 `__daemon` 不进入公开 help。
    - 预期产物：无 I/O 的 `parse_cli` 与 clap 帮助/版本入口。
    - _需求：1.1, 1.2, 1.5, 1.8, 1.9, 1.10, 1.11, 1.12_

  - [x]* 4.2 编写 Property 1：目标语法等价
    - 在 `tests/properties/property_01_target_syntax.rs` 生成合法 server/tool 与 JSON object，比较 split/slash 两种 info 和 call 的 `CommandSpec`。
    - **Property 1：目标语法等价**
    - **Validates: Requirements 1.5, 1.8**

  - [x]* 4.3 编写 Property 2：非法 CLI 语法总是产生可恢复错误
    - 在 `tests/properties/property_02_invalid_cli.rs` 生成六类非法 token，验证 client 错误、非空建议且建议只引用公开命令。
    - **Property 2：非法 CLI 语法总是产生可恢复错误**
    - **Validates: Requirements 1.12**

  - [x] 4.4 实现 Structured_Error renderer 与 exactly-once 顶层边界
    - 在 `src/output.rs` 实现固定首行及可选 Details/Suggestion 行；在 `src/main.rs` 建立唯一错误渲染、stderr 写入和退出码返回路径，内部层不得重复打印。
    - 预期产物：可注入 writer 的错误 presenter 和单一进程错误边界。
    - _需求：12.1, 12.2, 12.3, 12.4, 12.10_

  - [x]* 4.5 编写 Property 28：Structured_Error 格式
    - 在 `tests/properties/property_28_error_format.rs` 生成 ErrorKind/message/可选字段，逐字验证首行、缩进和字段出现次数。
    - **Property 28：Structured_Error 格式**
    - **Validates: Requirements 12.1, 12.2, 12.3**

  - [x] 4.6 实现 list/info/grep 纯文本 formatter 与描述开关
    - 在 `src/output.rs` 为 server snapshot、tool entry、grep hit、部分失败和零结果实现稳定无颜色格式；描述仅由 `with_descriptions` 控制。
    - 预期产物：不依赖异步完成顺序的业务文本 formatter。
    - _需求：1.11, 9.1, 9.2, 9.3, 10.6, 10.8, 13.1_

  - [x]* 4.7 编写 Property 3：描述开关只控制描述
    - 在 `tests/properties/property_03_descriptions.rs` 生成含/不含描述的条目，验证开关不改变集合、排序或退出码。
    - **Property 3：描述开关只控制描述**
    - **Validates: Requirements 1.11**

  - [x] 4.8 实现 JSON Schema 与完整 ToolResult formatter
    - 在 `src/output.rs` 使用 serde_json 将 info tool schema 与 call 完整结果输出为单个 JSON 值加结尾换行，不提取 text content、不添加诊断前后缀。
    - 预期产物：可直接重新解析的 schema/result stdout bytes。
    - _需求：9.8, 11.9, 11.10, 13.2_

  - [x]* 4.9 编写 Property 23：Tool_Schema 输出 round trip
    - 在 `tests/properties/property_23_schema_roundtrip.rs` 生成 JSON Schema Value，验证完整 stdout 可解析且语义等价。
    - **Property 23：Tool_Schema 输出 round trip**
    - **Validates: Requirements 9.8**

  - [x]* 4.10 编写 Property 26：Tool_Result 完整 JSON round trip
    - 在 `tests/properties/property_26_result_roundtrip.rs` 生成任意 serde_json Value（含扩展字段），验证输出仅一个 JSON 值且双重 round trip 语义等价。
    - **Property 26：Tool_Result 完整 JSON round trip**
    - **Validates: Requirements 11.9, 11.10**

  - [x] 4.11 实现按流 TTY/NO_COLOR 样式与诊断分流
    - 在 `src/output.rs` 为 stdout/stderr 分别计算 style policy；颜色只修饰语义片段，warning/debug/server stderr 只进入 stderr，call stdout 保持纯 JSON。
    - 预期产物：可注入 TTY 状态的双流 writer。
    - _需求：11.12, 13.3, 13.4, 13.5, 13.6, 13.7_

  - [x]* 4.12 编写 Property 27：诊断不污染业务结果
    - 在 `tests/properties/property_27_diagnostic_isolation.rs` 生成 outcome 与诊断事件排列，验证 stdout/退出码不变及 stderr 前缀/debug 抑制规则。
    - **Property 27：诊断不污染业务结果**
    - **Validates: Requirements 11.12, 13.3, 13.6, 13.7**

  - [x]* 4.13 编写 Property 31：颜色策略 truth table
    - 在 `tests/properties/property_31_color_policy.rs` 遍历 TTY/NO_COLOR 组合，验证仅允许组合含 ANSI，去色后的语义文本一致。
    - **Property 31：颜色策略 truth table**
    - **Validates: Requirements 13.4, 13.5**

  - [x]* 4.14 移植 CLI、错误和输出进程测试
    - 在 `tests/process/cli_syntax.rs` 与 `tests/unit/output.rs` 移植适用的 `cli-errors.test.ts`、`errors.test.ts`、`output.test.ts`，用 `assert_cmd` 分别断言 stdout、stderr、status、help/version 和 NO_COLOR。
    - 预期产物：解析/输出兼容测试目标通过。
    - _需求：1.9, 1.10, 1.12, 12.1–12.11, 13.1–13.7, 17.7, 17.10_

- [x] 5. 实现统一连接接口、stdio/HTTP direct 传输与资源关闭
  - [x] 5.1 实现 DirectConnector 与连接资源注册表
    - 在 `src/connection/mod.rs`、`src/connection/direct.rs` 定义 direct-only 获取路径、连接所有权与 RAII/显式 close 协议；每个操作接收同一 `CommandContext`，info/call 上限为一个 server。
    - 预期产物：命令可使用 mock 或真实 adapter、所有终止路径可观测资源归零的连接层。
    - _需求：5.8, 6.2, 6.3, 14.5, 14.6_

  - [x] 5.2 实现 rmcp stdio adapter
    - 在 `src/connection/rmcp_adapter.rs` 用 `tokio::process::Command::new(command).args(args)` 启动子进程，设置 cwd 和合并环境，完成 initialize/initialized、分页 tools、instructions、call 与 close；stderr 经 Redactor 带 server 前缀转发，宽限期后 kill+wait。
    - 预期产物：实现 `McpConnection` 的 stdio direct 连接且不经过 shell。
    - _需求：3.10, 5.1, 5.2, 5.3, 5.6, 5.7, 5.8, 5.9, 16.9_

  - [x]* 5.3 完成 mock stdio MCP server 与传输集成测试
    - 在 `src/bin/mock_stdio_server.rs` 和 `tests/integration/stdio_transport.rs` 实现可脚本化 fixture，验证 argv/cwd/env、initialize 顺序、instructions、分页 tools、完整结果、业务错误、stderr 分流、忽略 close 后 kill+wait。
    - 预期产物：`cargo test --test stdio_transport --all-features` 通过且无遗留 child。
    - _需求：5.1–5.3, 5.6–5.9, 11.9, 11.11, 17.3_

  - [x] 5.4 实现 rmcp Streamable HTTP adapter
    - 在 `src/connection/rmcp_adapter.rs` 用 loopback/远程 URL 构建 Streamable HTTP transport，附加 headers，覆盖 POST/GET/SSE 生命周期；错误仅保留 server/status 并脱敏认证信息。
    - 预期产物：与 stdio 共享域接口的 HTTP direct 连接。
    - _需求：5.4, 5.5, 5.6, 5.7, 5.8, 16.8_

  - [x]* 5.5 完成本地 mock Streamable HTTP server 与传输测试
    - 在 `tests/support/mock_http.rs` 和 `tests/integration/http_transport.rs` 实现随机 loopback 端口 fixture，捕获 headers 并脚本化 401/403/429/502/503/504、延迟、断流和成功响应。
    - 预期产物：不访问公网的 HTTP initialize/tools/call/headers/error/timeout 测试目标。
    - _需求：5.4–5.8, 8.1, 8.2, 8.6, 12.8, 12.9, 16.8, 17.4_

  - [x] 5.6 将重试与总 deadline 接入 direct 操作
    - 在 `src/connection/direct.rs` 对 connect/list/call 应用 RetryExecutor 和总预算；每 attempt 恰调用一次操作，非瞬态/Auth/Business 不重试，timeout 取消等待及未完成请求。
    - 预期产物：stdio/HTTP 共用的重试、debug 诊断和退出错误映射。
    - _需求：8.1–8.9, 11.8, 12.7, 12.8, 12.9_

  - [x] 5.7 实现 direct 正常/错误/超时/取消清理
    - 在 `src/connection/direct.rs` 和 `src/runtime.rs` 将 close、pipe、child、HTTP session 注册到命令资源表；主错误不被 close 错误覆盖，取消后执行有界 cleanup。
    - 预期产物：四类终止路径均无 direct/stdio/IPC 客户端泄漏。
    - _需求：5.8, 8.6, 14.6, 15.4, 15.5_

  - [x]* 5.8 编写 Property 34：命令拥有资源最终关闭
    - 在 `tests/properties/property_34_resource_cleanup.rs` 生成正常、typed failure、deadline、cancellation 终止路径，验证 resource registry 归零且不计 daemon worker 后端连接。
    - **Property 34：命令拥有资源最终关闭**
    - **Validates: Requirements 14.6**

  - [x]* 5.9 参数化验证两类传输的一致连接契约
    - 在 `tests/integration/transport_contract.rs` 对 stdio 与 HTTP fixture 运行同一 list/instructions/call/close 用例，并断言初始化完成后才允许操作、结果域模型一致。
    - 预期产物：统一 `McpConnection` 契约测试目标通过。
    - _需求：5.6, 5.7, 5.8, 17.3, 17.4_

- [x] 6. 实现 list/info/grep/call 命令并接入 direct 执行路径
  - [x] 6.1 实现有界批处理执行器
    - 在 `src/commands/mod.rs` 实现按 server 名创建任务、Semaphore 限流、每任务恰启动一次、失败隔离和结果收集；所有任务使用同一 command deadline。
    - 预期产物：list/grep 可复用、可注入 schedule 的 batch executor。
    - _需求：14.1, 14.3, 14.4_

  - [x]* 6.2 编写 Property 32：有界并发与失败隔离
    - 在 `tests/properties/property_32_bounded_concurrency.rs` 生成任务延迟、limit 和失败掩码，验证 peak、启动次数及其他任务完成。
    - **Property 32：有界并发与失败隔离**
    - **Validates: Requirements 14.1, 14.3**

  - [x] 6.3 实现 list handler
    - 在 `src/commands/list.rs` 连接全部 server、获取并过滤工具，单 server 失败转为可读项，保留全部成功并按 server/tool 排序；接入无子命令分发。
    - 预期产物：direct 模式下可执行的 list 业务路径。
    - _需求：1.1, 4.7, 9.1, 9.2, 9.3, 9.4, 14.1, 14.3, 14.4_

  - [x] 6.4 实现 info handler
    - 在 `src/commands/info.rs` 只连接目标 server，输出 transport、instructions、过滤后工具及参数；tool 视图从获准工具中查找并输出完整 schema，未知 server/tool 给出候选建议。
    - 预期产物：三种 info 语法共享同一执行路径且连接上限为 1。
    - _需求：1.2, 1.3, 1.4, 1.5, 4.7, 9.5, 9.6, 9.7, 9.8, 14.5_

  - [x] 6.5 实现 grep handler
    - 在 `src/commands/grep.rs` 预编译 SearchMatcher，并发获取过滤后的工具，连接失败写 warning 后继续，命中按 `(server, tool)` 排序，零结果成功输出提示。
    - 预期产物：direct 模式下跨 server 的确定性 grep。
    - _需求：1.6, 4.7, 10.1–10.8, 14.1, 14.3, 14.4_

  - [x]* 6.6 编写 Property 12：grep 是过滤与搜索的精确组合
    - 在 `tests/properties/property_12_grep_composition.rs` 生成 server tools、filter 和 pattern，验证命中集合严格等于先授权再搜索。
    - **Property 12：grep 是过滤与搜索的精确组合**
    - **Validates: Requirements 1.6**

  - [x] 6.7 实现 call 输入读取与验证
    - 在 `src/commands/call.rs` 实现 inline 优先、非 TTY stdin 流式读取、TTY/空白归一化 `{}`、16 MiB+1 探测、JSON 位置错误和顶层 object 校验；所有校验在连接前完成。
    - 预期产物：返回 `JsonObject` 的可独立测试输入组件。
    - _需求：11.1, 11.2, 11.3, 11.4, 11.5, 11.6, 11.7_

  - [x]* 6.8 编写 Property 24：call 输入源选择与空白归一化
    - 在 `tests/properties/property_24_call_input_source.rs` 生成 inline/stdin/whitespace，验证 inline 不读取 stdin，空白归一化为空 object。
    - **Property 24：call 输入源选择与空白归一化**
    - **Validates: Requirements 11.1, 11.4**

  - [x]* 6.9 编写 Property 25：call 只接受顶层 object
    - 在 `tests/properties/property_25_call_object.rs` 生成非 object JSON 和 object，验证拒绝类型与 object key/value 语义保持。
    - **Property 25：call 只接受顶层 object**
    - **Validates: Requirements 11.6**

  - [x] 6.10 实现 call handler 与工具授权前置检查
    - 在 `src/commands/call.rs` 验证 server/tool/filter 后仅连接目标 server；每 retry attempt 只发送一次 call，完整 ToolResult 交 Presenter，业务错误映射退出码 2，诊断只进 stderr。
    - 预期产物：inline/stdin 两种 direct call 的纯 JSON stdout 路径。
    - _需求：1.7, 1.8, 4.8, 11.8, 11.9, 11.11, 11.12, 14.5_

  - [x]* 6.11 编写 Property 10：展示过滤与调用授权同源
    - 在 `tests/properties/property_10_filter_authorization.rs` 生成工具集合与 filter，验证展示为 `is_allowed` 稳定子序列；拒绝 call 返回 `TOOL_DISABLED` 且 mock transport 调用计数为零。
    - **Property 10：展示过滤与调用授权同源**
    - **Validates: Requirements 4.7, 4.8, 4.9**

  - [x]* 6.12 编写 Property 21：批处理输出与完成顺序无关
    - 在 `tests/properties/property_21_order_independence.rs` 生成 list/grep 结果 permutations，验证无颜色 stdout 字节相同及规定排序。
    - **Property 21：批处理输出与完成顺序无关**
    - **Validates: Requirements 9.1, 9.2, 10.6, 14.4, 17.8**

  - [x]* 6.13 编写 Property 22：部分失败保留全部成功
    - 在 `tests/properties/property_22_partial_failure.rs` 生成 Success 集合和 Failure 掩码，验证 list 成功集合不被删除、改写或取消。
    - **Property 22：部分失败保留全部成功**
    - **Validates: Requirements 9.4**

  - [x] 6.14 完成 dispatcher 与四命令 direct 接线
    - 在 `src/commands/mod.rs`、`src/main.rs` 将配置加载、运行时上下文、ConnectionManager、四个 handler、Presenter 和唯一错误边界串联；正常、失败、timeout、取消均显式关闭命令资源。
    - 预期产物：`MCP_NO_DAEMON=1` 下四个公开命令可由真实二进制执行。
    - _需求：1.1–1.12, 12.4–12.10, 13.1–13.7, 14.5, 14.6_

  - [x]* 6.15 移植命令单元与 direct 进程级测试
    - 在 `tests/process/direct_cli.rs`、`tests/unit/commands.rs` 移植适用的 grep/call/integration 场景，覆盖描述、排序、部分失败、零结果、未知/禁用工具、stdin/inline、输入边界、业务错误、network/timeout/auth 与 `call | jq` 等价 JSON 解析。
    - 预期产物：分别可断言 stdout、stderr、status 的 direct 全命令测试通过。
    - _需求：1.1–1.12, 9.1–9.8, 10.1–10.8, 11.1–11.12, 12.5–12.9, 17.7, 17.8, 17.10_

- [x] 7. 实现 Unix daemon 路径、IPC、worker 与生命周期
  - [x] 7.1 实现安全 DaemonPaths 与 PID MetadataStore
    - 在 `src/daemon/paths.rs`、`src/daemon/metadata.rs` 创建 `${TMPDIR:-/tmp}/mcp-cli-<uid>/`、hash basename、socket/pid/lock 路径；校验 owner/type/symlink，设置目录 0700，PID 临时文件 0600 并原子 rename，元数据只含 pid/config_hash/started_at。
    - 预期产物：仅在 Unix 编译的安全路径与原子元数据 API。
    - _需求：7.2, 7.3, 7.4, 16.1, 16.2, 16.6_

  - [x]* 7.2 编写 Property 35：daemon 路径恒定受限
    - 在 `tests/properties/property_35_daemon_paths.rs` 生成含分隔符、`..`、控制字符的 Unicode server 名，验证 ServerId grammar、canonical parent 与 basename 不泄漏原名。
    - **Property 35：daemon 路径恒定受限**
    - **Validates: Requirements 16.1, 16.2**

  - [x] 7.3 实现 NDJSON codec 与 IPC serde 模型
    - 在 `src/daemon/codec.rs`、`src/daemon/mod.rs` 实现 request/response、受限 ID、camelCase operation、互斥 outcome、CRLF、拆/粘包、EOF 截断和 1 MiB 请求/响应硬限制。
    - 预期产物：增量 `NdjsonCodec` 与稳定 IPC error code。
    - _需求：7.5, 7.6, 7.7, 7.8_

  - [x]* 7.4 编写 Property 14：NDJSON 任意分块 round trip
    - 在 `tests/properties/property_14_ndjson_chunks.rs` 生成合法请求序列及任意非空 chunk partition，验证解码序列逐项相等。
    - **Property 14：NDJSON 任意分块 round trip**
    - **Validates: Requirements 7.6**

  - [x] 7.5 实现 daemon worker IPC 服务
    - 在 `src/daemon/worker.rs` 为每 client 顺序处理 ping/listTools/callTool/getInstructions/close，不同 client 并发；无效 JSON/缺 ID/未知 type 返回稳定错误并保持 worker 可服务，超大帧仅关闭该 client。
    - 预期产物：通过统一 `McpConnection` 驱动的 Unix worker RPC 循环。
    - _需求：7.1, 7.5, 7.6, 7.7, 7.8_

  - [x]* 7.6 编写 Property 15：IPC 关联与错误后可服务性
    - 在 `tests/properties/property_15_ipc_correlation.rs` 生成合法 ID/operation 与非法帧，验证响应 ID、稳定错误及随后 ping 成功。
    - **Property 15：IPC 关联与错误后可服务性**
    - **Validates: Requirements 7.5, 7.7**

  - [x] 7.7 实现有效请求驱动的 idle deadline
    - 在 `src/daemon/worker.rs` 只在成功解析并完成有效 IPC 请求后更新 `last_valid_request`，使用注入时钟触发 Daemon_Idle_Timeout 并关闭后端。
    - 预期产物：无效帧/I/O 噪声不延寿的 worker 状态机。
    - _需求：7.9, 7.10, 17.2_

  - [x]* 7.8 编写 Property 16：只有有效请求延长 daemon 生命周期
    - 在 `tests/properties/property_16_idle_deadline.rs` 生成 fake-clock 事件序列，验证 idle deadline 等于最近有效请求时间加 timeout。
    - **Property 16：只有有效请求延长 daemon 生命周期**
    - **Validates: Requirements 7.9**

  - [x] 7.9 实现幂等 worker shutdown 与信号清理
    - 在 `src/daemon/worker.rs` 统一 close、idle、SIGINT、SIGTERM 触发：停止 accept、取消/等待 clients、关闭 MCP、按安全检查删除 socket/PID/lock；每项副作用至多一次。
    - 预期产物：可注入 shutdown hooks 的 `shutdown_once`。
    - _需求：7.10, 7.11, 15.6_

  - [x]* 7.10 编写 Property 17：daemon 关闭幂等
    - 在 `tests/properties/property_17_shutdown_idempotent.rs` 生成非空关闭触发序列，验证 Closed 最终态及 close/unlink/release 各至多一次。
    - **Property 17：daemon 关闭幂等**
    - **Validates: Requirements 7.11**

  - [x] 7.11 实现隐藏 worker 入口、配置 stdin 传输与原子 ready
    - 在 `src/main.rs`、`src/daemon/worker.rs` 通过当前 executable 的隐藏 `__daemon` 启动；替换后的配置仅从 stdin（或 0600 create_new 临时文件 fallback）传递，MCP 初始化、socket bind、PID 原子发布完成后才经匿名 pipe ready。
    - 预期产物：配置不进入 argv/environment，失败路径删除短期文件和未发布资源的 DaemonSpawner。
    - _需求：7.1, 7.12, 7.13, 16.3, 16.4_

  - [x]* 7.12 编写 Unix worker/IPC 进程级测试
    - 在 `tests/integration/daemon_worker.rs` 覆盖 runtime/PID 权限、每 server 独立 worker、并发 client、拆粘包、1 MiB 边界、invalid UTF-8/EOF、原子 ready 故障注入、idle、close、SIGINT/SIGTERM 和文件清理。
    - 预期产物：Linux/macOS 上 worker 全生命周期测试通过且无遗留进程/文件。
    - _需求：7.1–7.13, 15.6, 17.5_

- [x] 8. 实现 daemon client、连接管理、安全边界与平台差异
  - [x] 8.1 实现 DaemonClient 与 5 秒 IPC 上限
    - 在 `src/daemon/client.rs` 实现 Unix socket、请求 ID、NDJSON framing、响应关联和 `min(5s, remaining)` timeout；请求失败先取消并关闭 stream，再向 manager 返回可回退错误，避免 call 双发。
    - 预期产物：实现 `McpConnection` 的 daemon IPC adapter，其 close 只关闭当前 IPC client。
    - _需求：6.6, 6.7, 6.9, 7.5, 7.8_

  - [x] 8.2 实现 Unix ConnectionManager 的复用、变更与 direct 回退
    - 在 `src/connection/manager.rs` 校验 metadata、UID、进程、hash、socket、ping；处理首次 spawn、已有复用、死 PID/缺 socket/陈旧 hash、5 秒 ready/ping/request 失败，并仅对 operational failure 回退 direct；Windows/`MCP_NO_DAEMON=1` 始终 direct 且不创建运行文件。
    - 预期产物：四命令统一使用、命令层无模式分支的 ConnectionManager。
    - _需求：6.1–6.10, 7.12, 15.1, 15.2, 15.3_

  - [x]* 8.3 编写 Property 13：direct 与 daemon 的可观察等价性
    - 在 `tests/properties/property_13_mode_equivalence.rs` 用返回相同域值/typed error 的内存 adapters 执行命令，验证 stdout 字节和退出码相同，mode 仅影响 debug。
    - **Property 13：direct 与 daemon 的可观察等价性**
    - **Validates: Requirements 6.10**

  - [x] 8.4 实现进程归属校验、安全清理与 fail-closed 策略
    - 在 `src/daemon/metadata.rs`、`src/connection/manager.rs` 为 Linux `/proc` 与 macOS 平台 API 实现 UID/start time/executable/worker identity 验证；拒绝 symlink/错误 owner/越界路径，只有已验证进程可 SIGTERM，安全错误禁止 direct 回退。
    - 预期产物：孤儿清理、配置变更终止和恶意路径均受安全验证约束。
    - _需求：6.5, 6.8, 7.12, 16.6, 16.7_

  - [x]* 8.5 编写 Property 36：敏感信息跨通道脱敏且保留安全上下文
    - 在 `tests/properties/property_36_secret_redaction.rs` 生成 env/header secrets、日志、错误、metadata 与 HTTP status/server，验证所有可见/持久化结果无 secret，status/server 保留且 PID key set 固定。
    - **Property 36：敏感信息跨通道脱敏且保留安全上下文**
    - **Validates: Requirements 7.4, 16.5, 16.8**

  - [x]* 8.6 编写 Linux daemon 复用与安全集成测试
    - 在 `tests/integration/daemon_linux.rs` 使用隔离 TMPDIR 验证首次启动/跨 CLI 复用、两 server 隔离、hash 变化、并发 IPC、dead PID、missing socket、ping/request timeout、direct fallback、安全错误不 fallback、symlink/无关 PID 不被 kill，以及 `/proc/<pid>/cmdline` 无 secret。
    - 预期产物：Linux daemon/direct 全场景进程测试通过。
    - _需求：6.1–6.10, 7.1–7.13, 15.1, 16.1–16.8, 17.5_

  - [x]* 8.7 编写 macOS daemon 复用与安全集成测试
    - 在 `tests/integration/daemon_macos.rs` 复用平台无关 daemon 套件并验证 macOS 进程查询、权限、信号、hash 变化、回退和无 secret argv。
    - 预期产物：macOS daemon/direct 生命周期测试通过。
    - _需求：6.1–6.10, 7.1–7.13, 15.2, 15.6, 16.3–16.7, 17.5_

  - [x]* 8.8 编写 Windows direct-only 编译与进程测试
    - 在 `tests/process/windows_direct.rs` 构建并运行 list/info/grep/call，验证不引用/要求 Unix socket、不创建 daemon 文件、公开语法和输出格式与其他平台一致。
    - 预期产物：Windows stable 上 direct-only 全命令测试通过。
    - _需求：6.3, 15.3, 15.7, 17.6_

  - [x]* 8.9 编写 Unix direct 与 daemon 信号进程测试
    - 在 `tests/process/signals.rs` 向 direct CLI 发送 SIGINT/SIGTERM，断言资源关闭与 130/143；向 worker 发送信号，断言 PID/socket 删除且 mock child 退出。
    - 预期产物：Linux/macOS 信号测试通过且无 zombie。
    - _需求：15.4, 15.5, 15.6_

- [x] 9. 完成进程级兼容套件、跨平台 CI 与发布构建门禁
  - [x] 9.1 建立完整进程级行为矩阵
    - 在 `tests/process/cli_end_to_end.rs` 用 mock stdio/HTTP server 对 list/info/grep/call、两种 target、inline/stdin、描述、部分失败、retry、timeout、auth、debug、NO_COLOR、过滤和完整 ToolResult 分别断言 stdout、stderr、exit code；重复 list/info/grep 比较字节级输出。
    - 预期产物：不依赖公网、覆盖 direct 与 Unix daemon 的公开行为回归套件。
    - _需求：1.1–1.12, 5.1–5.9, 6.10, 8.1–8.9, 9.1–9.8, 10.1–10.8, 11.1–11.12, 12.1–12.11, 13.1–13.7, 17.3–17.8_

  - [x]* 9.2 移植参考实现适用测试并建立 Rust 测试目标
    - 将 `config`、`filter`、`output`、`errors`、`grep`、`client`、`cli-errors`、`integration/cli` 的适用场景补入现有 Rust 单元/集成/进程测试；对与需求冲突之处以完整 ToolResult、安全 daemon 配置传输等需求行为编写回归断言。
    - 预期产物：参考场景均可通过明确的 `cargo test --test <target>` 自动执行，无遗漏测试模块。
    - _需求：17.10_

  - [x] 9.3 配置 Linux、macOS、Windows CI 构建与测试矩阵
    - 创建 CI workflow：三平台 stable 执行 direct 测试；Linux/macOS 额外执行 daemon/信号测试，Windows 执行 direct-only；隔离 TMPDIR、串行化信号 case、保存 proptest seed/counterexample。
    - 预期产物：三平台自动构建与测试配置，平台条件编译错误能在提交阶段被阻止。
    - _需求：15.1, 15.2, 15.3, 15.7, 17.5, 17.6, 17.9_

  - [x] 9.4 接入并通过发布质量门禁
    - 在 CI/release job 中依次执行 `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo test --all-features`、`cargo build --release --all-features`；修复所有格式、lint、测试和 release 构建失败，不绕过警告。
    - 预期产物：四条命令均以退出码 0 完成，生成单一 release `mcp-cli`（Windows 为 `.exe`）。
    - _需求：17.9_

## 说明

- 标记 `*` 的子任务是可选测试任务；37 个 correctness properties 均独立成项、独立测试文件，并应至少运行 100 cases。
- 实现任务均要求将代码接入当前主路径，不保留未使用模块或孤立接口。
- 测试只使用本地 mock stdio/HTTP server、隔离运行目录和可注入时钟/随机源，不依赖公网服务。
- `requirements.md` 是行为准绳；参考 TypeScript 场景仅在不冲突时移植。

## Task Dependency Graph

```json
{
  "waves": [
    { "id": 0, "tasks": ["1.1"] },
    { "id": 1, "tasks": ["1.2", "1.3"] },
    { "id": 2, "tasks": ["2.1", "2.7", "2.9", "2.11", "2.14"] },
    { "id": 3, "tasks": ["2.2", "2.5", "2.8", "2.10", "2.12", "2.13", "3.1"] },
    { "id": 4, "tasks": ["2.3", "2.4", "2.15", "2.16", "3.2", "3.5", "4.1"] },
    { "id": 5, "tasks": ["2.6", "3.3", "3.4", "3.6", "3.7", "3.9", "4.2", "4.3", "4.4"] },
    { "id": 6, "tasks": ["3.8", "3.10", "3.11", "4.5", "4.6", "5.1"] },
    { "id": 7, "tasks": ["4.7", "4.8", "5.2"] },
    { "id": 8, "tasks": ["4.9", "4.10", "4.11", "5.3", "5.4"] },
    { "id": 9, "tasks": ["4.12", "4.13", "5.5", "5.6"] },
    { "id": 10, "tasks": ["4.14", "5.7", "5.9"] },
    { "id": 11, "tasks": ["5.8", "6.1", "6.7"] },
    { "id": 12, "tasks": ["6.2", "6.3", "6.4", "6.5", "6.8", "6.9"] },
    { "id": 13, "tasks": ["6.6", "6.10", "6.12", "6.13"] },
    { "id": 14, "tasks": ["6.11", "6.14"] },
    { "id": 15, "tasks": ["6.15", "7.1", "7.3"] },
    { "id": 16, "tasks": ["7.2", "7.4", "7.5"] },
    { "id": 17, "tasks": ["7.6", "7.7"] },
    { "id": 18, "tasks": ["7.8", "7.9"] },
    { "id": 19, "tasks": ["7.10", "7.11"] },
    { "id": 20, "tasks": ["7.12", "8.1", "8.4"] },
    { "id": 21, "tasks": ["8.2", "8.5"] },
    { "id": 22, "tasks": ["8.3", "8.6", "8.7", "8.8", "8.9"] },
    { "id": 23, "tasks": ["9.1"] },
    { "id": 24, "tasks": ["9.2"] },
    { "id": 25, "tasks": ["9.3"] },
    { "id": 26, "tasks": ["9.4"] }
  ]
}
```
