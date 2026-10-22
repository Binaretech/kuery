#!/usr/bin/env bash
# Usage: update-manifest.sh <dist dir> <version> <tag> <owner/repo>
#
# Signs the archives the in-app updater installs with minisign and writes
# <dist dir>/latest.json, which the app reads from
# https://github.com/<owner/repo>/releases/latest/download/latest.json.
# Expects UPDATE_SIGNING_KEY_PATH (the minisign secret key, without a
# password) and checks every signature against crates/app/assets/update.pub,
# so a secret that does not match the key built into the app fails here.
set -euo pipefail

dist="$1"
version="$2"
tag="$3"
repo="$4"
public_key="$(dirname "$0")/../../crates/app/assets/update.pub"
base_url="https://github.com/$repo/releases/download/$tag"

# Updater platform → archive suffix. Both Mac architectures use the
# universal app.
platforms=(
  "darwin-aarch64:universal-apple-darwin.zip"
  "darwin-x86_64:universal-apple-darwin.zip"
  "linux-x86_64:x86_64-unknown-linux-gnu.tar.gz"
  "windows-x86_64:x86_64-pc-windows-msvc.zip"
)

entries='{}'
for platform in "${platforms[@]}"; do
  name="${platform%%:*}"
  file="kuery-$tag-${platform#*:}"
  path="$dist/$file"
  if [[ ! -f "$path" ]]; then
    echo "::error::Missing $file for the $name update."
    exit 1
  fi
  if [[ ! -f "$path.minisig" ]]; then
    # The trusted comment is signed too; it ties the signature to this
    # version and file name.
    minisign -S -s "$UPDATE_SIGNING_KEY_PATH" -m "$path" -x "$path.minisig" \
      -t "kuery $version $file" >/dev/null
    minisign -V -q -p "$public_key" -m "$path" -x "$path.minisig"
  fi
  entries="$(jq -c \
    --arg name "$name" \
    --arg url "$base_url/$file" \
    --arg sha256 "$(sha256sum "$path" | cut -d' ' -f1)" \
    --arg signature "$(cat "$path.minisig")" \
    '.[$name] = {url: $url, sha256: $sha256, signature: $signature}' <<<"$entries")"
done

jq -n \
  --arg version "$version" \
  --arg notes_url "https://github.com/$repo/releases/tag/$tag" \
  --arg pub_date "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --argjson platforms "$entries" \
  '{version: $version, notes_url: $notes_url, pub_date: $pub_date, platforms: $platforms}' \
  > "$dist/latest.json"
echo "Wrote $dist/latest.json for $version ($(jq -r '.platforms | keys | join(", ")' "$dist/latest.json"))"
