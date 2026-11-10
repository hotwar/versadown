#!/usr/bin/env bash
# Exercise the Mac engine against an official Thunder DMG without installing the GUI.
set -euo pipefail

build=${1:?Pass a supported four-part Thunder build number}
case "$build" in
  5.80.6.66655) sha=91e7fe0eca34ad21a39355a8dc9066470c31e3f1e14a4d2f293d413b7df70d9f ;;
  5.80.7.66659) sha=2d5c41dad06f2070e0496a77754be25c3848e33c4e7ddd25ca61a04f8fb5bf9c ;;
  5.80.8.66675) sha=820b9023348d3923b0414cb63d9f5eb253a43e832112e1e38c9eaa66e6f9a32d ;;
  *) echo "No verified DMG for build $build" >&2; exit 2 ;;
esac

work=$(mktemp -d)
mount="$work/mount"
mounted=false
cleanup() {
  if [[ "$mounted" == true ]]; then
    hdiutil detach "$mount" || return 1
  fi
  rm -rf "$work"
}
trap cleanup EXIT
mkdir "$mount"

# Official builds listed at https://yangtai.xunlei.com/?cat=44.
curl --fail --location --silent --show-error --proto '=https' --proto-redir '=https' \
  --connect-timeout 20 --max-time 240 --retry 3 \
  "https://down.sandai.net/mac/thunder_${build}.dmg" -o "$work/thunder.dmg"
(cd "$work" && printf '%s  thunder.dmg\n' "$sha" | shasum -a 256 -c -)
hdiutil attach -readonly -nobrowse -noautoopen -mountpoint "$mount" "$work/thunder.dmg"
mounted=true
export VERSADOWN_THUNDER_FIXTURE_APP="$mount/Thunder.app"
plist="$VERSADOWN_THUNDER_FIXTURE_APP/Contents/Info.plist"
test "$(plutil -extract CFBundleShortVersionString raw "$plist")" = "${build%.*}"
test "$(plutil -extract CFBundleVersion raw "$plist")" = "${build##*.}"

# This test also verifies the Developer ID and the staged XPC before running the SDK.
cargo test --locked --manifest-path src-tauri/Cargo.toml --lib \
  xunlei_native::macos::probe::tests::signed_service_passes_anonymous_probe \
  -- --exact --ignored --nocapture | tee "$work/result.log"
grep -Fq 'test result: ok. 1 passed;' "$work/result.log"
