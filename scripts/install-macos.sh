#!/bin/sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
apps=${SWITCH_APP_DIR:-/Applications}
bin=${SWITCH_BIN_DIR:-"$HOME/.local/bin"}
app="$apps/Switch.app"
swx="$app/Contents/MacOS/swx"

[ -d "$root/vendor/opentui-native" ] || "$root/scripts/fetch-opentui.sh"
"$root/scripts/bundle-macos.sh"

was_running=false
if pgrep -xq switch-desktop; then
  was_running=true
  osascript -e 'quit app "Switch"' >/dev/null 2>&1 || pkill -x switch-desktop || true
  while pgrep -xq switch-desktop; do sleep 0.2; done
fi

"$root/dist/Switch.app/Contents/MacOS/swx" stop

if [ "$(readlink "$bin/sw" 2>/dev/null)" = "$app/Contents/MacOS/sw" ]; then
  rm "$bin/sw"
fi

mkdir -p "$apps" "$bin"
rm -rf "$app"
ditto "$root/dist/Switch.app" "$app"
ln -sf "$swx" "$bin/swx"
echo "Installed $app and $bin/swx"

case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "Add $bin to your PATH to use swx from any terminal." ;;
esac

if [ "$was_running" = true ]; then
  open "$app"
fi
