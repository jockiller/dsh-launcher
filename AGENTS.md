# Repository Instructions

## Project Overview

DSH Desktop is a cross-platform desktop manager for DeepSeek Harness (DSH) Web services.

- Stack: Tauri 2, Rust 2024, React 19, TypeScript, and Vite
- Platforms: macOS, Windows, and Linux
- Frontend: `src/App.tsx`, `src/i18n.ts`, `src/styles.css`
- Backend: `src-tauri/src/`
- Packaging scripts: `scripts/`

## Working Style

- Understand the repository and existing conventions before editing.
- Keep changes focused on the user's request. Avoid speculative features, unrelated refactors, and broad formatting changes.
- Preserve existing user changes and do not overwrite work you did not create.
- State important assumptions when they affect implementation; ask only when a decision genuinely requires user input.

## Permissions and Safety

- Never connect to an SSH server without explicit authorization for the current task, including `ssh`, `scp`, `sftp`, and SSH tunnels.
- Do not infer permission to access another host, account, repository, or environment from a previous task.
- Do not start external services, modify production systems, publish data, deploy, send real notifications, or change real user data without explicit authorization.
- Treat databases as read-only by default. Ask before write, migration, seed, or deletion commands.
- Do not install system-wide dependencies or create new environments without explicit authorization.
- Do not use destructive commands or broad deletion without explicit authorization.
- If credentials, permissions, or external approval are required, report the blocker instead of bypassing it.

## Python

- Never use the system Python.
- When Python is required, use an existing Conda environment, preferably `base` or `py310`.
- Verify the selected interpreter before running Python tooling, for example:
  ```bash
  conda run -n base python --version
  conda run -n py310 python --version
  ```
- Do not create or install a new Python environment unless explicitly requested.

## Development and Verification

```bash
pnpm dev
pnpm tauri dev
./node_modules/.bin/tsc --noEmit
./node_modules/.bin/vite build
cargo check --manifest-path src-tauri/Cargo.toml --offline
cargo test --manifest-path src-tauri/Cargo.toml --offline
```

- Inspect `package.json` and `src-tauri/Cargo.toml` before changing commands or dependencies.
- Run the smallest relevant tests, type checks, lint checks, or compile checks available.
- Ordinary local inspection, editing, testing, and reversible non-release builds are allowed by default.
- Do not start or leave development servers or other long-running processes running unless required by the task.
- Do not build, sign, package, publish, or release deliverable artifacts without explicit authorization.
- Report verification results, skipped checks, failures, and blockers accurately.

## Code Standards

### Rust and Tauri

- Track spawned processes and clean them up on exit; avoid orphaned process groups.
- Stop processes gracefully before escalation.
- Keep background observers isolated so they cannot crash or block the application.
- Run blocking network, disk, download, and process-wait work off the UI runtime.
- Tauri command handlers normally return user-friendly `Result<T, String>` errors.
- Add tests for path resolution, environment handling, parsing, and platform fallbacks.

### Frontend

- Add every user-facing string to both `zhDict` and `enDict` in `src/i18n.ts`.
- Translate backend messages through `translateBackendMessage`.
- Preserve modal focus management and accessible controls.
- Maintain responsive layouts and both light and dark theme variables.

## Git

- Do not create commits automatically.
- A user's request to commit applies only to the current task and current session; it does not carry over to later tasks or sessions.
- Before a requested commit, inspect the diff and exclude unrelated changes.
- Do not push, create releases, or alter remote history without separate explicit authorization.
- Never commit secrets, credentials, generated artifacts, temporary files, or `handoff.md`.

## Communication and Completion

- Use Simplified Chinese for collaboration messages and commit messages by default, unless the user requests another language. Preserve existing source and artifact languages where required.
- Create or update `handoff.md` only when work spans sessions or the user requests a handoff artifact; never commit it.
- Before finishing:
  1. Review the scoped diff and confirm that changes match the request.
  2. Run the appropriate available checks.
  3. Check for unintended temporary files and running processes.
  4. Report completed work, verification results, and any remaining limitations.
