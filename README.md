# VersaDown · Desktop Beta

本地优先的桌面下载客户端。它把标准下载链接、磁力/种子、迅雷链接、网盘分享链接和复杂文本放入统一任务流程，并使用小型悬浮输入窗与下载管理器显示进度。

## 下载与安装

前往 [GitHub Releases 下载最新版](https://github.com/liuwenhuan/versadown/releases/latest)，按系统和处理器架构选择安装包。发布页同时提供 SHA-256 校验文件。

| 平台 | 架构 | 发布包 |
| --- | --- | --- |
| Windows 10/11 | x86_64 | `.exe` 安装包，需要 WebView2 runtime |
| macOS 12+ | Apple Silicon（ARM64）/ Intel（x86_64） | `.dmg` / `.app.zip`，请选择对应架构 |
| Linux | x86_64 | `.deb` / `.rpm` / `.AppImage` |

## 当前可运行能力

- 基于 Tauri 2 的 Windows、macOS 与 Linux 桌面应用，使用系统 WebView；没有 Electron/Chromium 捆绑包。
- HTTP/HTTPS、FTP、ED2K、HLS `.m3u8`、MPEG-DASH `.mpd`、magnet、`.torrent`、Thunder 和网盘分享地址的规则分类；直链自动进入下载状态，多文件类型先进入确认窗口。
- 本地 SQLite 任务状态、恢复时读取的任务列表、暂停/继续/取消状态操作，以及目录父子任务界面。
- 各平台均随包提供对应的 aria2-next 引擎，负责 HTTP、BT、ED2K 以及原生 HLS/DASH 下载；任务状态、续传信息和媒体清单均由 aria2-next 统一持久化。FTP 仍由进程内纯 Rust 引擎下载并支持断点续传。
- 安全的后处理计划模型：只允许解压、合并、改名、校验；拒绝程序执行和路径穿越。页面中取得的密码仅存为受保护引用，不写入调试对象。
- 已接入夸克、PikPak、百度网盘、阿里云盘、迅雷网盘和 UC 网盘，提供应用内文件管理、上传下载、回收站和分享管理入口。各网盘的具体能力及授权要求存在差异，以应用界面和服务商限制为准。
- 可选的 Beta 调试报告开关默认关闭；关闭时不应收集详细链路数据。

## 界面预览

以下截图采自 macOS 上实际运行的 VersaDown，账号信息已遮挡；不同版本和平台的外观可能略有差异。

### 下载管理

在统一列表中查看任务进度并管理下载。

![VersaDown 下载管理界面](docs/screenshots/downloads.png)

### 新建下载

查看已识别的下载信息，确认后创建单个下载任务。

![VersaDown 新建下载界面](docs/screenshots/new-download.png)

### 网盘文件

在夸克网盘目录中浏览文件，并使用上传、新建目录、回收站和「我的分享」入口。

![VersaDown 夸克网盘文件管理界面](docs/screenshots/cloud-files.png)

<details>
<summary>更多界面：网盘授权与任务搜索</summary>

### 网盘授权

集中管理夸克、PikPak、百度网盘、阿里云盘、迅雷网盘和 UC 网盘的授权。

![VersaDown 网盘授权管理界面](docs/screenshots/cloud-accounts.png)

### 任务搜索

按关键词查找已有下载任务。

![VersaDown 任务搜索界面](docs/screenshots/task-search.png)

</details>

<details>
<summary>更多界面：网络与隐私设置</summary>

### 网络设置

配置网络代理和下载限速。

![VersaDown 网络代理与下载限速设置界面](docs/screenshots/network-settings.png)

### 隐私设置

管理隐私模式等高级设置。隐私任务解锁后才显示，下载文件本身不加密。

![VersaDown 高级与隐私设置界面](docs/screenshots/privacy-settings.png)

</details>

## 明确的安全边界

- 不共享用户账号、Cookie、token、快速下载链接或下载能力。
- 不绕过验证码、短信、扫码、DRM、真实支付、未知表单或解压密码。
- AI 仅在规则无法可靠理解文本/网页时接收当前任务的脱敏可读文本；绝不发送 Cookie、凭据、密码、二维码、短信或本地文件。
- 任何服务商拒绝嵌入式授权时应转为系统浏览器或服务商允许流程；不得抓取 OAuth token。

## 开发环境

### macOS（Apple Silicon / Intel）

需要 macOS 12+、Xcode Command Line Tools、Node 24+、pnpm 10 与 Rust stable。
两种架构的 aria2-next 3.0.9 引擎随包提供，构建前检查官方 SHA-256；应用使用系统 WKWebView。

```bash
pnpm install --frozen-lockfile
pnpm run build:macos:quick       # 增量调试二进制
pnpm run build:macos:installer   # 调试 .app / .dmg，供本地验证
pnpm run build:macos:release     # 正式 Release 编译与打包
```

安装产物和校验文件在 `artifacts/macos-arm64/` 或 `artifacts/macos-x86_64/`。
将 DMG 中的 `VersaDown.app` 拖入 Applications；默认下载目录为用户的 Downloads。
关闭主窗口后下载继续，点击 Dock 图标可重开；菜单栏菜单或 ⌘Q 退出时保存引擎会话。
系统钥匙串不可用时会显示提示，普通下载仍可用，凭据操作报错且隐私任务保持锁定。
没有 Developer ID 凭据时只做本地 ad-hoc 签名，产物不代表已通过 Apple 公证。

包级黑盒检查（启动、真实下载校验、凭据不可用和子进程回收）：

```bash
python3 scripts/smoke-macos.py src-tauri/target/release/bundle/macos/VersaDown.app
cargo test --manifest-path src-tauri/Cargo.toml --features native-smoke --test macos_native_smoke
```

`Desktop checks` 工作流在 PR 中检查 Windows、Linux、Apple Silicon 与 Intel Mac。
推送与应用版本一致的 `v*` 标签后，`Desktop release` 在各平台构建，全部成功后发布同一个 GitHub Release。

### Linux x86_64 调试二进制

安装 Node 24+、Rust stable、GTK/WebKitGTK 开发依赖后，执行以下命令生成嵌入静态前端资源的本机调试二进制。不要直接执行不含 `custom-protocol` 功能的 `cargo build` 产物，它会尝试连接 Vite 开发服务器的 `localhost:1420`。

```bash
pnpm install --frozen-lockfile
pnpm run build:linux:quick
./src-tauri/target/debug/versadown
```

产物包含 Linux x86_64 的 aria2-next 3.0.9 sidecar。该命令生成调试二进制，不生成 Release 包或安装包。

### Linux x86_64 发布包

在具备上述依赖的 Linux x86_64 系统执行：

```bash
pnpm install --frozen-lockfile
pnpm run build:linux:release
```

脚本使用 Tauri 生成 `.deb`、`.rpm` 和 `.AppImage`，只打包 Linux 版 aria2-next，并把安装包及 `SHA256SUMS` 放在 `artifacts/linux/`。该命令是完整 Release 编译，不是日常调试入口。GitHub Actions 的 `Desktop release` 工作流可手动运行并下载产物；推送与应用版本一致的 `v*` 标签后，各平台构建全部成功才会统一发布 GitHub Release。

### Windows

Windows 10/11、WebView2 runtime、Node 24+、Rust stable MSVC、Visual Studio C++ Build Tools。项目构建时会使用 Tauri 的 Windows WiX 打包链。

```powershell
# 验证仓库内置的 Aria2 Next 原生媒体 sidecar
powershell -ExecutionPolicy Bypass -File scripts/download-sidecars.ps1

# 安装依赖，运行测试与 Web 构建
npm install
npm run test:run
npm run build

# 桌面开发模式
npm run tauri:dev

# 快速构建：适合日常本机验证；独立目标目录，不覆盖正式发布文件
pnpm run build:windows:quick

# 正式发布：生产前端构建 + Release + Thin LTO + 符号剥离
pnpm run build:windows:release

# 正式安装包（MSI / NSIS）
pnpm run build:windows:installer

# 验证正式发布包已产出
powershell -ExecutionPolicy Bypass -File scripts/smoke-download.ps1
```

快速构建输出到 `artifacts/quick/`，用于本机验证；正式主程序输出到
`artifacts/versadown.exe`，安装包位于
`.tools/release-target/release/bundle/`。`build:windows` 继续等同于
`build:windows:release`，以保持旧自动化命令兼容。

默认下载目录为当前 Windows 用户的 `Downloads`。任务数据库位于 Windows 应用数据目录中的 `VersaDown/tasks.sqlite`；不要手动复制其中可能关联授权会话的信息。

## 网盘授权与分享下载

网盘操作使用用户自身授权的会话，凭据在本机保护，目录由应用自绘。账号登录、文件访问和上传下载受服务商接口、账号权限及条款限制，不能依赖未审查的 Cookie 导入或规避服务商限制。

分享链接可能需要登录、输入提取码，或先转存到自己的网盘后才能下载；不保证免登录或免转存，也不保证所有网盘提供相同能力。天翼云盘当前未启用。

没有真实账户授权时，自动化测试使用 mock 连接器，不将测试数据当作真实云端操作结果。
