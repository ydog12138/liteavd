#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

cargo_version="$({
  sed -n '/^\[package\]/,/^\[/{s/^version = "\([^"]*\)"/\1/p;}' Cargo.toml
} | head -n 1)"
release_version="${1:-$cargo_version}"
appstream_version="$(
  xmllint --xpath 'string(/component/releases/release[1]/@version)' \
    data/io.github.ydog12138.liteavd.metainfo.xml
)"

if [[ -z "$cargo_version" || -z "$release_version" || -z "$appstream_version" ]]; then
  echo "Could not resolve Cargo/AppStream release versions" >&2
  exit 1
fi

if [[ "$release_version" != "$cargo_version" || "$release_version" != "$appstream_version" ]]; then
  echo "Release version mismatch: requested=$release_version Cargo=$cargo_version AppStream=$appstream_version" >&2
  exit 1
fi

app_id="io.github.ydog12138.liteavd"
arch="$(flatpak --default-arch)"
build_dir="${LITEAVD_FLATPAK_BUILD_DIR:-build/github-flatpak}"
repo_dir="${LITEAVD_FLATPAK_REPO_DIR:-build/github-flatpak-repo}"
dist_dir="${LITEAVD_FLATPAK_DIST_DIR:-dist}"
bundle_name="liteavd-${release_version}-${arch}.flatpak"
bundle_path="$dist_dir/$bundle_name"

mkdir -p "$dist_dir"

flatpak-builder \
  --user \
  --force-clean \
  --disable-rofiles-fuse \
  --install-deps-from=flathub \
  --repo="$repo_dir" \
  "$build_dir" \
  io.github.ydog12138.liteavd.yml

temporary_bundle="$(mktemp "$dist_dir/.${bundle_name}.XXXXXX")"
trap 'rm -f "$temporary_bundle"' EXIT

flatpak build-bundle \
  --arch="$arch" \
  --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
  "$repo_dir" \
  "$temporary_bundle" \
  "$app_id" \
  master

mv -f "$temporary_bundle" "$bundle_path"
trap - EXIT

(
  cd "$dist_dir"
  sha256sum "$bundle_name" >"$bundle_name.sha256"
)

printf '%s\n' "$bundle_path" "$bundle_path.sha256"
