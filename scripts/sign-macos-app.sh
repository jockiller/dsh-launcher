#!/usr/bin/env bash
set -euo pipefail

app="${1:-src-tauri/target/release/bundle/macos/DSH Desktop.app}"
if [[ -z "$app" || ! -d "$app" ]]; then
  echo "No macOS app bundle found." >&2
  exit 1
fi

plist="$app/Contents/Info.plist"
description=$(plutil -extract NSLocalNetworkUsageDescription raw "$plist")
local_networking=$(plutil -extract NSAppTransportSecurity.NSAllowsLocalNetworking raw "$plist")
test -n "$description"
test "$local_networking" = "true"

if [[ -z "${APPLE_CERTIFICATE:-}" ]]; then
  codesign --force --deep --sign - --identifier ai.deepseek.dsh-desktop "$app"
fi
codesign --verify --deep --strict --verbose=2 "$app"
printf 'Signed and verified: %s\n' "$app"
