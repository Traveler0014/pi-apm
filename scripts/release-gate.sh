#!/usr/bin/env bash
# Decide whether a release workflow run should build and publish.
#
# Inputs (environment; the workflow passes GitHub's own values):
#   GITHUB_EVENT_NAME  push | workflow_dispatch
#   GITHUB_REF         refs/heads/main or refs/tags/vX.Y.Z
#   GITHUB_REF_NAME    branch or tag name
#   VERSION_FILE       optional, default Cargo.toml
#
# Outputs (`key=value` lines, appended to $GITHUB_OUTPUT):
#   run=true|false       build at all (false when this version is already released)
#   publish=true|false   publish a GitHub Release (false for workflow_dispatch)
#   version, tag         the version from Cargo.toml and the v<version> tag
#   auto_tag=true|false  the tag does not exist yet and must be created
#
# A GITHUB_TOKEN-created tag does not trigger another workflow run, so this
# cannot recurse.
set -euo pipefail

EVENT="${GITHUB_EVENT_NAME:-}"
REF="${GITHUB_REF:-}"
REF_NAME="${GITHUB_REF_NAME:-}"
VERSION_FILE="${VERSION_FILE:-Cargo.toml}"

if [[ ! -f "$VERSION_FILE" ]]; then
  echo "release-gate: cannot read $VERSION_FILE" >&2
  exit 1
fi
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$VERSION_FILE" | head -1)"
if [[ -z "$version" ]]; then
  echo "release-gate: no version in $VERSION_FILE" >&2
  exit 1
fi

tag="v$version"
run=true
publish=false
auto_tag=false

case "$EVENT" in
  push)
    if [[ "$REF" == refs/tags/v* ]]; then
      tag="$REF_NAME"
      if [[ "$tag" != "v$version" ]]; then
        echo "release-gate: tag $tag does not match Cargo.toml version $version" >&2
        exit 1
      fi
      publish=true
    else
      tag_exists=false
      release_exists=false
      if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
        tag_exists=true
      fi
      if gh release view "$tag" >/dev/null 2>&1; then
        release_exists=true
      fi
      if [[ "$tag_exists" == "true" && "$release_exists" == "true" ]]; then
        # Same version already published: the whole release pipeline is skipped.
        # ci.yml still verifies the commit on its own.
        run=false
      else
        publish=true
        # Tag exists but its Release does not (a previous partial run): reuse it
        # and let the idempotent publish script create/update the Release.
        [[ "$tag_exists" == "true" ]] || auto_tag=true
      fi
    fi
    ;;
  workflow_dispatch)
    # Manual runs build artifacts for inspection and never publish.
    publish=false
    ;;
  *)
    echo "release-gate: unsupported event '${EVENT}'" >&2
    exit 1
    ;;
esac

printf 'run=%s\npublish=%s\nversion=%s\ntag=%s\nauto_tag=%s\n' \
  "$run" "$publish" "$version" "$tag" "$auto_tag"
