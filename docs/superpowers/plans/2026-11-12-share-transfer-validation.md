# 网盘分享输入与转存验收

分享输入修复覆盖整段文案、Markdown 链接、链接紧贴提取码、查询串提取码与批量输入。扫描结果应携带当前分享自己的提取码，原始 URL 的签名和查询串保持不变。输入回归在 `parser::tests` 和 `parser_policy` 中运行，使用替代分享 ID。

## 自动回归

```sh
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib parser::tests
cargo test --locked --manifest-path src-tauri/Cargo.toml --test parser_policy
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib cloud::share::tests
pnpm exec vitest run src/components/useDownloadDraftBatch.test.tsx src/components/NewDownloadDialog.test.tsx src/components/ParseConfirmDialog.test.tsx src/lib/api.test.ts --exclude '.worktrees/**'
```

现有 Desktop checks 会在推送 `master`、`main` 或 `codex/**` 分支后运行跨平台检查，并编译真实分享矩阵测试。真实账号测试为显式启用项，公共 CI 不加载用户凭据。

## 真实分享矩阵

`src-tauri/tests/live_share_matrix.rs` 从外部 JSON 文件读取样本。实际分享地址、提取码和用户会话保留在测试机器，不提交到仓库。格式如下：

```json
[
  {
    "id": "uc-with-code",
    "provider": "uc",
    "input": "链接：https://drive.uc.cn/s/REPLACE提取码：REPLACE"
  }
]
```

每项必需 `id`、`provider`、`input`，可选 `access_code`（模拟手动输入，优先于文案）和 `select_name`（选择特定文件）。`input` 可以直接使用整段分享文案。案例 ID 只允许字母、数字、点、下划线和连字符。

PowerShell 中先设置本机路径：

```powershell
$env:VERSADOWN_LIVE_CLOUD = '1'
$env:VERSADOWN_LIVE_SHARE_MATRIX = 'C:\VD-tests\samples.json'
$env:VERSADOWN_LIVE_SHARE_WORK_DIR = 'C:\VD-tests\share-matrix'
$env:VERSADOWN_LIVE_SESSION_DIR = Join-Path $env:APPDATA 'com.versadown.desktop\cloud-sessions'
cargo test --locked --manifest-path src-tauri/Cargo.toml --test live_share_matrix live_share_matrix_inspects_all_cases -- --ignored --exact --nocapture
```

会话目录应选择当前实际使用的 VD 实例。Windows 未指定时采用上述 APPDATA 默认路径，其他平台需显式提供。工作目录与生产会话目录分开，保存独立的加密分享计划和测试报告。

解析通过后再验证转存和取链：

```powershell
$env:VERSADOWN_LIVE_SHARE_TRANSFER = '1'
cargo test --locked --manifest-path src-tauri/Cargo.toml --test live_share_matrix live_share_matrix_resolves_and_reads_bounded_ranges -- --ignored --exact --nocapture
```

此步骤按服务商需要将文件保存到当前账号，每例最多选择一个文件、读取最多 64 KiB，并保留加密转存记录供重复运行复用。它不建立生产下载任务，也不删除云文件。`inspect-report.json` 与 `range-report.json` 只记录案例、网盘、阶段、状态、资源数与字节数；筛选单例可设置 `VERSADOWN_LIVE_SHARE_CASE`。

UC 返回需要网页授权时，可另设 `VERSADOWN_LIVE_UC_DIAGNOSTICS=1`，记录接口分类、HTTP 状态、业务码及必要授权字段是否存在。诊断不输出字段值、Cookie 或签名下载地址。缺少分享取链所需会话信息时，应通过 VD 的官方网页授权恢复，再重跑该案例。

## 放行口径

- 文案识别、服务端资源解析、转存取链、字节读取、完整下载和自动清理分别记录结果。
- 64 KiB 探测成功只证明取链和内容读取可用，不能代替完整文件校验。
- 自动清理需用真实完成任务及其精确转存记录验证；不能将部分读取伪装成下载完成，也不能按文件名或目录名删除文件。
- 历史验收与当前提交、Mac 与 Windows 的实测结果分开记录。未运行或未通过的阶段继续保留为待验收。
