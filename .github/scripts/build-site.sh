#!/usr/bin/env bash
# Usage: build-site.sh <output-dir> [tag]
#
# Assembles the GitHub Pages site: the files in site/, the README screenshots
# and the app icon, with %TAG% and %VERSION% replaced by the latest release
# (or the given tag) and %CONTRIBUTORS% by the repository's contributors.
# Needs gh with a token unless a tag is passed; without one, the page leaves
# out its contributors section.
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

# People with commits, most first. Bots and AI coding agents are left out:
# GitHub counts commits by author, so an agent that commits as itself would
# otherwise be listed next to the people.
agents='claude|claude-code|copilot|copilot-swe-agent|devin-ai-integration|cursor|cursoragent'
agents+='|codex|chatgpt-codex-connector|google-labs-jules|gemini-code-assist|openhands-agent'
agents+='|aider|sweep-ai|codegen-sh'
people="$(mktemp)"
trap 'rm -f "$people"' EXIT
repo="${GITHUB_REPOSITORY:-$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || true)}"
if [[ -n "$repo" ]] &&
  gh api --paginate "repos/$repo/contributors?per_page=100" 2>/dev/null |
  jq -rs --arg agents "$agents" '
    add // []
    | .[]
    | select(.type == "User")
    | select(.login | test("\\[bot\\]$|^(" + $agents + ")$"; "i") | not)
    | "<li><a class=\"person\" href=\"\(.html_url | @html)\">"
      + "<img src=\"\(.avatar_url | @html)&amp;s=96\" width=\"48\" height=\"48\" alt=\"\" loading=\"lazy\">"
      + "<span class=\"pn\">\(.login | @html)</span>"
      + "<span class=\"pc\">\(.contributions) commit\(if .contributions == 1 then "" else "s" end)</span>"
      + "</a></li>"' >"$people" &&
  [[ -s "$people" ]]; then
  for page in $(find "$out" -name '*.html'); do
    awk -v list="$people" '
      /%CONTRIBUTORS%/ { while ((getline line < list) > 0) print line; close(list); next }
      { print }' "$page" >"$page.tmp"
    mv "$page.tmp" "$page"
  done
  echo "Listed $(wc -l <"$people" | tr -d ' ') contributors"
else
  echo "::warning::Could not list the contributors; the page leaves them out."
  find "$out" -name '*.html' -exec sed -i.bak -e '/<!-- contributors -->/,/<!-- \/contributors -->/d' {} +
fi

find "$out" -name '*.html' -exec sed -i.bak -e "s/%TAG%/$tag/g" -e "s/%VERSION%/$version/g" {} +
find "$out" -name '*.bak' -delete

if grep -rqE '%(TAG|VERSION|CONTRIBUTORS)%' "$out"; then
  echo "::error::A placeholder was left unreplaced."
  exit 1
fi
echo "Built the site for $tag in $out"
