#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
sidecar="$repo_root/src-tauri/binaries/aria2-next-x86_64-unknown-linux-gnu"
target_dir="$repo_root/.tools/linux-release-target"
bundle_dir="$target_dir/release/bundle"
output_dir="$repo_root/artifacts/linux"

if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
  echo 'Linux release packaging requires Linux x86_64.' >&2
  exit 1
fi
if [[ ! -x "$sidecar" ]]; then
  echo "Missing executable Linux aria2 sidecar: $sidecar" >&2
  exit 1
fi

export CARGO_TARGET_DIR="$target_dir"
marker="$(mktemp)"
trap 'rm -f "$marker"' EXIT

cd "$repo_root"
node scripts/prepare-video-tools.mjs
pnpm exec tauri build --bundles deb,rpm,appimage --config .tools/video-bundle.config.json --ci

mapfile -d '' deb_files < <(find "$bundle_dir/deb" -maxdepth 1 -type f -name '*.deb' -newer "$marker" -print0)
mapfile -d '' rpm_files < <(find "$bundle_dir/rpm" -maxdepth 1 -type f -name '*.rpm' -newer "$marker" -print0)
mapfile -d '' appimage_files < <(find "$bundle_dir/appimage" -maxdepth 1 -type f -name '*.AppImage' -newer "$marker" -print0)
if (( ${#deb_files[@]} != 1 )); then
  echo "Expected exactly one newly built .deb in $bundle_dir/deb; found ${#deb_files[@]}." >&2
  exit 1
fi
if (( ${#rpm_files[@]} != 1 )); then
  echo "Expected exactly one newly built .rpm in $bundle_dir/rpm; found ${#rpm_files[@]}." >&2
  exit 1
fi
if (( ${#appimage_files[@]} != 1 )); then
  echo "Expected exactly one newly built AppImage in $bundle_dir/appimage; found ${#appimage_files[@]}." >&2
  exit 1
fi

mkdir -p "$output_dir"
cp "${deb_files[0]}" "${rpm_files[0]}" "${appimage_files[0]}" "$output_dir/"
deb_name="$(basename "${deb_files[0]}")"
rpm_name="$(basename "${rpm_files[0]}")"
appimage_name="$(basename "${appimage_files[0]}")"
(
  cd "$output_dir"
  sha256sum "$deb_name" "$rpm_name" "$appimage_name" > SHA256SUMS
  sha256sum --check SHA256SUMS
)
echo "Linux release packages: $output_dir"
