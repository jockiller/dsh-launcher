# DSH Launcher

English | [简体中文](README.md)

**A tiny, non-intrusive DSH launcher — no plugin conflicts to worry about. One-click install and auto start.**

A lightweight desktop app to install, start, and manage a local DeepSeek Harness (DSH) Web service. Built with Tauri 2 + React + Rust.

## Screenshots

![DSH Launcher desktop interface](docs/images/launcher.png)

![DeepSeek embedded WebView](docs/images/webview.png)

## Features

- **Auto-detect**: automatically finds an installed DSH, or pick one manually
- **One-click install**: downloads and verifies Node LTS, then installs DSH into an empty directory — no system PATH changes
- **One-click upgrade**: detects new managed DSH releases, stops the service after confirmation, then upgrades
- **In-place self-update**: the Launcher detects its own new versions, shows the update log, verifies signatures, and installs with a single restart — no manual download required
- **Service management**: start / stop / restart, HTTP health checks, live logs
- **Ready to use**: opens an embedded WebView or the default browser once the service is up
- **Flexible config**: profile, bind address, port, and extra DSH arguments
- **Works with external installs**: existing DSH is detected read-only and never modified
- **Cross-platform**: macOS / Windows / Linux (arm64 & x64), follows system light/dark theme

## Notes

- Plugin install and upgrades are handled by DSH's own commands and configuration
- Managed environments upgrade DSH only, never Node; upgrades ask for confirmation, stop the service, and do not auto-restart
- For external DSH, make sure it runs: `dsh --version`

## macOS First Launch

- The app is not notarized. If macOS reports an unidentified developer, move `DSH Launcher.app` to `/Applications` and run:

  ```bash
  sudo xattr -r -d com.apple.quarantine "/Applications/DSH Launcher.app"
  ```

- On macOS 15+, allow the Local Network permission on first launch (System Settings → Privacy & Security → Local Network)

## Build from Source

Requirements: Node.js 22+, Rust 1.88+, and the Tauri 2 dependencies for your platform.

```bash
pnpm install --frozen-lockfile
pnpm run tauri dev        # local development
pnpm run build:mac        # macOS build (re-sign + permission metadata check)
pnpm run tauri build      # Windows / Linux build
```

## License

[MIT](LICENSE)