#!/usr/bin/env bash
# Usage: notarize.sh <file.zip|file.dmg>
#
# Submits the file to Apple's notary service and waits for the verdict.
# Expects APPLE_API_KEY_PATH, APPLE_API_KEY_ID and APPLE_API_ISSUER_ID.
set -euo pipefail

file="$1"
auth=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER_ID")

# Submit first and wait separately, so the submission ID is known even when
# Apple is slow (a new team's first submissions can take hours; later ones
# usually take minutes).
id="$(xcrun notarytool submit "$file" "${auth[@]}" --output-format json | jq -r .id)"
if [[ -z "$id" || "$id" == null ]]; then
  echo "::error::notarytool did not return a submission ID for $file."
  exit 1
fi
echo "Notarization submission for $(basename "$file"): $id"

# Poll instead of `notarytool wait`, which gives up on the first network
# error; a failed check is simply retried.
status=""
deadline=$((SECONDS + 2 * 60 * 60))
while (( SECONDS < deadline )); do
  status="$(xcrun notarytool info "$id" "${auth[@]}" --output-format json 2>/dev/null | jq -r '.status // empty' || true)"
  if [[ -n "$status" && "$status" != "In Progress" ]]; then
    break
  fi
  echo "$(date -u +%H:%M:%S) status: ${status:-unavailable, retrying}"
  sleep 30
done
if [[ -z "$status" || "$status" == "In Progress" ]]; then
  echo "::error::Notarization submission $id did not finish within 2 hours; check it with 'xcrun notarytool info $id'."
  exit 1
fi
if [[ "$status" != Accepted ]]; then
  xcrun notarytool log "$id" "${auth[@]}" || true
  echo "::error::Notarization submission $id finished with status $status."
  exit 1
fi
