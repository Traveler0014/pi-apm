#!/usr/bin/env bash
# Publish (or refresh) the GitHub Release for a tag from already-built assets.
#
# Idempotent on purpose: a re-run of the workflow, or a retry after a partial
# upload, must converge instead of failing with "release already exists".
#   - release missing  → gh release create <tag> <assets> --generate-notes
#   - release existing → gh release upload <tag> <assets> --clobber, then sync the title
#
# Notes are generated once at creation and kept on retries: regenerating them
# would rewrite the changelog every time a matrix job is retried.
#
# Usage: GH_TOKEN=… bash scripts/publish-release.sh <tag> <dist-dir>
# Requires: gh. Tests inject a fake `gh` on PATH (scripts/test-release-scripts.sh).
set -euo pipefail

TAG="${1:-}"
DIST="${2:-}"
if [[ -z "$TAG" || -z "$DIST" ]]; then
  sed -n '2,14p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 2
fi
[[ -d "$DIST" ]] || { echo "publish-release: no such directory: $DIST" >&2; exit 1; }
command -v gh >/dev/null || { echo "publish-release: gh is required" >&2; exit 1; }

if [[ ! -f "$DIST/SHA256SUMS.txt" ]]; then
  echo "publish-release: $DIST/SHA256SUMS.txt is missing (aggregate checksums)" >&2
  exit 1
fi

assets=()
for candidate in "$DIST"/*.tar.gz "$DIST"/*.zip "$DIST/SHA256SUMS.txt"; do
  [[ -f "$candidate" ]] && assets+=("$candidate")
done
if [[ ${#assets[@]} -lt 2 ]]; then
  echo "publish-release: expected archives plus checksums in $DIST" >&2
  exit 1
fi

TITLE="apm $TAG"
if gh release view "$TAG" >/dev/null 2>&1; then
  gh release upload "$TAG" "${assets[@]}" --clobber
  # Keep the human title in sync; notes stay as generated at creation.
  gh release edit "$TAG" --title "$TITLE"
  echo "publish-release: refreshed existing release $TAG (${#assets[@]} assets)"
else
  gh release create "$TAG" "${assets[@]}" --title "$TITLE" --generate-notes
  echo "publish-release: created release $TAG (${#assets[@]} assets)"
fi
