#!/usr/bin/env bash
# 本地 macOS 构建入口：为 updater 热更签名加载私钥与密码，再走 tauri build + 补签。
# 查找顺序：已导出的环境变量 > ~/.tauri 下的密钥/密码文件；都没有时直接报错退出
# （createUpdaterArtifacts 已开启，无法跳过签名）。
set -euo pipefail

if [[ -z "${TAURI_SIGNING_PRIVATE_KEY:-}" ]]; then
  key_file="${TAURI_SIGNING_PRIVATE_KEY_FILE:-}"
  if [[ -z "$key_file" ]]; then
    if [[ -s "$HOME/.tauri/dsh-desktop.key" ]]; then
      key_file="$HOME/.tauri/dsh-desktop.key"
    else
      key_file="$HOME/.tauri/dsh-launcher.key"
    fi
  fi
  if [[ -s "$key_file" ]]; then
    export TAURI_SIGNING_PRIVATE_KEY="$(cat "$key_file")"
  else
    echo "未找到 updater 签名私钥（$key_file）。" >&2
    echo "请运行：pnpm exec tauri signer generate -w ~/.tauri/dsh-desktop.key --password \"<你的密码>\" --ci" >&2
    exit 1
  fi
fi

if [[ -z "${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}" ]]; then
  password_file="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD_FILE:-}"
  if [[ -z "$password_file" ]]; then
    if [[ -s "$HOME/.tauri/dsh-desktop.key.password" ]]; then
      password_file="$HOME/.tauri/dsh-desktop.key.password"
    else
      password_file="$HOME/.tauri/dsh-launcher.key.password"
    fi
  fi
  if [[ -s "$password_file" ]]; then
    export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$(cat "$password_file")"
  else
    echo "未找到 updater 签名密码（$password_file）。" >&2
    echo "请 export TAURI_SIGNING_PRIVATE_KEY_PASSWORD=<你的密码>，或把密码写入该文件（chmod 600）。" >&2
    exit 1
  fi
fi

pnpm exec tauri build --bundles app "$@"
scripts/sign-macos-app.sh