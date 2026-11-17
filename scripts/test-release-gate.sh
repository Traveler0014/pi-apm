#!/usr/bin/env bash
# Offline tests for scripts/release-gate.sh (fake git/gh, temp Cargo.toml).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
BIN="$WORK/bin"
mkdir -p "$BIN"
printf '[package]\nversion = "1.2.3"\n' > "$WORK/Cargo.toml"

cat > "$BIN/git" <<'FAKE'
#!/usr/bin/env bash
echo "git $*" >> "${FAKE_LOG:?}"
exit "${FAKE_TAG_EXISTS:-2}"
FAKE
cat > "$BIN/gh" <<'FAKE'
#!/usr/bin/env bash
echo "gh $*" >> "${FAKE_LOG:?}"
exit "${FAKE_RELEASE_EXISTS:-1}"
FAKE
chmod +x "$BIN/git" "$BIN/gh"

fail() { echo "FAIL $*" >&2; exit 1; }
gate() {
  env PATH="$BIN:$PATH" FAKE_LOG="$WORK/log" VERSION_FILE="$WORK/Cargo.toml" \
    VERSION=1.2.3 "$ROOT/scripts/release-gate.sh"
}
expect_gate() {
  local name="$1" event="$2" ref="$3" ref_name="$4" tag_exists="$5" release_exists="$6"
  shift 6
  local out
  out="$(GITHUB_EVENT_NAME="$event" GITHUB_REF="$ref" GITHUB_REF_NAME="$ref_name" \
    FAKE_TAG_EXISTS="$tag_exists" FAKE_RELEASE_EXISTS="$release_exists" gate)"
  for pair in "$@"; do
    grep -qx "$pair" <<<"$out" || fail "$name: expected $pair in: $out"
  done
  echo "PASS $name"
}

expect_gate "push main, already released (tag + release)" \
  push refs/heads/main main 0 0 run=false publish=false auto_tag=false
expect_gate "push main, tag exists but release missing" \
  push refs/heads/main main 0 1 run=true publish=true auto_tag=false
expect_gate "push main, new version" \
  push refs/heads/main main 2 1 run=true publish=true auto_tag=true
expect_gate "tag push matching Cargo.toml" \
  push refs/tags/v1.2.3 v1.2.3 0 0 run=true publish=true auto_tag=false
expect_gate "workflow_dispatch builds but never publishes" \
  workflow_dispatch refs/heads/main main 2 1 run=true publish=false auto_tag=false

if GITHUB_EVENT_NAME=push GITHUB_REF=refs/tags/v9.9.9 GITHUB_REF_NAME=v9.9.9 \
  FAKE_TAG_EXISTS=0 FAKE_RELEASE_EXISTS=0 gate >"$WORK/mismatch.log" 2>&1; then
  fail "mismatched tag must fail"
fi
grep -q "does not match Cargo.toml version" "$WORK/mismatch.log" || fail "mismatch error unclear"
echo "PASS mismatched tag is refused"

if GITHUB_EVENT_NAME=schedule FAKE_LOG="$WORK/log" gate >/dev/null 2>&1; then
  fail "unsupported event must fail"
fi
echo "PASS unsupported event is refused"

echo "test-release-gate: PASS"
