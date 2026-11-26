#!/bin/sh
set -eu

version=0.9.9
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform=darwin-arm64; sha256=c576d7ac40dfa0d9a50343c338da0936e5f89cf4c3f1132f6a830a5873c382b5 ;;
  Darwin-x86_64) platform=darwin-x64; sha256=f25ef248dad776a85f5fdc497c904e35f2e299a67fed6d724141b0ae24d5399b ;;
  Linux-x86_64) platform=linux-x64; sha256=abd03e0d1fd8ad79948f133cae9cc97421a9116802b81929465deb7bc0365660 ;;
  Linux-aarch64) platform=linux-arm64; sha256=629e3c5777578da3cc743de5bdbd22f594163b56a326b03e43d6ed8c831d46ad ;;
  *) echo "unsupported platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if command -v sha256sum >/dev/null; then sha256sum=sha256sum; else sha256sum="shasum -a 256"; fi

root=$(cd "$(dirname "$0")/.." && pwd)
target="$root/vendor/opentui-native"
archive=$(mktemp)
trap 'rm -f "$archive"' EXIT

curl -fsSL -o "$archive" "https://github.com/anomalyco/opentui/releases/download/v$version/opentui-native-v$version-$platform.zip"
echo "$sha256  $archive" | $sha256sum -c - >/dev/null

rm -rf "$target"
mkdir -p "$target"
unzip -q "$archive" -d "$target"
echo "OpenTUI $version installed in $target"
