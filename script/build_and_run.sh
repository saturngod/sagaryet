#!/usr/bin/env bash
set -euo pipefail

MODE="${1:-run}"
case "$MODE" in
  run|--debug|--logs|--telemetry|--verify) ;;
  *) echo "usage: $0 [--debug|--logs|--telemetry|--verify]" >&2; exit 2 ;;
esac
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"
pkill -x sagaryet >/dev/null 2>&1 || true
bun run tauri build --debug --bundles app
APP_BUNDLE="$ROOT_DIR/src-tauri/target/debug/bundle/macos/Sagaryet.app"
case "$MODE" in
  --debug) lldb -- "$APP_BUNDLE/Contents/MacOS/sagaryet" ;;
  --logs)
    /usr/bin/open -n "$APP_BUNDLE"
    /usr/bin/log stream --info --style compact --predicate 'process == "sagaryet"'
    ;;
  --telemetry)
    /usr/bin/open -n "$APP_BUNDLE"
    /usr/bin/log stream --info --style compact --predicate 'subsystem == "app.myanmars.sagaryet"'
    ;;
  --verify)
    /usr/bin/open -n "$APP_BUNDLE"
    sleep 1
    pgrep -x sagaryet >/dev/null
    ;;
  run) /usr/bin/open -n "$APP_BUNDLE" ;;
esac
