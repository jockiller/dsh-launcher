# DSH Launcher

English | [简体中文](README.md)

## Screenshots

**DSH Launcher**

![DSH Launcher desktop interface](docs/images/launcher.png)

**Embedded DeepSeek Harness WebView**

![DeepSeek Harness embedded WebView](docs/images/webview.png)

## Overview

DSH Launcher is a lightweight Tauri 2, React, and Rust desktop application for starting and managing an existing local DeepSeek Harness (DSH) Web service.

> **Platform testing status:** The current version has been verified on macOS. Build configurations are provided for Windows and Linux, but they have not yet been tested on real Windows/Linux desktop environments. Treat those artifacts as candidate builds.

### Features

- Detect `dsh` automatically or select its executable manually
- Configure the profile, bind host, port, and additional DSH arguments
- Start, stop, and restart DSH processes created by the Launcher
- Run HTTP health checks and display live stdout/stderr logs
- Open an embedded WebView or the default browser after the service is ready
- Follow the system light or dark theme on macOS, Windows, and Linux
- Clean up only Launcher-owned DSH processes on exit and never take over an external service already using the configured port

### Scope

DSH Launcher **only starts and manages an existing DSH installation**. It does not install or upgrade DSH, and it does not install, add, remove, or update DSH plugins. Continue to manage plugins through DSH's own commands and configuration.

Before using the Launcher, make sure `dsh` is installed and works normally:

```bash
dsh --version
```

### macOS Installation Note

Builds that are not notarized by Apple may be marked as coming from an unidentified developer. Move `DSH Launcher.app` to `/Applications` first. If macOS still refuses to open it, remove the quarantine attribute:

```bash
sudo xattr -r -d com.apple.quarantine "/Applications/DSH Launcher.app"
```

Run this command only for an application you trust and obtained from a known source.

macOS requests Local Network permission the first time the app accesses a model service on your LAN. Allow this permission, or DSH may be unable to connect to local network model providers.

### Development

Requirements: Node.js 22, Rust 1.88 or newer, and the Tauri 2 system dependencies for your platform.

```bash
pnpm install --frozen-lockfile
pnpm run tauri dev
```

Run checks:

```bash
pnpm run build
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings
```

### Build Desktop Packages

```bash
pnpm run tauri build
```

The included GitHub Actions workflow ([.github/workflows/build.yml](.github/workflows/build.yml)) uses pnpm 11 to build DMG, NSIS, DEB, RPM, and AppImage candidate artifacts on native macOS (arm64 / x64), Windows (x64), and Linux (x64) runners. Code signing and Apple notarization are intentionally not part of the default workflow.