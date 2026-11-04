#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
mode="${1:-quick}"
case "$mode" in quick|installer|release) ;; *) echo 'Usage: build-macos.sh [quick|installer|release]' >&2; exit 2 ;; esac
[[ "$(uname -s)" == Darwin ]] || { echo 'macOS builds require macOS.' >&2; exit 1; }
case "$(uname -m)" in arm64) arch=arm64 ;; x86_64) arch=x86_64 ;; *) echo 'Unsupported Mac architecture.' >&2; exit 1 ;; esac
cd "$repo_root"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/src-tauri/target}"
export MACOSX_DEPLOYMENT_TARGET=12.0

for triple in aarch64 x86_64; do
  sidecar="src-tauri/binaries/aria2-next-${triple}-apple-darwin"
  [[ -x "$sidecar" ]] || { echo "Missing executable sidecar: $sidecar" >&2; exit 1; }
  case "$triple" in
    aarch64) expected=be6e8ee76a8124fca03d20fa2a8446bac35ea0f24df7e159aa9206106bcfe47c ;;
    x86_64) expected=68b4c64c338b27b2bf929b9110393420908f79de4e33e6037973b38577b715a5 ;;
  esac
  [[ "$(shasum -a 256 "$sidecar" | awk '{print $1}')" == "$expected" ]] || { echo "Sidecar checksum mismatch: $sidecar" >&2; exit 1; }
done

if [[ "$mode" == release ]]; then
  pnpm exec tauri build --bundles app --ci
  profile=release
else
  pnpm run build
  # Include filenames and content, with NUL-delimited input for paths with spaces.
  export VERSADOWN_FRONTEND_SIGNATURE="$(find dist -type f -print0 | xargs -0 shasum -a 256 | LC_ALL=C sort | shasum -a 256 | awk '{print $1}')"
  cargo build --locked --manifest-path src-tauri/Cargo.toml --features custom-protocol
  [[ "$mode" == quick ]] && exit 0
  pnpm exec tauri bundle --debug --features custom-protocol --bundles app --ci
  profile=debug
fi

bundle_dir="$CARGO_TARGET_DIR/$profile/bundle"
app="$bundle_dir/macos/VersaDown.app"
[[ -d "$app" ]] || { echo "Missing app bundle: $app" >&2; exit 1; }
version="$(node -p "JSON.parse(require('fs').readFileSync('package.json', 'utf8')).version")"
output_dir="$repo_root/artifacts/macos-$arch"
mkdir -p "$output_dir"
suffix=""
[[ "$profile" == debug ]] && suffix=-local
asset="VersaDown_${version}_${arch}${suffix}"
# Local distributions need a complete ad-hoc bundle signature too. Sign only
# the host bundle: the pinned engine resources must retain their vendor bytes.
if [[ -z "${APPLE_SIGNING_IDENTITY:-}" ]]; then
  codesign --force --sign - "$app"
fi
codesign --verify --deep --strict "$app"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
ditto "$app" "$staging/VersaDown.app"
ln -s /Applications "$staging/Applications"
hdiutil create -ov -volname VersaDown -srcfolder "$staging" -format UDZO "$output_dir/$asset.dmg"
ditto -c -k --sequesterRsrc --keepParent "$app" "$output_dir/$asset.app.zip"
(
  cd "$output_dir"
  shasum -a 256 "$asset.app.zip" "$asset.dmg" > "SHA256SUMS.macos-$arch"
  shasum -a 256 --check "SHA256SUMS.macos-$arch"
)
echo "macOS packages: $output_dir"
