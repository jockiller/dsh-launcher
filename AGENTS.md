# Project Instructions

Shared rules: `～/.config/AGENTS.md`. This file contains only DSH Launcher-specific technology, structure, commands, and domain rules.

## Project

DSH Launcher is a cross-platform desktop manager for DeepSeek Harness (DSH) Web services.

- Stack: Tauri 2, Rust 2024, React 19, TypeScript, and Vite
- Platforms: macOS, Windows, and Linux
- Frontend: `src/App.tsx`, `src/i18n.ts`, `src/styles.css`
- Backend: `src-tauri/src/`
- Packaging scripts: `scripts/`

## Commands

```bash
pnpm dev
pnpm tauri dev
./node_modules/.bin/tsc --noEmit
./node_modules/.bin/vite build
cargo check --manifest-path src-tauri/Cargo.toml --offline
cargo test --manifest-path src-tauri/Cargo.toml --offline
```

Inspect `package.json` and `src-tauri/Cargo.toml` before changing commands or dependencies. Run focused checks relevant to the change.

## Rust and Tauri Rules

- Track spawned processes and clean them up on exit; avoid orphaned process groups.
- Stop processes gracefully before escalation.
- Keep background observers isolated so they cannot crash or block the application.
- Run blocking network, disk, download, and process-wait work off the UI runtime.
- Tauri command handlers normally return user-friendly `Result<T, String>` errors.
- Add tests for path resolution, environment handling, parsing, and platform fallbacks.

## Frontend Rules

- Add every user-facing string to both `zhDict` and `enDict` in `src/i18n.ts`.
- Translate backend messages through `translateBackendMessage`.
- Preserve modal focus management and accessible controls.
- Maintain responsive layouts and both light and dark theme variables.
