# DSH Launcher

简体中文 | [English](README_EN.md)

## 项目截图

**DSH Launcher**

![DSH Launcher 桌面界面](docs/images/launcher.png)

**内置 DeepSeek Harness WebView**

![DeepSeek 内置 WebView](docs/images/webview.png)

## 简介

DSH Launcher 是一个基于 Tauri 2、React 和 Rust 的轻量桌面启动器，用于启动和管理本机已经安装的 DeepSeek Harness（DSH）Web 服务。

> **平台测试状态：** 当前版本已在 macOS 上验证。Windows 和 Linux 构建配置已经提供，但尚未在真实 Windows/Linux 桌面环境中测试，请将对应平台产物视为候选版本。

### 功能

- 自动检测或手动选择 `dsh` 可执行文件
- 配置 Profile、监听主机、端口和额外 DSH 参数
- 启动、停止和重启由 Launcher 创建的 DSH 服务
- HTTP 健康检查和实时 stdout/stderr 日志
- 服务就绪后打开内置 WebView 或默认浏览器
- 支持 macOS、Windows 和 Linux 的系统明暗主题
- Launcher 退出时仅回收自己创建的 DSH 进程，不接管端口上已有的外部服务

### 使用边界

DSH Launcher **只负责启动和管理已有的 DSH 安装**。它不会安装或升级 DSH，也不会安装、添加、删除或更新任何 DSH 插件。插件管理仍应通过 DSH 自身提供的命令和配置完成。

使用前请确保 `dsh` 已安装并可正常运行：

```bash
dsh --version
```

### macOS 安装提示

未经 Apple 公证的构建可能被 macOS 标记为来自未知开发者。请先将 `DSH Launcher.app` 放入 `/Applications`。如果仍无法打开，可以移除应用的 quarantine 属性：

```bash
sudo xattr -r -d com.apple.quarantine "/Applications/DSH Launcher.app"
```

只应对你信任且来源明确的应用执行此命令。

首次启动时，macOS 15 或更高版本会请求“本地网络”权限。请允许访问，否则 DSH 可能无法连接局域网中的模型服务。可在“系统设置 → 隐私与安全性 → 本地网络”中修改此权限。

### 开发

要求：Node.js 22、Rust 1.88 或更高版本，以及对应平台的 Tauri 2 系统依赖。

```bash
pnpm install --frozen-lockfile
pnpm run tauri dev
```

执行检查：

```bash
pnpm run build
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

### 构建桌面安装包

```bash
pnpm run tauri build
```

仓库中的 GitHub Actions 工作流（[.github/workflows/build.yml](.github/workflows/build.yml)）使用 pnpm 11 在原生 macOS、Windows 和 Linux runner 上构建候选产物，三个平台均覆盖 arm64 / x64：macOS 生成 DMG，Windows 生成 NSIS，Linux 生成 DEB、RPM 和 AppImage。

macOS 的“本地网络”权限以代码签名标识应用。要让权限在升级和重装后可靠保持，请配置 Apple Developer ID 签名证书；未签名或 ad-hoc 签名包仍可构建，但系统可能无法稳定识别其权限状态。GitHub Actions 支持以下仓库 Secrets：

- `APPLE_CERTIFICATE`：Base64 编码的 `.p12` 证书
- `APPLE_CERTIFICATE_PASSWORD`：证书密码
- `APPLE_SIGNING_IDENTITY`：Developer ID Application 签名身份

默认工作流不执行 Apple 公证。
