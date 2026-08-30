# DSH Launcher

English | [简体中文](README.md)

## Screenshots

**DSH Launcher**

![DSH Launcher desktop interface](docs/images/launcher.png)

**Embedded DeepSeek Harness WebView**

![DeepSeek Harness embedded WebView](docs/images/webview.png)

## Overview

DSH Launcher is a lightweight Tauri 2, React, and Rust desktop application for installing, starting, and managing a local DeepSeek Harness (DSH) Web service.

> **Platform testing status:** The current version has been verified on macOS. Build configurations are provided for Windows and Linux, but they have not yet been tested on real Windows/Linux desktop environments. Treat those artifacts as candidate builds.

### Features

- Detect `dsh` automatically or select its executable manually
- Install a dedicated Node LTS runtime and DSH in an empty directory selected by the user
- Detect and upgrade Launcher-managed DSH after confirmation and service shutdown
- Check Launcher releases and mark the version when an update is available
- Configure the profile, bind host, port, and additional DSH arguments
- Start, stop, and restart DSH processes created by the Launcher
- Run HTTP health checks and display live stdout/stderr logs
- Open an embedded WebView or the default browser after the service is ready
- Follow the system light or dark theme on macOS, Windows, and Linux
- Clean up only Launcher-owned DSH processes on exit and never take over an external service already using the configured port

### Scope

DSH Launcher can continue using an existing external DSH installation or create an isolated managed environment in an empty directory selected by the user. Managed installation downloads and verifies the latest official Node LTS release before installing DSH. After installation, the Launcher can upgrade DSH only; it does not upgrade Node. External installations remain read-only and are never modified. Continue to install, add, remove, and update plugins through DSH's own commands and configuration.

Upgrading managed DSH requires confirmation and stops the Launcher-owned service; it does not restart automatically afterward. When using an external installation, make sure `dsh` works normally:

```bash
dsh --version
```

### macOS Installation Note

Builds that are not notarized by Apple may be marked as coming from an unidentified developer. Move `DSH Launcher.app` to `/Applications` first. If macOS still refuses to open it, remove the quarantine attribute:

```bash
sudo xattr -r -d com.apple.quarantine "/Applications/DSH Launcher.app"
```

Run this command only for an application you trust and obtained from a known source.

On first launch, macOS 15 or later requests Local Network permission. Allow it, or DSH may be unable to connect to model services on your LAN. You can change this permission in System Settings > Privacy & Security > Local Network.

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

Use the signing-aware command for local macOS builds so Local Network permission metadata and the final app signature are verified:

```bash
pnpm run build:mac
```

Use `pnpm run tauri build` on Windows/Linux. The included GitHub Actions workflow ([.github/workflows/build.yml](.github/workflows/build.yml)) uses pnpm 11 to build candidate artifacts on native macOS, Windows, and Linux runners, with arm64 and x64 covered on all three platforms: DMG for macOS, NSIS for Windows, and DEB, RPM, and AppImage for Linux.

The macOS build does not require an Apple Developer account. Without a certificate, the workflow ad-hoc signs the complete `.app` with a fixed bundle identifier before creating the DMG, allowing macOS to read the Local Network usage description and request permission. Ad-hoc signing provides neither notarization nor a trusted developer identity, so upgrades or reinstalls may require permission again. For stable identity across releases and formal distribution, configure these repository secrets:

- `APPLE_CERTIFICATE`: Base64-encoded `.p12` certificate
- `APPLE_CERTIFICATE_PASSWORD`: Certificate password
- `APPLE_SIGNING_IDENTITY`: Developer ID Application signing identity

The default workflow does not perform Apple notarization.
