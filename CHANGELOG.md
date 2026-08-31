# Changelog

## v1.2.1 - 2026-08-31

### 修复

- 修复通过 Launcher 启动 DSH 时继承 GUI 会话中的错误 Node/pnpm PATH，导致插件安装出现 `ERR_PNPM_UNEXPECTED_STORE`。
- 正式启动服务时从用户默认登录 Shell 读取工具链环境（PATH、PNPM_HOME、nvm/volta/asdf/mise/fnm），不再把 GUI 旧 PATH 传给 Shell 初始化。
- 登录 Shell 使用平台最小系统 PATH 启动，保证 rc 文件在设置用户 PATH 前仍能调用基础命令。
- 外部 DSH 只追加自身目录，不改变用户工具链优先级；托管 DSH 仅在规范入口、有效标记和完整 Node/runtime 同时存在时前置托管运行时。
- 兼容 Windows 环境变量键的大小写语义，并保留 Unix 环境变量中的非 UTF-8 字节。
- 版本探测不再重复启动登录 Shell，避免 bootstrap/detect/validate 额外卡住。

### 验证

- macOS Rust 单元测试通过。
- Windows GNU 目标 Rust 编译检查通过。
- Linux 原生构建由 GitHub Actions runner 执行。

## v1.2.0 - 2026-08-31

- 外部 DSH 强制关闭与版本信息弹窗。

## v1.1.0 - 2026-08-31

- 增加 DSH 版本更新提示与升级命令弹窗。
