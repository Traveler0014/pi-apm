#!/usr/bin/env bash
# Offline tests for scripts/publish-release.sh: both branches (release missing /
# already present), the --clobber retry path, and a failing upload. A fake `gh`
# on PATH records argv and simulates `gh release view` exit codes, so this needs
# no network, no token and no GitHub repository.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

BIN="$WORK/bin"
mkdir -p "$BIN"
cat > "$BIN/gh" <<'FAKE'
#!/usr/bin/env bash
echo "$*" >> "$GH_LOG"
if [[ "$1 $2" == "release view" ]]; then
  [[ "${GH_EXISTS:-0}" == "1" ]] && exit 0 || exit 1
fi
if [[ "$1 $2" == "release upload" ]]; then
  [[ " $* " == *" --clobber "* ]] || { echo "upload without --clobber" >&2; exit 42; }
  [[ "${GH_UPLOAD_FAIL:-0}" == "1" ]] && exit 7
fi
exit 0
FAKE
chmod +x "$BIN/gh"

fail() { echo "FAIL $*" >&2; exit 1; }
run_case() {
  local name="$1" exists="$2"
  local dist="$WORK/dist-$name"
  mkdir -p "$dist"
  : > "$dist/apm-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
  : > "$dist/apm-0.2.0-aarch64-apple-darwin.tar.gz"
  : > "$dist/apm-0.2.0-x86_64-pc-windows-msvc.zip"
  printf 'deadbeef  apm-0.2.0-x86_64-unknown-linux-gnu.tar.gz\n' > "$dist/SHA256SUMS.txt"
  export GH_LOG="$WORK/gh-$name.log"
  : > "$GH_LOG"
  export GH_EXISTS="$exists"
  PATH="$BIN:$PATH" bash "$ROOT/scripts/publish-release.sh" v0.2.0 "$dist" >/dev/null
}

# 1. Release does not exist yet → create, never upload.
run_case create 0
grep -q '^release create v0.2.0 ' "$WORK/gh-create.log" || fail "expected release create"
grep -q -- '--generate-notes' "$WORK/gh-create.log" || fail "expected --generate-notes"
grep -q 'apm-0.2.0-x86_64-pc-windows-msvc.zip' "$WORK/gh-create.log" || fail "expected archives in create"
grep -q 'SHA256SUMS.txt' "$WORK/gh-create.log" || fail "expected checksums in create"
if grep -q '^release upload' "$WORK/gh-create.log"; then fail "create path must not upload"; fi
echo "PASS create path"

# 2. Release already exists → upload --clobber and sync the title, never create.
run_case existing 1
grep -q '^release upload v0.2.0 ' "$WORK/gh-existing.log" || fail "expected release upload"
grep -q -- '--clobber' "$WORK/gh-existing.log" || fail "expected --clobber on retry"
grep -q '^release edit v0.2.0 --title apm v0.2.0' "$WORK/gh-existing.log" || fail "expected title sync"
if grep -q '^release create' "$WORK/gh-existing.log"; then fail "existing path must not create"; fi
echo "PASS existing path (upload --clobber + title)"

# 3. A failing upload must fail the script (no silent success).
dist="$WORK/dist-failing"
mkdir -p "$dist"
: > "$dist/apm-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
: > "$dist/apm-0.2.0-aarch64-apple-darwin.tar.gz"
: > "$dist/SHA256SUMS.txt"
export GH_LOG="$WORK/gh-failing.log"
: > "$GH_LOG"
if GH_EXISTS=1 GH_UPLOAD_FAIL=1 PATH="$BIN:$PATH" bash "$ROOT/scripts/publish-release.sh" v0.2.0 "$dist" >/dev/null 2>&1; then
  fail "upload failure must propagate"
fi
echo "PASS failing upload propagates"

# 4. Missing aggregate checksums is refused before touching GitHub.
dist="$WORK/dist-nochecks"
mkdir -p "$dist"
: > "$dist/apm-0.2.0-x86_64-unknown-linux-gnu.tar.gz"
: > "$dist/apm-0.2.0-aarch64-apple-darwin.tar.gz"
export GH_LOG="$WORK/gh-nochecks.log"
: > "$GH_LOG"
if PATH="$BIN:$PATH" bash "$ROOT/scripts/publish-release.sh" v0.2.0 "$dist" >/dev/null 2>&1; then
  fail "missing SHA256SUMS.txt must fail"
fi
[[ ! -s "$GH_LOG" ]] || fail "must not call gh without checksums"
echo "PASS missing checksums refused before gh"

echo "test-release-scripts: PASS"
