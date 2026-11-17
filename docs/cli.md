# VD CLI 与 MCP

`vd-cli.exe`（macOS/Linux 为 `vd-cli`）通过鉴权的本机接口调用 VD。默认实例未运行时，CLI / MCP 会按需启动同目录的 `versadown --agent-host`，等待本机 API 就绪。后台宿主持有数据库、账号与下载引擎；客户端不直接打开任务库。本文描述当前开发分支行为，已生成的旧版程序需要重新配套构建才能获得自动启动能力。

首次在桌面「设置 → 高级」中启用「允许本机 Agent 调用」并授权下载目录，保存后再调用 `health` 检查能力与限制。之后无需预先打开主界面。后台启动不创建主界面、悬浮窗或托盘；用户正常打开 VD 时，在同一宿主中创建桌面界面，继续使用同一批任务。CLI 和 MCP 共用宿主权限；GUI 已解锁隐私任务也不会授予它们访问权。取消本机访问会阻止后续调用，已受理任务仍由宿主管理。此接口以当前操作系统用户为信任边界，不提供同用户下不同 Agent 的强隔离。

后台模式复用 Tauri 和现有下载运行时，仍需要受支持的桌面运行环境；它不是面向无图形环境服务器的独立 Headless 核心。普通请求不会因网盘验证失败反复弹窗；收到 `AUTH_REQUIRED` 后，显式调用 `cloud auth-begin` 打开 VD 授权流程，用户完成交互后再继续原操作。

## 命令

```powershell
vd-cli.exe help --lang zh-CN
vd-cli.exe version
vd-cli.exe doctor --lang zh-CN
vd-cli.exe mcp-config --lang zh-CN
vd-cli.exe health --json
vd-cli.exe health --no-start --json
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
vd-cli.exe batch-add --file requests.json
vd-cli.exe prepare --file resource.json
vd-cli.exe confirm --file selection.json
```

所有命令输出一条 JSON 结果。`--json` 可省略。stdout 只包含结果，stderr 留给诊断。`--lang en|zh-CN` 选择错误和帮助文案；错误码与字段名不会随语言变化。`--no-start` 禁止自动启动，只连接已有宿主；`help`、`version` 始终不启动宿主。

`add` 接受 HTTP/HTTPS/FTP 单文件下载，必须指定绝对保存目录和请求 ID。请求 ID 使用 1–128 个 ASCII 字母、数字或 `-_.:`。重试时复用同一请求 ID 与参数；不同参数复用 ID 返回 `REQUEST_CONFLICT`。不同 ID 提交已经存在的来源返回 `DUPLICATE`。这两类冲突含义不同。URL 用户名/密码、fragment 和覆盖选项不受支持。

## 诊断、配置与批量

`doctor` 只读检查配套程序、发现文件和已有宿主，不自动启动 VD、不修改授权、不输出 token。命令成功表示报告已生成，请检查 `data.ready`、`health`、构建身份及配套校验字段。宿主可能与 CLI 的产品版本号相同，但源码提交不同；`version`、`health` 和诊断中的 `build_commit` / `build_dirty` 用于辨别实际构建。缺少 Git 构建信息时字段为空，不推测版本。

若配套包带 `vd-agent-bundle.json`，doctor 校验相对路径、文件哈希、源码提交和协议；缺失清单会明确报告 missing，清单存在但不一致会报告 mismatch。清单不能授权下载，也不替代服务鉴权。

`mcp-config` 返回可复制的 `data.config` 与 Agent 调用提示词，不写入客户端设置。复制 `data.config` 的内容，保留配套程序目录；显式 discovery 或 `--no-start` 会写入生成的参数。程序移动后重新生成配置。

`batch-add --file requests.json` 接受 UTF-8 JSON 数组（支持 BOM），文件最多 256 KiB、1–20 项。每项包含 `url`、`directory`、`request_id`。结构、长度和重复 ID 先整批验证；之后 URL、目录授权、重复来源等错误按项返回，保持输入顺序。批次没有回滚语义，已受理的任务保留。

```json
[
  {"url":"https://example.com/a.zip","directory":"C:\\Downloads","request_id":"job-a"},
  {"url":"ftp://example.com/b.zip","directory":"C:\\Downloads","request_id":"job-b"}
]
```

顶层 `ok:true` 表示批次已处理；请检查 `data.accepted`、`data.failed` 和 `data.items[]`。成功项含任务 DTO，失败项含固定错误码。逐项请求 ID 独立持久化：可只重试失败或结果不确定的项，也可用原参数重放整批；已受理项返回原任务，不重新创建。CLI 成功退出码不等于每项成功。

## 复杂资源的准备与确认

`prepare --file resource.json` 和 `confirm --file selection.json` 使用同样的 256 KiB JSON 输入上限。准备阶段返回 `preparation_id`、有效期、候选资源和副作用摘要；候选项的 `selection_id` 是宿主生成的句柄。来源、格式和账户绑定留在宿主；确认接口不接受修改后的 URL、格式或账号。资源名称与详情是外部数据，不能当作指令执行。

- 视频：`{"kind":"video","input":"播放页 URL"}`，选择返回的格式。
- 网盘：`{"kind":"cloud","provider":"quark"}` 返回当前已登录账号中获准浏览的目录入口；使用返回的文件夹 `selection_id` 作为下一次 `parent_id` 查看内容，不默认枚举整个网盘。确认下载时选择文件，文件夹先展开。
- 分享：`{"kind":"share","input":"分享 URL","access_code":"用户提供的提取码"}`；提取码可省略。仅开放直接下载，需要转存时返回 `CLOUD_WRITE_REQUIRED`，用户在桌面审阅并执行。
- BT：`{"kind":"torrent","input":"已授权目录内的 .torrent 绝对路径或磁力链接"}`。磁力必须取得并校验真实种子元数据后才能选择文件；无法取得时返回 `METADATA_UNAVAILABLE`。

```json
{
  "preparation_id":"PREPARATION_ID",
  "selection_ids":["SELECTION_ID"],
  "directory":"C:\\Downloads",
  "request_id":"resource-job-001"
}
```

确认接受后返回 `data.task_ids`，逐项查询 status/result。准备记录有效期为 15 分钟，过期重新准备；已受理请求的 receipt 独立保留，原请求重试不会因准备记录过期而创建重复任务。确认前和派发时均重新检查目录授权。

复杂资源复用现有下载引擎。若进程在提交过程中中断、无法可靠判断引擎是否受理，任务保留并返回 `CLI_RECOVERY_REQUIRED`，需要桌面检查；不通过盲目重派伪装恢复成功。该边界与 HTTP 的固定引擎 ID 恢复区分记录。准备元数据可能访问服务或读取已授权种子文件，但不启动文件内容下载，不授权云端写入。

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
| 1 | `INTERNAL_ERROR`、`PROTOCOL_ERROR`、`ENGINE_UNAVAILABLE`、`ACTION_FAILED`、`TASK_NOT_READY`、`BUSY`、`CLI_RECOVERY_REQUIRED`、`METADATA_UNAVAILABLE` |
| 2 | `INVALID_ARGUMENT`、`UNSUPPORTED`、`RESOURCE_LIMIT` |
| 3 | `HOST_UNAVAILABLE` |
| 4 | `UNAUTHORIZED`、`FORBIDDEN`、`HOST_DISABLED`、`AUTH_REQUIRED`、`CLOUD_WRITE_REQUIRED`、`ACCESS_CODE_REQUIRED` |
| 5 | `NOT_FOUND`、`PREPARATION_EXPIRED` |
| 6 | `REQUEST_CONFLICT`、`DUPLICATE` |
| 7 | `TIMEOUT` |
| 8 | `TASK_FAILED`、`TASK_CANCELLED` |
| 130 | `INTERRUPTED` |

## 发现与隔离实例

Windows 默认发现文件是 `%APPDATA%\com.versadown.desktop\cli\connection.json`。使用 `--discovery PATH` 或 `VD_CLI_DISCOVERY` 可显式选择实例；命令行优先。显式指定发现文件时始终只连接已有实例，不会回退到默认实例或自动启动另一个宿主。发现文件含本机接口令牌，不应分享、打印或加入版本控制。

`HOST_UNAVAILABLE`（退出码 3，`next_action: inspect_desktop`）表示无法发现或连接本机 Agent 服务，不能据此断定 VD 未启动。即使 VD 和 MCP 进程都在运行，未启用 Agent 调用也可能没有发现文件。请确认 VD 已启动，在「设置 → 高级」中启用「允许本机 Agent 调用」并授权下载目录，再检查 `--discovery` / `VD_CLI_DISCOVERY` 是否指向该实例的发现文件。`HOST_DISABLED` 仅表示明确收到 Agent 调用已关闭的响应，对应退出码 4 和 `enable_desktop_agent`；客户端不会通过读取桌面设置猜测授权状态。完成检查后重新调用 `health`。

默认模式先以 `health` 探测宿主，确认不可达后按需启动。启动等待上限为 20 秒，CLI 的整体 `--timeout` 可以更早结束等待；启动超时不会取消已经受理的下载。缺少配套宿主或启动失败返回 `HOST_UNAVAILABLE`；宿主明确拒绝未启用的授权时返回 `HOST_DISABLED`；超时返回 `TIMEOUT`。客户端不会读取桌面配置或替用户授权。

`health.ok` 表示本机 API 可用，下载引擎可能仍在初始化，应同时检查 `data.engine_available`。自动启动不会把引擎状态改写为成功；任务提交成功仍仅表示已受理。

多个 CLI / MCP 客户端共用启动锁，宿主在打开任务库和引擎前取得同一 profile 的独占锁。CLI 结束、MCP 正常断开或进程退出不会请求停止宿主；用户在桌面明确退出 VD 则停止宿主，下一次允许自动启动的调用会重新启动它。操作系统或第三方客户端强制结束整个进程树属于外部终止，不承诺下载继续。

接口协议版本为 1：客户端对 `http://127.0.0.1:PORT/v1/call` 发出 JSON POST 并使用 Bearer 鉴权。客户端禁用代理和重定向。主程序仅监听回环地址，并拒绝浏览器来源的调用。CLI 不支持远端宿主。

每次调用重新读取发现文件；鉴权失败时只在文件已更换的情况下重试一次。下载提交的超时/断连不会自动重试，因为宿主可能已经接受该请求。调用方用原 request_id 重试。CLI 和 MCP 共享同一本机 API 客户端。

## MCP

`vd-mcp-server.exe` 是独立 stdio MCP 适配器，由支持 MCP 的 Agent 客户端管理其进程。它按需连接或启动共享 VD 宿主，不启动另一套下载引擎。握手和工具列表不会启动宿主，第一次工具调用才按需连接。关闭 MCP 会话不取消下载。stdout 仅输出 MCP JSON-RPC；诊断以及 `--help` / `--version` 信息输出到 stderr。

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
| `vd_download_add` | HTTP/HTTPS/FTP 单文件新建，重试复用 request_id |
| `vd_download_batch_add` | 1–20 项独立 HTTP/HTTPS/FTP 新建，逐项结果与幂等 |
| `vd_resource_prepare` | 视频、网盘、分享、BT 的资源准备与选择 |
| `vd_resource_confirm` | 确认已准备的选择，返回持久任务 ID |
| `vd_task_list` | 分页查询，支持 parent_id、cursor、limit |
| `vd_task_status` | 查询当前状态 |
| `vd_task_pause` | 根据用户意图暂停 |
| `vd_task_resume` | 根据用户意图继续，并由宿主检查权限 |
| `vd_task_cancel` | 根据用户停止意图取消，临时数据按宿主规则处理 |
| `vd_task_result` | 查询完成文件的授权路径与存在性 |
| `vd_cloud_accounts` | 查询脱敏账号、授权状态、网盘支持能力与 Agent 权限 |
| `vd_cloud_auth_begin` | 显式发起 VD 网盘登录/验证，不接收密码或令牌 |
| `vd_cloud_auth_status` | 查询授权会话，检查是否成功及账号是否匹配 |
| `vd_cloud_auth_cancel` | 取消本次授权等待，不退出已有账号 |
| `vd_cloud_prepare` | 预览上传或针对云端已有文件创建分享，不写入云端 |
| `vd_cloud_confirm` | 用稳定 request_id 确认已准备的云端操作 |
| `vd_cloud_status` | 查询云端操作进度、逐项失败和真实结果 |
| `vd_cloud_cancel` | 按 supported_actions 取消操作，不隐式删除云端文件 |

19 个工具通过 `structuredContent` 返回同一业务 envelope，并在文本 content 中提供 JSON。`isError = !ok`，查询失败任务、部分失败的批次仍可为 `isError:false`。未知工具属于协议错误；已知工具的参数/业务失败返回可读业务错误。工具 outputSchema 基于共享 DTO；注解不代替宿主授权。MCP 没有 wait，Agent 应每 1–5 秒查询 status，不把会话结束当作 cancel。上传和创建分享使用独立的云端操作权限与准备/确认流程；已有分享链接下载仍不会自动转存或删除云端文件。

Windows Quick 构建在相同产物目录收集 `versadown.exe`、`vd-cli.exe` 和 `vd-mcp-server.exe`。应使用配套版本；本说明不代表安装包及 macOS/Linux 已经实机验收。

FTP 当前使用匿名登录，不接收 URL 内嵌凭据，也不经全局 HTTP 代理。BT 当前支持 v1 种子、最多 100 个文件和安全 UTF-8 路径；磁力依赖用户配置的元数据服务返回可校验的真实种子。确认时指定的授权目录下会为 BT 创建独立任务子目录，避免覆盖其他任务文件。视频还需要可用的 yt-dlp、FFmpeg 与适用的 JavaScript 运行时；分发时应保留完整 `binaries/` 和配套工具。

## 网盘账号授权、上传与创建分享

账号连接与 Agent 操作授权是两件事。先在 VD 设置中授予上传源目录的读取权限，以及目标网盘账号、目录和所需云端操作的权限。已有下载目录许可不自动包含读取本地文件上传或创建分享；Agent 不能通过工具扩大自己的权限。

```powershell
vd-cli.exe cloud accounts --provider quark --json
vd-cli.exe cloud auth-begin --file login.json --json
vd-cli.exe cloud auth-status AUTHORIZATION_ID --json
vd-cli.exe cloud auth-cancel AUTHORIZATION_ID --json
vd-cli.exe cloud prepare --file cloud-operation.json --json
vd-cli.exe cloud confirm --file cloud-confirm.json --json
vd-cli.exe cloud status OPERATION_ID --json
vd-cli.exe cloud cancel OPERATION_ID --json
```

`cloud accounts` 不弹登录窗口，返回 `authorization_state`、脱敏 `account_ref`、网盘 `capabilities` 与 Agent 的 `permitted_operations`，支持功能不等于已授权执行。`login.json` 内容为 `{"provider":"quark"}`；重新连接原账号时追加返回的 `account_ref`。不接受密码、验证码、Cookie 或 Token 字段。`auth-begin` 显式打开或聚焦 VD 授权窗口，用户在其中完成登录；按返回的 `authorization_id`、`poll_interval_ms` 和 `expires_at` 查询，不能用窗口已打开来判断登录成功。

授权状态包括 `pending`、`succeeded`、`account_mismatch`、`cancelled`、`closed`、`expired`、`failed`。查询 `ok:true` 仅代表查询成功，只有 `succeeded` 且账号匹配后才能继续。取消/关闭/过期不自动重开；账号变化需重新选择文件与准备操作。取消授权不等于退出已有账号。准备记录过期则重新准备；已受理的传输仍按原 ID 查询，不能重新创建。

上传前用 `prepare` / `vd_resource_prepare` 浏览目标网盘，取得服务器返回的目标目录 `selection_id`，再准备云端上传。例如 `cloud-operation.json`：

```json
{"input":{"operation":"upload","destination_id":"FOLDER_SELECTION_ID","paths":["C:\\Users\\example\\Documents\\report.pdf"]}}
```

本地文件必须位于授权上传源范围内。本阶段每次接受 1–20 个普通文件，目录上传返回 `UNSUPPORTED`，不隐式展开目录。返回的 `items`、`effects`、账号和网盘是待执行内容；此时未上传。检查后提交 `cloud-confirm.json`，使用固定请求 ID：

```json
{"preparation_id":"CLOUD_PREPARATION_ID","request_id":"upload-report-001"}
```

确认返回 `data.id`，用 `cloud status` 查询。检查 `status`、逐项 `failures` 和 `error`；受理不是成功完成。上传成功的 `files[].selection_id` 可继续用于创建分享。控制动作以 `supported_actions` 为准，不能假定上传一定支持暂停/续传；取消不删除已完成云端文件。

**创建网盘分享**只接受已登录账号内已有且有权分享的文件/目录选择，不接受本地路径。本地文件先上传成功；已有云端文件可从资源浏览返回值选择。例如：

```json
{"input":{"operation":"create_share","selection_ids":["CLOUD_FILE_SELECTION_ID"],"name":"课程资料","expires_in_days":7,"pass_code":"aB12","generate_pass_code":false}}
```

`expires_in_days` 必须明确填写，`0` 表示请求永久有效，网盘可能拒绝不支持的策略。指定 `pass_code` 与 `generate_pass_code:true` 不能同时使用；自动生成提取码也取决于网盘支持情况。最终分享链接、提取码和有效期来自 `cloud status` 的真实 `share` 结果，不能从预览推测。`share.expires_at` 为网盘返回的时间字符串或 `null`，不假设它与授权/准备记录的 Unix 秒时间戳同格式。

本阶段沿现有驱动校验策略：夸克、UC、阿里不支持自动生成码；PikPak 创建时可生成码或不用码，但不能指定自定义码；迅雷必须请求服务端生成码，不能指定自定义码或无提取码策略；百度必须生成或指定提取码。UC 有效期仅支持永久、1、7、30、60、180 天。参数预校验不替代网盘真实服务的最终验证。

**已有分享链接解析与下载**继续使用 `prepare` / `confirm` 的 `kind:share`，与创建分享不同。是否要求账号、提取码或转存由对应网盘和链接决定；本阶段需转存仍返回 `CLOUD_WRITE_REQUIRED`，交由桌面审阅，不以创建分享权限代替转存授权。

上传/分享确认重试必须复用 `preparation_id`、`request_id` 和相同参数。`CLOUD_RESULT_UNCERTAIN` 表示云端受理结果需要核对，不能更换请求 ID 来盲目重放。`CLOUD_PERMISSION_REQUIRED` 要求用户调整操作授权，`ACCOUNT_MISMATCH` 要求确认账号，`FILE_CONFLICT` 不自动覆盖同名文件，`SOURCE_CHANGED` 要求重新检查源文件并准备。MCP 对应工具参数与上述文件相同，额外的 `action` 字段被拒绝。

## English

The capability branch adds FTP file submission, `batch-add --file`, and `prepare/confirm --file` for complex resources. The MCP adapter exposes 19 tools, including eight cloud account/authorization/operation tools. Batch input is a UTF-8 JSON array of 1–20 independent `{url,directory,request_id}` items (256 KiB maximum file size). Top-level success means processing finished: inspect `accepted`, `failed`, and every item. Retry with the original IDs and parameters; successful acceptance is durable and is not duplicated.

`doctor` is read-only and never starts VD. Inspect `data.ready`, build identity, health, and optional bundle hash verification separately from the command's success. `mcp-config` returns a copyable configuration in `data.config` and an Agent prompt; it never edits client settings. Build metadata identifies different source revisions even when product versions match.

Use `prepare` for video pages, cloud browsing, direct-download shares and verified torrent metadata. Cloud browsing begins with `kind:cloud`, a provider and empty input, returning authorized directory entry selections rather than listing the entire drive. Pass a returned folder selection as `parent_id` to browse its contents. Confirm uses only `preparation_id`, `selection_ids`, an authorized directory, and a stable `request_id`; it returns `task_ids`. Previews expire after 15 minutes, but accepted receipts remain retryable. Preparation does not start file content downloads or grant cloud write access. Shares requiring a cloud save return `CLOUD_WRITE_REQUIRED` for desktop review. An uncertain resource submission after a crash requires review (`CLI_RECOVERY_REQUIRED`) instead of blind resubmission. These additions require matching binaries and separate real-service acceptance evidence.

Enable Agent access once in VD's desktop settings and authorize output directories. The CLI and MCP then start the default host on demand using the sibling `versadown --agent-host` executable. The background host creates no main window, launcher or tray; opening VD normally creates its desktop in that same process. This is the current development branch behavior and requires matching rebuilt binaries. It reuses Tauri and the existing runtime, not a standalone server Headless core; a supported desktop environment is still required. Interactive login requires an explicit `cloud auth-begin` request; ordinary queries never repeatedly open login windows.

Every CLI command emits one JSON envelope to stdout. `--json` is optional; `--lang en` selects English messages. Use `help` for the command grammar. Both adapters share the same host authorization and download engine. `--no-start`, `--discovery PATH`, or `VD_CLI_DISCOVERY` selects connect-only behavior. Help/version and MCP initialization/tool listing never start the host. Startup waits up to 20 seconds, bounded further by the CLI operation timeout. The client probes health before startup and never replays an already-sent business operation after a transport failure.

A profile startup lock coordinates clients; a separate lifetime lock gives one host ownership of its task database and engine. Normal client exit or MCP disconnect leaves the host running. Explicitly quitting VD stops it; a later automatic-start call can start it again. External termination of an entire process tree is outside this guarantee.

Health success means the local API is ready. The engine may still be initializing; inspect `data.engine_available` separately. Successful submission means acceptance, not completion.

`add URL --directory ABSOLUTE_PATH --request-id ID` submits an HTTP/HTTPS/FTP single-file download. Retry with the same request ID and arguments. Reusing an ID for different arguments yields `REQUEST_CONFLICT`; a different ID for an existing download source yields `DUPLICATE`. Private tasks are always excluded, regardless of the desktop privacy unlock state. FTP is anonymous and does not use the global HTTP proxy. BT currently supports v1 metadata, at most 100 files and safe UTF-8 paths; magnets require a configured metadata service to return verified torrent bytes. BT outputs use an isolated task subdirectory. Preserve the complete bundle, including `binaries/` and video tools.

`wait ID --timeout SECONDS` waits for completion, including pending cleanup. Timeout and Ctrl-C end only the client operation; they never cancel the task. An interrupted `add` may still have been accepted, so retry using its original request ID. Discovery can be overridden with `--discovery PATH` or `VD_CLI_DISCOVERY`. Keep the discovery file private because it contains the local authentication token. The exit-code table above is language-independent.

`HOST_UNAVAILABLE` (exit code 3, `next_action: inspect_desktop`) means the local Agent service could not be discovered or reached; it does not prove VD is stopped. Even with VD and MCP processes running, disabled Agent access can leave no discovery file. Confirm VD is running, enable **Allow local Agent access** in **Settings > Advanced**, authorize output directories, and check that any `--discovery` / `VD_CLI_DISCOVERY` override points to that instance. `HOST_DISABLED` is reserved for an explicit disabled-access response (exit code 4, `enable_desktop_agent`); the client does not read desktop settings to infer authorization. Retry `health` after these checks.

`list` is paginated (default 50, maximum 100). Use `--parent-id ID` for direct children and pass the returned `next_cursor` with unchanged filters. `result ID` returns authorized final file paths and existence; `TASK_NOT_READY` means check status later. A successful status query can describe a failed download. Never equate `ok:true` with completed file transfer.

Configure `vd-mcp-server` as a local stdio MCP server, optionally with `--discovery PATH` and `--lang en`. Its nineteen tools are listed above. MCP stdout contains only protocol messages; help and diagnostics use stderr. Results include both structuredContent and JSON text, with isError equal to !ok. Poll status every 1–5 seconds, then obtain result; closing the MCP session never requests task cancellation. SDK and local API versions are separate. Windows Quick outputs all three executables; cross-platform release acceptance is recorded separately.

Cloud login and Agent operation permissions are separate. Query `cloud accounts [--provider NAME]`, then explicitly start `cloud auth-begin --file login.json` if user interaction is needed. The file contains `provider` and optional opaque `account_ref`; never send passwords, verification codes, cookies or tokens. Poll `cloud auth-status AUTHORIZATION_ID` using its interval and deadline. Only `succeeded` with the expected account permits continuing; `ok:true` alone only means the status query succeeded. Cancellation, window closure, expiry, failure and account mismatch are terminal. `cloud auth-cancel` does not log out an existing account. The same commands map to `vd_cloud_accounts`, `vd_cloud_auth_begin/status/cancel` in MCP.

Use `cloud prepare --file` / `vd_cloud_prepare` with the JSON examples above. Uploads take an opaque destination folder selection and 1–20 authorized local regular files; local directory uploads return `UNSUPPORTED` in this stage. Creating a share takes selections of existing cloud files/folders, an explicit expiry (`0` requests permanent access), and a provider-supported pass-code strategy. Local files must be uploaded first. After reviewing the returned effects, `cloud confirm --file` / `vd_cloud_confirm` accepts only `preparation_id` and stable `request_id`; poll `cloud status OPERATION_ID` / `vd_cloud_status` for actual files/share results and per-item failures. `share.expires_at` preserves the provider's time string or null, unlike the Unix-second authorization/preparation deadlines. Never add an `action` discriminator to tool/file arguments. Cancellation uses `cloud cancel` / `vd_cloud_cancel` only if listed in `supported_actions`, and does not delete completed cloud files or roll back created shares.

Reuse identical confirmation parameters on retries. `CLOUD_PERMISSION_REQUIRED` requires a user permission change, not login; `ACCOUNT_MISMATCH` requires account review; `CLOUD_RESULT_UNCERTAIN` requires checking the original operation before any retry; `FILE_CONFLICT` never silently overwrites; `SOURCE_CHANGED` requires re-preparation. Creation of a cloud share is distinct from parsing/downloading an existing share link. The latter continues through resource prepare/confirm, and required cloud saves still return `CLOUD_WRITE_REQUIRED` for desktop review in this stage.
