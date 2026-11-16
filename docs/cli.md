# VD CLI 与 MCP

`vd-cli.exe`（macOS/Linux 为 `vd-cli`）通过鉴权的本机接口控制已运行的 VD。主程序持有数据库、账号与下载引擎。CLI 不启动主程序，也不直接打开任务库。

先在桌面设置中启用 Agent/自动化访问并授权下载目录，再调用 `health` 检查能力与限制。CLI 和 MCP 共用宿主权限；GUI 已解锁隐私任务也不会授予它们访问权。取消本机访问会阻止后续调用，已受理任务仍由桌面管理。此接口以当前操作系统用户为信任边界，不提供同用户下不同 Agent 的强隔离。

## 命令

```powershell
vd-cli.exe help --lang zh-CN
vd-cli.exe version
vd-cli.exe health --json
vd-cli.exe add "https://example.com/file.zip" --directory "C:\Downloads\中文 目录" --request-id download-001 --json
vd-cli.exe list --status downloading --query file --json
vd-cli.exe list --parent-id DIRECTORY_TASK_ID --limit 50 --json
vd-cli.exe list --parent-id DIRECTORY_TASK_ID --limit 50 --cursor NEXT_CURSOR --json
vd-cli.exe status TASK_ID --json
vd-cli.exe pause TASK_ID --json
vd-cli.exe resume TASK_ID --json
vd-cli.exe cancel TASK_ID --json
vd-cli.exe wait TASK_ID --timeout 300 --json
vd-cli.exe result TASK_ID --json
```

所有命令输出一条 JSON 结果。`--json` 可省略。stdout 只包含结果，stderr 留给诊断。`--lang en|zh-CN` 选择错误和帮助文案；错误码与字段名不会随语言变化。

第一批 `add` 仅接受 HTTP/HTTPS 单文件下载，必须指定绝对保存目录和请求 ID。请求 ID 使用 1–128 个 ASCII 字母、数字或 `-_.:`。重试时复用同一请求 ID 与参数；不同参数复用 ID 返回 `REQUEST_CONFLICT`。不同 ID 提交已经存在的来源返回 `DUPLICATE`。这两类冲突含义不同。URL 用户名/密码不受支持。

`list/status/pause/resume/cancel/wait` 可用于现有普通、视频、网盘和目录任务，控制操作经过主程序共享分派。隐私任务始终不可供 CLI 访问，即使 GUI 已解锁；针对不可见任务与不存在任务统一返回 `NOT_FOUND`。

`list` 默认只列顶层任务；`--parent-id` 列指定目录的直接子任务。默认每页 50，`--limit` 为 1–100；将 `data.next_cursor` 原样传入下一次调用，并保持筛选参数一致。任务返回 `child_count`，不会递归展开全部子任务。游标为空表示没有下一页。

`result` 返回宿主确认的最终产物 `data.files[]`，包含 `path`、`size_bytes`、`exists` 和可选校验值 `sha256`。仅返回授权目录内的非隐私任务结果；不会读取文件内容或执行下载。未完成时返回 `TASK_NOT_READY`，应稍后查询状态。完成后文件被移动或删除时 `exists:false`。不要根据原 URL、目录和显示名称猜测最终文件名。

`wait` 轮询状态，下载完成且无需等待清理时成功；任务失败、取消或回收时以非零退出码结束。`--timeout` 为 1–86400 的整数秒，默认 `wait` 300 秒、其他命令 30 秒。超时或 Ctrl-C 只结束客户端等待，不取消任务。对于 `add` 的超时或中断，主程序可能已接收请求，应使用相同请求 ID 重试查询结果。

## 输出与退出码

```json
{"ok":true,"data":{"id":"TASK_ID","status":"downloading"}}
```

```json
{"ok":false,"error":{"code":"HOST_UNAVAILABLE","message":"Unable to connect to the local VD Agent service. Confirm VD is running, enable 'Allow local Agent access' in Settings > Advanced, and check the discovery file configuration.","retryable":false,"next_action":"inspect_desktop"}}
```

以上成功示例省略了其余任务字段。任务字段是专用安全投影，不包含下载 URL、账号凭据或引擎令牌。`list` 结果为 `data.tasks` 数组及 `next_cursor`；`status/add` 为 `data` 中的任务。`wait` 因任务失败或取消结束时也携带任务投影。时间字段为 Unix epoch 秒，长度为 bytes、速度为 bytes/second。`ok:true` 表示本次调用成功；例如 status 可以成功查到一个 failed 任务，add 成功也不代表文件下载完成。

错误另含 `retryable`，并可含 `next_action` 和 `retry_after_ms`。只按稳定错误码判断，不解析中英文消息。需要启动、启用访问、登录或修改授权时先完成对应动作，不无限重试。

| 退出码 | 错误码 |
| --- | --- |
| 0 | 成功 |
| 1 | `INTERNAL_ERROR`、`PROTOCOL_ERROR`、`ENGINE_UNAVAILABLE`、`ACTION_FAILED`、`TASK_NOT_READY`、`BUSY`、`CLI_RECOVERY_REQUIRED` |
| 2 | `INVALID_ARGUMENT`、`UNSUPPORTED` |
| 3 | `HOST_UNAVAILABLE` |
| 4 | `UNAUTHORIZED`、`FORBIDDEN`、`HOST_DISABLED`、`AUTH_REQUIRED` |
| 5 | `NOT_FOUND` |
| 6 | `REQUEST_CONFLICT`、`DUPLICATE` |
| 7 | `TIMEOUT` |
| 8 | `TASK_FAILED`、`TASK_CANCELLED` |
| 130 | `INTERRUPTED` |

## 发现与隔离实例

Windows 默认发现文件是 `%APPDATA%\com.versadown.desktop\cli\connection.json`。使用 `--discovery PATH` 或 `VD_CLI_DISCOVERY` 可显式选择实例；命令行优先。发现文件含本机接口令牌，不应分享、打印或加入版本控制。

`HOST_UNAVAILABLE`（退出码 3，`next_action: inspect_desktop`）表示无法发现或连接本机 Agent 服务，不能据此断定 VD 未启动。即使 VD 和 MCP 进程都在运行，未启用 Agent 调用也可能没有发现文件。请确认 VD 已启动，在「设置 → 高级」中启用「允许本机 Agent 调用」并授权下载目录，再检查 `--discovery` / `VD_CLI_DISCOVERY` 是否指向该实例的发现文件。`HOST_DISABLED` 仅表示明确收到 Agent 调用已关闭的响应，对应退出码 4 和 `enable_desktop_agent`；客户端不会通过读取桌面设置猜测授权状态。完成检查后重新调用 `health`。

接口协议版本为 1：客户端对 `http://127.0.0.1:PORT/v1/call` 发出 JSON POST 并使用 Bearer 鉴权。客户端禁用代理和重定向。主程序仅监听回环地址，并拒绝浏览器来源的调用。CLI 不支持远端宿主或自动拉起主程序。

每次调用重新读取发现文件；鉴权失败时只在文件已更换的情况下重试一次。下载提交的超时/断连不会自动重试，因为宿主可能已经接受该请求。调用方用原 request_id 重试。CLI 和 MCP 共享同一本机 API 客户端。

## MCP

`vd-mcp-server.exe` 是独立 stdio MCP 适配器，由支持 MCP 的 Agent 客户端管理其进程。它连接桌面宿主，不启动另一套下载引擎。关闭 MCP 会话不取消下载。stdout 仅输出 MCP JSON-RPC；诊断以及 `--help` / `--version` 信息输出到 stderr。

采用 `mcpServers` 格式的客户端可使用下列配置；替换为实际程序路径，具体配置文件位置以所用客户端版本为准：

```json
{
  "mcpServers": {
    "versadown": {
      "command": "C:\\Program Files\\VersaDown\\vd-mcp-server.exe",
      "args": ["--lang", "zh-CN"]
    }
  }
}
```

测试实例可追加 `--discovery PATH`，不要把 token 放到 Agent 配置。服务使用官方 [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)，精确依赖 `rmcp 3.9.0` / `schemars 1.2.3`，传输和协议协商由 SDK 完成；VD 本机 API v1 与 MCP 协议版本互相独立。

| 工具 | 用途 |
| --- | --- |
| `vd_health` | 查询宿主、能力和限制 |
| `vd_download_add` | HTTP/HTTPS 单文件新建，重试复用 request_id |
| `vd_task_list` | 分页查询，支持 parent_id、cursor、limit |
| `vd_task_status` | 查询当前状态 |
| `vd_task_pause` | 根据用户意图暂停 |
| `vd_task_resume` | 根据用户意图继续，并由宿主检查权限 |
| `vd_task_cancel` | 根据用户停止意图取消，临时数据按宿主规则处理 |
| `vd_task_result` | 查询完成文件的授权路径与存在性 |

工具通过 `structuredContent` 返回同一业务 envelope，并在文本 content 中提供 JSON。`isError = !ok`，查询失败任务仍可为 `isError:false`。未知工具属于协议错误；已知工具的参数/业务失败返回可读业务错误。工具 outputSchema 基于共享 DTO；注解不代替宿主授权。首期没有 MCP wait，Agent 应每 1–5 秒查询 status，不把会话结束当作 cancel。上传、云端修改、网页视频与分享新建暂不开放。

Windows Quick 构建在相同产物目录收集 `versadown.exe`、`vd-cli.exe` 和 `vd-mcp-server.exe`。应使用配套版本；本说明不代表安装包及 macOS/Linux 已经实机验收。

## English

Start VD, enable Agent access in desktop settings and authorize output directories before invoking `vd-cli`. Every CLI command emits one JSON envelope to stdout. `--json` is optional; `--lang en` selects English messages. Use `help` for the command grammar. Both adapters share the same host authorization; neither starts a new download engine.

`add URL --directory ABSOLUTE_PATH --request-id ID` submits an HTTP/HTTPS single-file download. Retry with the same request ID and arguments. Reusing an ID for different arguments yields `REQUEST_CONFLICT`; a different ID for an existing download source yields `DUPLICATE`. Private tasks are always excluded, regardless of the desktop privacy unlock state.

`wait ID --timeout SECONDS` waits for completion, including pending cleanup. Timeout and Ctrl-C end only the client operation; they never cancel the task. An interrupted `add` may still have been accepted, so retry using its original request ID. Discovery can be overridden with `--discovery PATH` or `VD_CLI_DISCOVERY`. Keep the discovery file private because it contains the local authentication token. The exit-code table above is language-independent.

`HOST_UNAVAILABLE` (exit code 3, `next_action: inspect_desktop`) means the local Agent service could not be discovered or reached; it does not prove VD is stopped. Even with VD and MCP processes running, disabled Agent access can leave no discovery file. Confirm VD is running, enable **Allow local Agent access** in **Settings > Advanced**, authorize output directories, and check that any `--discovery` / `VD_CLI_DISCOVERY` override points to that instance. `HOST_DISABLED` is reserved for an explicit disabled-access response (exit code 4, `enable_desktop_agent`); the client does not read desktop settings to infer authorization. Retry `health` after these checks.

`list` is paginated (default 50, maximum 100). Use `--parent-id ID` for direct children and pass the returned `next_cursor` with unchanged filters. `result ID` returns authorized final file paths and existence; `TASK_NOT_READY` means check status later. A successful status query can describe a failed download. Never equate `ok:true` with completed file transfer.

Configure `vd-mcp-server` as a local stdio MCP server, optionally with `--discovery PATH` and `--lang en`. Its eight tools are listed above. MCP stdout contains only protocol messages; help and diagnostics use stderr. Results include both structuredContent and JSON text, with isError equal to !ok. Poll status every 1–5 seconds, then obtain result; closing the MCP session never requests task cancellation. SDK and local API versions are separate. Windows Quick outputs all three executables; cross-platform release acceptance is recorded separately.
