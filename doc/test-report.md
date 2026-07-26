# Test and Retest Guide: mcp-cli (Rust)

## 1. 文档目的

本文记录 `mcp-cli` Rust 实现的当前测试流程、真实 MCP 互操作测试、覆盖范围和测试结论，供后续版本回归与问题复现使用。

测试基线：

- 测试日期：2026-07-26
- 测试平台：Linux
- 工程版本：`mcp-cli 0.3.0`
- HTTP MCP：`https://mcp.amap.com/mcp?key=xxxxxx`
- stdio MCP：`npx -y 12306-mcp`
- 测试二进制：`target/release/mcp-cli`

本文的“Spec 全功能覆盖”指 `doc/requirements.md`、`doc/design.md` 和 `doc/tasks.md` 定义的 CLI、配置、传输、错误、策略及生命周期能力。远端 MCP 暴露的第三方业务工具不属于本工程实现；真实测试会覆盖工具发现以及无参数、带参数的代表性调用，但不会逐个调用全部第三方工具。

## 2. 覆盖范围

| 能力 | 自动化测试 | 真实 HTTP | 真实 stdio |
|------|------------|-----------|------------|
| `--help`、`--version`、参数解析 | 是 | 不适用 | 不适用 |
| 无子命令 `list` | 是 | 是 | 是 |
| `info SERVER` 与裸 `SERVER` | 是 | 是 | 是 |
| `info SERVER/TOOL` 与 split target | 是 | 是 | 是 |
| `grep` 命中、描述和零结果 | 是 | 是 | 是 |
| `call` slash/split target | 是 | 是 | 是 |
| inline JSON 与 stdin JSON | 是 | 是 | 是 |
| 带参数工具调用 | 是 | 是 | 是 |
| 配置发现、优先级和校验 | 是 | 是 | 是 |
| 环境变量替换与工具过滤 | 是 | 是 | 传输无关 |
| direct 模式 | 是 | 是 | 是 |
| daemon 启动、复用与空闲退出 | 是 | 是 | 是 |
| 多服务器并发和部分失败隔离 | 是 | 是 | 是 |
| 重试、超时和资源清理 | 是 | 是 | 是 |
| 结构化错误与退出码 `1`–`4` | 是 | 是 | 传输无关 |
| Authorization 脱敏 | 是 | 本地 401 fixture | 不适用 |
| SIGINT/SIGTERM | 是 | 不重复破坏性实测 | 不重复破坏性实测 |
| macOS/Windows 专属分支 | 对应平台条件测试 | 当前 Linux 未实测 | 当前 Linux 未实测 |
| 真实 TTY ANSI 颜色 | 策略测试 | 当前非 TTY 未实测 | 当前非 TTY 未实测 |

## 3. 前置条件

确认 Rust、Node.js、npm/npx 和 Python 3 可用：

```bash
rustc --version
cargo --version
node --version
npx --version
python3 --version
```

所有命令均从仓库根目录执行：

```bash
cd /home/pan/code/github/mcp-cli-rust
```

`npx -y 12306-mcp` 会从 npm 下载并执行该包的当前版本。若需要完全可重复的 stdio 回归，应先确认并记录 npm 包版本，再改用明确版本号。

## 4. 完整质量门禁

按顺序执行：

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release --all-features
```

通过标准：

- 四条命令退出码均为 `0`。
- 不允许格式差异或 Clippy warning。
- 不允许失败、忽略之外的异常测试退出。
- release 二进制存在：`target/release/mcp-cli`。

当前基线结果：

- `cargo fmt --check`：通过。
- 严格 Clippy：通过。
- `cargo test --all-features`：全部通过；其中核心库 252 个测试、主程序 5 个测试，其余 integration、process、property、stdio、HTTP、daemon 和 signal 测试均通过。
- release 构建：通过。

## 5. 准备临时配置

建议使用 `/tmp`，避免测试文件进入工作区。

### 5.1 Streamable HTTP

```bash
cat > /tmp/mcp-cli-http.json <<'JSON'
{
  "mcpServers": {
    "http-remote": {
      "url": "https://mcp.amap.com/mcp?key=xxxxxx"
    }
  }
}
JSON
```

### 5.2 stdio

```bash
cat > /tmp/mcp-cli-stdio.json <<'JSON'
{
  "mcpServers": {
    "stdio-12306": {
      "command": "npx",
      "args": ["-y", "12306-mcp"]
    }
  }
}
JSON
```

### 5.3 混合传输与故障隔离

```bash
cat > /tmp/mcp-cli-combined.json <<'JSON'
{
  "mcpServers": {
    "http-remote": {
      "url": "https://mcp.amap.com/mcp?key=xxxxxx"
    },
    "stdio-12306": {
      "command": "npx",
      "args": ["-y", "12306-mcp"]
    },
    "unreachable": {
      "url": "http://127.0.0.1:9/mcp"
    }
  }
}
JSON
```

定义复测变量：

```bash
BIN=target/release/mcp-cli
HTTP_CONFIG=/tmp/mcp-cli-http.json
STDIO_CONFIG=/tmp/mcp-cli-stdio.json
COMBINED_CONFIG=/tmp/mcp-cli-combined.json
```

## 6. 公共 CLI 基础测试

```bash
"$BIN" --version
"$BIN" --help
```

预期：

- 版本输出为 `mcp-cli 0.3.0`，或与待测版本一致。
- help 仅展示公开的 `info`、`grep`、`call` 和全局选项。
- 内部 daemon 入口不得出现在帮助中。

## 7. Streamable HTTP 真实端到端测试

为排除 daemon 状态影响，先测试 direct 模式：

```bash
export MCP_NO_DAEMON=1
export MCP_MAX_RETRIES=0
export MCP_TIMEOUT=180
export NO_COLOR=1
```

### 7.1 工具发现

```bash
"$BIN" -c "$HTTP_CONFIG"
"$BIN" -c "$HTTP_CONFIG" --with-descriptions
```

当前基线：

- 服务器名为 `http-remote`。
- 成功发现 29 个工具。
- 包括 12306、高德地图、彩云天气、音乐、电台和贴纸工具。
- `--with-descriptions` 为每个有描述的工具附加描述。

### 7.2 server info 等价形式

```bash
"$BIN" -c "$HTTP_CONFIG" http-remote
"$BIN" -c "$HTTP_CONFIG" info http-remote
```

两条命令 stdout 应完全一致。

### 7.3 tool info 等价形式

```bash
"$BIN" -c "$HTTP_CONFIG" info http-remote/12306-mcp-get-current-date
"$BIN" -c "$HTTP_CONFIG" info http-remote 12306-mcp-get-current-date
```

两条命令应输出相同的单个紧凑 JSON Schema：

```json
{"properties":{},"type":"object"}
```

### 7.4 grep

```bash
"$BIN" -c "$HTTP_CONFIG" --with-descriptions grep '12306*'
"$BIN" -c "$HTTP_CONFIG" grep 'definitely-no-such-tool-*'
```

预期：

- 第一条匹配 8 个 12306 工具并显示描述。
- 第二条退出码为 `0`，输出 `No matching tools found.`。

### 7.5 call 输入形式

```bash
"$BIN" -c "$HTTP_CONFIG" call http-remote/12306-mcp-get-current-date '{}'
"$BIN" -c "$HTTP_CONFIG" call http-remote 12306-mcp-get-current-date '{}'
printf '{}' | "$BIN" -c "$HTTP_CONFIG" call http-remote/12306-mcp-get-current-date
```

三条命令应输出等价、可被 JSON parser 重新解析的结果。当前基线：

```json
{"content":[{"text":"2026-07-26","type":"text"}]}
```

日期是动态值，复测时应等于 Asia/Shanghai 当天日期，而不是固定断言 `2026-07-26`。

### 7.6 带参数调用

```bash
"$BIN" -c "$HTTP_CONFIG" info http-remote/amap-mcp-maps_weather
"$BIN" -c "$HTTP_CONFIG" call http-remote/amap-mcp-maps_weather '{"city":"北京"}'
```

预期 Schema 要求字符串字段 `city`；调用结果为 JSON，`isError` 不得为 `true`。当前基线返回北京市四日天气预报。

## 8. stdio 真实端到端测试

继续使用 direct 模式：

```bash
export MCP_NO_DAEMON=1
export MCP_MAX_RETRIES=0
export MCP_TIMEOUT=180
export NO_COLOR=1
```

### 8.1 工具发现

```bash
"$BIN" -c "$STDIO_CONFIG"
"$BIN" -c "$STDIO_CONFIG" --with-descriptions
```

当前基线：

- 成功发现 8 个 12306 工具。
- 服务端启动日志出现在 stderr：

```text
[server] stdio-12306: 12306 MCP Server running on stdio @Joooook
```

- 启动日志不得污染 stdout 中的 JSON 或工具列表。

### 8.2 info、grep 与等价 target

```bash
"$BIN" -c "$STDIO_CONFIG" stdio-12306
"$BIN" -c "$STDIO_CONFIG" info stdio-12306
"$BIN" -c "$STDIO_CONFIG" info stdio-12306/get-current-date
"$BIN" -c "$STDIO_CONFIG" info stdio-12306 get-current-date
"$BIN" -c "$STDIO_CONFIG" --with-descriptions grep 'get-*tickets'
```

预期：

- 两种 server info 输出一致。
- 两种 tool info 输出一致。
- grep 匹配 `get-interline-tickets` 和 `get-tickets`。
- 当前 stdio 工具 Schema 包含 Draft 7 `$schema`：

```json
{"$schema":"http://json-schema.org/draft-07/schema#","properties":{},"type":"object"}
```

HTTP 聚合端点没有返回 `$schema`，这是上游暴露差异，不是 CLI 字段丢失。

### 8.3 call 输入形式与带参数调用

```bash
"$BIN" -c "$STDIO_CONFIG" call stdio-12306/get-current-date '{}'
"$BIN" -c "$STDIO_CONFIG" call stdio-12306 get-current-date '{}'
printf '{}' | "$BIN" -c "$STDIO_CONFIG" call stdio-12306/get-current-date

"$BIN" -c "$STDIO_CONFIG" info stdio-12306/get-station-code-by-names
"$BIN" -c "$STDIO_CONFIG" call stdio-12306/get-station-code-by-names \
  '{"stationNames":"北京南|上海虹桥"}'
```

当前带参数调用基线：

```json
{"content":[{"text":"{\"北京南\":{\"station_code\":\"VNP\",\"station_name\":\"北京南\"},\"上海虹桥\":{\"station_code\":\"AOH\",\"station_name\":\"上海虹桥\"}}","type":"text"}]}
```

## 9. daemon 模式测试

取消 direct 环境变量，缩短空闲超时：

```bash
unset MCP_NO_DAEMON
export MCP_DEBUG=1
export MCP_DAEMON_TIMEOUT=10
export MCP_MAX_RETRIES=0
export MCP_TIMEOUT=180
export NO_COLOR=1
```

### 9.1 HTTP daemon 启动与复用

```bash
"$BIN" -c "$HTTP_CONFIG" info http-remote/12306-mcp-get-current-date
"$BIN" -c "$HTTP_CONFIG" call http-remote/12306-mcp-get-current-date '{}'
```

### 9.2 stdio daemon 启动与复用

```bash
"$BIN" -c "$STDIO_CONFIG" info stdio-12306/get-current-date
"$BIN" -c "$STDIO_CONFIG" call stdio-12306/get-current-date '{}'
```

预期每条命令在 stderr 包含：

```text
[mcp-cli] debug: selected daemon mode
```

连续命令均应成功，第二条命令应复用 worker。

### 9.3 daemon 请求 deadline 回归

已确认并修复过一个 daemon 生命周期缺陷：worker 启动成功后，第二次命令可能输出：

```text
mcphub
  <error: Failed to communicate with server "mcphub">
```

该问题只影响 daemon 路径；相同配置使用 `MCP_NO_DAEMON=1` 时能够正常列出工具，而且失败时 `mcp-cli __daemon` 进程仍然存活。

根因是 worker bootstrap 创建的 `CommandContext` 带有最多 5 秒的启动 deadline。这个 deadline 本应只限制后端初始化、socket 发布和 ready 握手，却被继续传给长期运行的 `listTools`、`callTool` 等请求。daemon 存活超过 5 秒后，后续请求继承了已经过期的 deadline，因此立即失败；worker 的 accept/idle 循环没有退出，所以进程仍可被观察到。

修复位于 `src/daemon/worker.rs` 的 `execute_request`：

- bootstrap deadline 仍只约束 daemon 启动，不改变 5 秒快速失败语义；
- 每个 IPC 操作开始时创建新的请求级 `CommandContext`；
- 请求 deadline 使用与客户端一致的 `DAEMON_IPC_CAP`，即单次请求最多 5 秒；
- diagnostics 与 cancellation 边界继续复用，不改变脱敏、取消和连接清理行为；
- daemon 总存活时间仍由 `MCP_DAEMON_TIMEOUT` 控制，不能把 5 秒请求上限解释为 daemon 生命周期。

使用单服务器配置进行回归。等待时间必须超过 5 秒 startup cap，同时小于本节设置的 10 秒 idle timeout。第一次命令启动或连接 worker 后再采集 PID：

```bash
"$BIN" -c "$HTTP_CONFIG"
pid_before="$(pgrep -f '[m]cp-cli __daemon' | sort)"
sleep 6
"$BIN" -c "$HTTP_CONFIG"
pid_after="$(pgrep -f '[m]cp-cli __daemon' | sort)"
test "$pid_before" = "$pid_after"
```

通过标准：两次命令都成功，第二次输出中没有 `Failed to communicate with server`，且 PID 集合不变。这同时证明请求复用了原 worker，而不是重启 daemon 或回退 direct。

本次修复已通过：

```bash
cargo fmt --check
cargo test --all-features --test daemon_worker
cargo test --all-features --test daemon_linux
cargo clippy --all-targets --all-features -- -D warnings
```

其中 `daemon_worker` 和 `daemon_linux` 均为 4/4 通过；真实 debug 二进制在等待 6 秒后第二次 list 成功，daemon PID 保持不变。

完成回归后，等待超过空闲超时再检查：

```bash
sleep 12
pgrep -af '[m]cp-cli __daemon'
```

预期无输出，`pgrep` 退出码为 `1`。不得新增残留的 `npx`、`node .../12306-mcp` 或 daemon 进程。

完成后恢复：

```bash
unset MCP_DEBUG MCP_DAEMON_TIMEOUT
export MCP_NO_DAEMON=1
```

## 10. 配置发现、替换与过滤

创建环境变量配置：

```bash
cat > /tmp/mcp-cli-env-filter.json <<'JSON'
{
  "mcpServers": {
    "http-filtered": {
      "url": "${MCP_SPEC_REMOTE_URL}",
      "allowedTools": ["12306-mcp-*", "amap-mcp-maps_weather"],
      "disabledTools": ["*weather"]
    }
  }
}
JSON
```

### 10.1 `MCP_CONFIG_PATH` 与环境替换

```bash
MCP_CONFIG_PATH=/tmp/mcp-cli-env-filter.json \
MCP_SPEC_REMOTE_URL='https://mcp.amap.com/mcp?key=xxxxxx' \
MCP_NO_DAEMON=1 "$BIN"
```

预期仅显示 8 个 12306 工具，天气工具因 `disabledTools` 优先而被排除。

### 10.2 严格模式

```bash
env -u MCP_SPEC_REMOTE_URL \
  MCP_CONFIG_PATH=/tmp/mcp-cli-env-filter.json \
  MCP_NO_DAEMON=1 "$BIN"
echo $?
```

预期：`MISSING_ENV_VAR`，退出码 `1`。

### 10.3 非严格模式

```bash
env -u MCP_SPEC_REMOTE_URL \
  MCP_CONFIG_PATH=/tmp/mcp-cli-env-filter.json \
  MCP_STRICT_ENV=0 MCP_NO_DAEMON=1 "$BIN"
echo $?
```

预期先警告缺失变量被替换为空字符串，再因空 URL 返回 `INVALID_SERVER_CONFIG`，退出码 `1`。

### 10.4 配置优先级

```bash
MCP_CONFIG_PATH=/tmp/mcp-cli-combined.json \
MCP_NO_DAEMON=1 "$BIN" -c "$HTTP_CONFIG" \
  info http-remote/12306-mcp-get-current-date
```

预期 `-c` 配置优先，命令成功。

默认当前目录发现可通过临时创建 `./mcp_servers.json` 验证；测试后必须立即删除，避免影响其他命令。

## 11. 多服务器并发与部分失败

```bash
MCP_NO_DAEMON=1 MCP_CONCURRENCY=2 MCP_MAX_RETRIES=0 MCP_TIMEOUT=180 \
  "$BIN" -c "$COMBINED_CONFIG"

MCP_NO_DAEMON=1 MCP_CONCURRENCY=3 MCP_MAX_RETRIES=0 MCP_TIMEOUT=180 \
  "$BIN" -c "$COMBINED_CONFIG" grep '*current-date'
```

预期：

- HTTP 和 stdio 服务器均正常展示。
- `unreachable` 显示网络错误，但不取消其他服务器。
- list 和 grep 整体退出码为 `0`。
- grep 输出 HTTP 与 stdio 两条 current-date 命中。
- 结果按服务器和工具名称稳定排序。

## 12. 错误与退出码复测

### 12.1 客户端/配置错误：退出码 1

```bash
"$BIN" -c "$HTTP_CONFIG" info no-such-server
"$BIN" -c "$HTTP_CONFIG" info http-remote/no-such-tool
"$BIN" -c "$HTTP_CONFIG" call http-remote/12306-mcp-get-current-date '{'
"$BIN" -c "$HTTP_CONFIG" call http-remote/12306-mcp-get-current-date '[]'
MCP_TIMEOUT=0 "$BIN" -c "$HTTP_CONFIG"
```

分别预期 `SERVER_NOT_FOUND`、`TOOL_NOT_FOUND`、`INVALID_JSON`、`INVALID_ARGUMENTS`、`INVALID_RUNTIME_CONFIG`，退出码均为 `1`。

### 12.2 工具执行错误：退出码 2

```bash
MCP_NO_DAEMON=1 MCP_MAX_RETRIES=0 \
  "$BIN" -c "$HTTP_CONFIG" call http-remote/amap-mcp-maps_weather '{}'
echo $?
```

预期 `TOOL_EXECUTION_FAILED`，退出码 `2`。

### 12.3 网络和超时：退出码 3

`/tmp/mcp-cli-combined.json` 中的 `unreachable` 可用于连接拒绝测试：

```bash
MCP_NO_DAEMON=1 MCP_MAX_RETRIES=0 MCP_TIMEOUT=5 \
  "$BIN" -c "$COMBINED_CONFIG" info unreachable
echo $?
```

预期 `NETWORK_ERROR`，退出码 `3`。

超时、429/502/503/504 重试、指数退避、共享总预算和连接资源释放由以下自动化测试覆盖：

```bash
cargo test --test http_transport
cargo test --test direct_retry
cargo test --test runtime_retry
cargo test --all-features --test cli_end_to_end
```

### 12.4 认证错误：退出码 4

本轮使用本地 HTTP fixture 返回 401，并在配置中设置测试 Authorization header。结果为：

```text
Error [AUTH_ERROR]: Authentication or authorization failed for server "unauthorized"
  Details: HTTP status: 401
```

退出码为 `4`，普通和 debug 输出均未包含 header 值。后续常规复测可依赖 `http_transport`、`cli_end_to_end` 和 secret redaction property tests，避免向真实服务发送无效凭据。

### 12.5 退出码表

| 退出码 | 含义 | 已验证示例 |
|-------:|------|------------|
| `0` | 成功；批量命令可容忍单服务器失败 | list/info/grep/call、组合配置 |
| `1` | CLI、配置、JSON 或目标错误 | 缺失 env、未知 server/tool、非法 JSON |
| `2` | MCP 工具执行失败 | weather 缺少必填参数 |
| `3` | 网络或超时 | 连接拒绝、本地挂起 fixture |
| `4` | 认证或授权失败 | 本地 HTTP 401 fixture |

## 13. 输出、安全与资源检查

重点断言：

- list/info/grep 只将业务输出写入 stdout。
- call stdout 是单个紧凑 JSON 值并以换行结束。
- 错误、debug、stdio server log 只进入 stderr。
- `NO_COLOR` 存在时不输出 ANSI 控制序列。
- 配置中的 env/header secret 不出现在错误、debug、stdio stderr 或 daemon 元数据中。
- inline JSON 优先，不读取 stdin。
- direct 和 daemon 完成后连接、子进程、Unix socket 与元数据得到清理。

自动化重点测试：

```bash
cargo test --all-features --test stdio_transport
cargo test --test http_transport
cargo test --all-features --test transport_contract
cargo test --all-features --test daemon_linux
cargo test --all-features --test signals
cargo test property_36_secret_redaction
cargo test property_31_color_policy
cargo test property_34_command_owned_resources_are_eventually_closed
```

## 14. 当前测试结论

### 14.1 通过项

- Spec 定义的公开 CLI、配置、工具过滤、stdio、Streamable HTTP、direct、daemon、重试、超时、并发、输出、错误、脱敏和资源管理均有自动化覆盖。
- Linux 环境下全部质量门禁与自动化测试通过。
- 公网 Streamable HTTP 端点真实发现 29 个工具；list/info/grep/call 及输入形式全部通过。
- `npx -y 12306-mcp` 真实 stdio 发现 8 个工具；list/info/grep/call、stderr 分流和参数传输全部通过。
- HTTP 与 stdio 的 direct 和 daemon 模式均通过；daemon 连续请求复用和空闲退出正常。
- HTTP、stdio、不可达服务器混合执行时，部分失败被隔离，成功结果保留。
- 错误退出码 `1`、`2`、`3`、`4` 与 Spec 一致。
- 测试结束后未留下本轮创建的 daemon、本地 fixture、临时配置或 stdio 子进程。

### 14.2 未确认缺陷

首次探索测试中曾出现一次 HTTP daemon `NETWORK_ERROR: Failed while listing tools`。本轮 release 二进制下重新覆盖 daemon 启动、连续 info/call、复用和空闲退出均成功，无法稳定复现，因此不列为已确认缺陷。若后续再现，应记录：

- 完整命令和时间。
- `MCP_DEBUG=1` stderr。
- direct 与 daemon 是否只有 daemon 失败。
- daemon 是否复用了旧 config hash/session。
- 服务端 HTTP 状态、`Mcp-Session-Id` 生命周期和限流响应。

### 14.3 已知限制

- 当前会话是 Linux，无法实机覆盖 macOS daemon 与 Windows direct；需依赖对应平台 CI。
- 当前命令执行环境不是交互式 PTY，真实 ANSI 颜色仅由策略测试覆盖。
- 公网 29 个工具全部完成发现，但未逐个执行业务调用；本轮选取无参数日期和带参数天气工具验证 HTTP 调用链。
- stdio 8 个工具全部完成发现，本轮选取日期和车站代码工具验证空参数与带参数调用链。
- `npx -y 12306-mcp` 未固定版本，npm 上游更新可能改变工具名、Schema、描述或返回值。

## 15. 清理

```bash
rm -f \
  /tmp/mcp-cli-http.json \
  /tmp/mcp-cli-stdio.json \
  /tmp/mcp-cli-combined.json \
  /tmp/mcp-cli-env-filter.json

unset BIN HTTP_CONFIG STDIO_CONFIG COMBINED_CONFIG
unset MCP_NO_DAEMON MCP_MAX_RETRIES MCP_TIMEOUT MCP_CONCURRENCY
unset MCP_DEBUG MCP_DAEMON_TIMEOUT MCP_CONFIG_PATH MCP_STRICT_ENV
unset MCP_SPEC_REMOTE_URL NO_COLOR
```

确认没有本轮遗留 daemon：

```bash
pgrep -af '[m]cp-cli __daemon'
```

预期无输出。不要直接终止测试前已经存在、且不属于本轮启动的 `12306-mcp` 进程；应先使用 `ps -o pid,ppid,stat,etime,cmd -p <PID>` 确认来源。

## 16. 推荐复测顺序

1. 运行第 4 节完整质量门禁。
2. 创建第 5 节临时配置。
3. 运行第 7 节 HTTP direct 测试。
4. 运行第 8 节 stdio direct 测试。
5. 运行第 9 节 HTTP/stdio daemon 测试。
6. 运行第 10–12 节配置、并发和错误测试。
7. 检查 stdout/stderr、退出码与资源清理。
8. 执行第 15 节清理。
9. 将新结果与第 14 节基线比较，并记录版本、日期与差异。
