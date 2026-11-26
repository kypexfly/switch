#!/bin/sh
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-"$root/target"}
bin=${SWITCH_BIN_DIR:-"$HOME/.local/bin"}
built="$target_dir/release/swx"

[ -d "$root/vendor/opentui-native" ] || "$root/scripts/fetch-opentui.sh"
(cd "$root" && cargo build --release -p switch-cli)

service_was_active=false
if command -v systemctl >/dev/null && systemctl --user --quiet is-active switch 2>/dev/null; then
  service_was_active=true
  systemctl --user stop switch
else
  "$built" stop
fi

mkdir -p "$bin"
install -m 755 "$built" "$bin/swx"
echo "Installed $bin/swx"

case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "Add $bin to your PATH to use swx from any terminal." ;;
esac

if [ "$service_was_active" = true ]; then
  systemctl --user start switch
  echo "Restarted the switch service."
fi
