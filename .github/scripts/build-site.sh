#!/usr/bin/env bash
# Usage: build-site.sh <output-dir> [tag]
#
# Assembles the GitHub Pages site: the files in site/, the README screenshots
# and the app icon, with %TAG% and %VERSION% replaced by the latest release
# (or the given tag). Needs gh with a token unless a tag is passed.
set -euo pipefail

out="$1"
tag="${2:-$(gh release view --json tagName --jq .tagName)}"
version="${tag#v}"

# The download links name these files; refuse to publish links that 404.
if [[ -z "${2:-}" ]]; then
  assets="$(gh release view "$tag" --json assets --jq '.assets[].name')"
  for file in \
    "kuery-$tag-universal-apple-darwin.dmg" \
    "kuery-$tag-x86_64-pc-windows-msvc.zip" \
    "kuery-$tag-x86_64-unknown-linux-gnu.tar.gz" \
    SHA256SUMS; do
    if ! grep -qxF "$file" <<<"$assets"; then
      echo "::error::Release $tag has no $file; the download links would be broken."
      exit 1
    fi
  done
fi

rm -rf "$out"
mkdir -p "$out/assets/screenshots"
cp -R site/. "$out/"
cp docs/screenshots/{file-sql,file-data,file-profile,database-browser}.jpg \
  docs/screenshots/{plan-graph,write-review,redis-console}.png \
  "$out/assets/screenshots/"
cp crates/app/assets/branding/kuery.png "$out/assets/kuery.png"
touch "$out/.nojekyll"

find "$out" -name '*.html' -exec sed -i.bak -e "s/%TAG%/$tag/g" -e "s/%VERSION%/$version/g" {} +
find "$out" -name '*.bak' -delete

if grep -rqE '%(TAG|VERSION)%' "$out"; then
  echo "::error::A version placeholder was left unreplaced."
  exit 1
fi
echo "Built the site for $tag in $out"
