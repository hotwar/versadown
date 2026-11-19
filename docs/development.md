# 开发与构建

[返回项目介绍](../README.md)

以下命令均在仓库根目录执行。开发与验证约定见 [AGENTS.md](../AGENTS.md)；按改动范围选择检查，日常验证优先使用 quick 构建。

## 技术组成

- Tauri 2 + React + TypeScript 桌面界面，使用系统 WebView。
- Rust 后端与本地 SQLite 任务存储。
- aria2-next 负责 HTTP/HTTPS、BT、ED2K、HLS/DASH 下载；FTP 使用进程内 Rust 引擎，支持断点续传。
- yt-dlp 解析视频播放页，FFmpeg 合并音视频，QuickJS 提供所需的脚本运行环境。

组件许可证和对应源码见 [第三方说明](../THIRD_PARTY_NOTICES.md)及[视频工具说明](../src-tauri/video-tool-notices/SOURCE.md)。

## 视频开发工具

从源码运行开发版时可使用 `.tools/video` Python 虚拟环境中的 `yt-dlp[default,curl-cffi]`，以及本机的 FFmpeg 和 Node.js 22+。打包脚本直接下载并校验 yt-dlp 的官方可执行文件；Windows 和 Linux 的 QuickJS 也使用官方二进制。精简版 FFmpeg 和兼容 macOS 12 的 QuickJS 会缓存编译结果，macOS 构建机另需 CMake。

## macOS（Apple Silicon / Intel）

需要 macOS 12+、Xcode Command Line Tools、Node 24+、pnpm 10 与 Rust stable。
两种架构的 aria2-next 3.0.9 引擎随包提供，构建前检查官方 SHA-256；应用使用系统 WKWebView。

迅雷网盘任务可使用本机迅雷 5.80.7（66660）的下载引擎。将官方迅雷安装在 `/Applications` 或 `~/Applications` 即可，下载时不必打开迅雷。VD 不附带迅雷组件。没有匹配的引擎、文件没有加速凭据，或 VD 配置了代理时，新任务仍走 aria2。已有迅雷断点需在直连模式下用匹配的引擎继续。队列在引擎收到加速通道数据后显示“迅雷会员通道”，实际速度取决于资源。

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

## Linux x86_64 调试二进制

安装 Node 24+、Rust stable、GTK/WebKitGTK 开发依赖后，执行以下命令生成嵌入静态前端资源的本机调试二进制。不要直接执行不含 `custom-protocol` 功能的 `cargo build` 产物，它会尝试连接 Vite 开发服务器的 `localhost:1420`。

```bash
pnpm install --frozen-lockfile
pnpm run build:linux:quick
./src-tauri/target/debug/versadown
```

产物包含 Linux x86_64 的 aria2-next 3.0.9 sidecar。该命令生成调试二进制，不生成 Release 包或安装包。

## Linux x86_64 发布包

在具备上述依赖的 Linux x86_64 系统执行：

```bash
pnpm install --frozen-lockfile
pnpm run build:linux:release
```

脚本使用 Tauri 生成 `.deb`、`.rpm` 和 `.AppImage`，只打包 Linux 版 aria2-next，并把安装包及 `SHA256SUMS` 放在 `artifacts/linux/`。该命令是完整 Release 编译，不是日常调试入口。跨平台发布流程见下文。

## Windows

Windows 10/11、WebView2 runtime、Node 24+、Rust stable MSVC、Visual Studio C++ Build Tools。本地生成 NSIS 安装包时，FFmpeg 使用 MSYS2 UCRT64 的 GCC、binutils 和 MSYS make；可用 `VD_MSYS2_ROOT` 指定 MSYS2 目录。CI 使用 MinGW-w64 交叉编译 FFmpeg，并在 Windows 上运行和验证打包后的工具。

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

## 持续集成与发布

`Desktop checks` 工作流在 PR 中检查 Windows、Linux、Apple Silicon 与 Intel Mac。`Desktop release` 可手动运行并下载构建产物；推送与应用版本一致的 `v*` 标签后，各平台构建全部成功才会统一发布 GitHub Release。
