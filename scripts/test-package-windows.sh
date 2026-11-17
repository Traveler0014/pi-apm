#!/usr/bin/env bash
# Offline regression for the Windows branch of package-release.sh.
#
# On GitHub's Windows runner a native 7z.exe receives a POSIX `/d/a/...` path
# from Git Bash unless it is converted with cygpath; that was the Package-step
# failure in release run 44054797594. This test fakes cygpath/7z on Linux:
#   - 7z must receive a Windows-style path (Z:\...), not the POSIX one;
#   - the produced zip must still contain the same six files and verify with
#     scripts/verify-package.sh;
#   - with no zip tool at all the script must fail with a clear message.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
FAIL=0

fail() { echo "FAIL $*" >&2; FAIL=1; }

VERSION="0.1.0"
TARGET="x86_64-pc-windows-msvc"
PKG="apm-$VERSION-$TARGET"

mkdir -p "$WORK/bin" "$WORK/fake" "$WORK/out" "$WORK/out-fail"
for exe in apm apm-mcp apm-serve; do
  printf 'binary' > "$WORK/bin/$exe.exe"
done

# --- fake cygpath: POSIX -> drive-letter Windows path ----------------------
cat > "$WORK/fake/cygpath" <<'FAKE'
#!/usr/bin/env bash
echo "cygpath $*" >> "${FAKE_LOG:?}"
[[ "${1:-}" == "-w" ]] || { printf '%s\n' "${2:-}"; exit 0; }
path="${2#/}"
printf 'Z:\\%s\n' "${path//\//\\}"
FAKE

# --- fake 7z: records argv, then writes the real zip via python ------------
cat > "$WORK/fake/7z" <<'FAKE'
#!/usr/bin/env bash
echo "7z $*" >> "${FAKE_LOG:?}"
[[ "${NOOP:-0}" == "1" ]] && exit 0
archive="$3"
package="$4"
native="${archive#Z:}"
native="${native//\\//}"
python3 - "$native" "$package" <<'PY'
import os, sys, zipfile
archive, package = sys.argv[1], sys.argv[2]
parent = os.path.dirname(archive)
if parent:
    os.makedirs(parent, exist_ok=True)
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
    for directory, _, files in os.walk(package):
        for name in sorted(files):
            full = os.path.join(directory, name)
            bundle.write(full, os.path.relpath(full, "."))
PY
FAKE
chmod +x "$WORK/fake/cygpath" "$WORK/fake/7z"

export FAKE_LOG="$WORK/fake.log"
: > "$FAKE_LOG"
PATH="$WORK/fake:$PATH" bash "$ROOT/scripts/package-release.sh" \
  --target "$TARGET" --version "$VERSION" \
  --bin-dir "$WORK/bin" --outdir "$WORK/out" > "$WORK/package.log" 2>&1 || {
    cat "$WORK/package.log" >&2
    fail "package-release.sh failed with fake 7z/cygpath"
  }

ARCHIVE="$WORK/out/$PKG.zip"
[[ -f "$ARCHIVE" ]] || fail "missing $ARCHIVE"
if [[ -f "$ARCHIVE" ]]; then
  if grep -q '^cygpath -w ' "$FAKE_LOG"; then
    echo "PASS cygpath -w converted the archive path"
  else
    fail "package-release.sh never called cygpath -w"
  fi
  if grep -q "7z a -tzip Z:\\\\" "$FAKE_LOG" && grep -q "$PKG.zip" "$FAKE_LOG"; then
    echo "PASS 7z received a Windows-style archive path"
  else
    fail "7z did not receive a Windows-style path: $(cat "$FAKE_LOG")"
  fi
  if grep -q "7z a -tzip /" "$FAKE_LOG"; then
    fail "7z was given a POSIX path: $(cat "$FAKE_LOG")"
  fi
  names="$(python3 - "$ARCHIVE" <<'PY'
import sys, zipfile
print("\n".join(sorted(zipfile.ZipFile(sys.argv[1]).namelist())))
PY
)"
  expected="$(printf '%s\n' \
    "$PKG/LICENSE" "$PKG/README.md" "$PKG/SHA256SUMS" \
    "$PKG/apm-mcp.exe" "$PKG/apm-serve.exe" "$PKG/apm.exe" | LC_ALL=C sort)"
  if [[ "$names" == "$expected" ]]; then
    echo "PASS archive keeps the six expected paths inside $PKG/"
  else
    fail "unexpected archive contents: $names"
  fi
  if bash "$ROOT/scripts/verify-package.sh" "$ARCHIVE" --skip-run > "$WORK/verify.log" 2>&1; then
    echo "PASS verify-package accepts the Windows zip"
  else
    cat "$WORK/verify.log" >&2
    fail "verify-package rejected the Windows zip"
  fi
fi

# --- relative --outdir: the real workflow passes `--outdir dist` ----------
# A relative archive path is resolved by the native tool against its own cwd
# (the staging directory), which is how the zip silently landed nowhere. The
# fake 7z has real semantics: it resolves the path it is given, from its cwd.
mkdir -p "$WORK/cwd"
: > "$FAKE_LOG"
(
  cd "$WORK/cwd"
  PATH="$WORK/fake:$PATH" bash "$ROOT/scripts/package-release.sh" \
    --target "$TARGET" --version "$VERSION" \
    --bin-dir "$WORK/bin" --outdir dist > "$WORK/relative.log" 2>&1
) || {
  cat "$WORK/relative.log" >&2
  fail "relative --outdir failed"
}
RELATIVE_ARCHIVE="$WORK/cwd/dist/$PKG.zip"
if [[ -f "$RELATIVE_ARCHIVE" ]]; then
  echo "PASS relative --outdir writes $PKG.zip into that directory"
else
  fail "relative --outdir produced no archive (7z: $(grep '7z a' "$FAKE_LOG" || true))"
fi
if grep -q '7z a -tzip Z:.*cwd.*dist' "$FAKE_LOG"; then
  echo "PASS relative --outdir was made absolute before cygpath"
else
  fail "relative --outdir was not resolved to an absolute native path: $(cat "$FAKE_LOG")"
fi
if [[ -f "$RELATIVE_ARCHIVE" ]]; then
  count="$(python3 -c 'import sys, zipfile; print(len(zipfile.ZipFile(sys.argv[1]).namelist()))' "$RELATIVE_ARCHIVE")"
  [[ "$count" == "6" ]] || fail "relative archive has $count files, expected 6"
  echo "PASS relative archive still holds the six files"
fi

# --- a tool that exits 0 but writes nothing: name the archive, not sha256sum --
: > "$FAKE_LOG"
if NOOP=1 PATH="$WORK/fake:$PATH" bash "$ROOT/scripts/package-release.sh" \
  --target "$TARGET" --version "$VERSION" \
  --bin-dir "$WORK/bin" --outdir "$WORK/out-noop" > "$WORK/noop.log" 2>&1; then
  fail "a tool that creates no archive must fail the step"
else
  if grep -q "without creating" "$WORK/noop.log" && grep -q ".zip" "$WORK/noop.log"; then
    echo "PASS silent tool failure names the missing archive"
  else
    fail "silent tool failure is unclear: $(cat "$WORK/noop.log")"
  fi
  if grep -qi "sha256sum" "$WORK/noop.log"; then
    fail "silent tool failure fell through to sha256sum"
  fi
fi

# --- no zip tool at all: clear error --------------------------------------
mkdir -p "$WORK/toolbin"
for tool in bash sh dirname sed head mktemp mkdir cp rm sha256sum cat; do
  ln -s "$(command -v "$tool")" "$WORK/toolbin/$tool"
done
cat > "$WORK/toolbin/rustc" <<'FAKE'
#!/usr/bin/env bash
if [[ "${1:-}" == "-vV" ]]; then
  echo "rustc 1.100.0"
  echo "host: x86_64-unknown-linux-gnu"
fi
FAKE
chmod +x "$WORK/toolbin/rustc"
if env PATH="$WORK/toolbin" /bin/bash "$ROOT/scripts/package-release.sh" \
  --target "$TARGET" --version "$VERSION" \
  --bin-dir "$WORK/bin" --outdir "$WORK/out-fail" > "$WORK/failing.log" 2>&1; then
  fail "package-release.sh succeeded without any zip tool"
else
  if grep -q "cannot build the Windows zip" "$WORK/failing.log"; then
    echo "PASS missing zip tool fails with a clear message"
  else
    fail "unclear error without zip tools: $(cat "$WORK/failing.log")"
  fi
fi

[[ $FAIL -eq 0 ]] || exit 1
echo "test-package-windows: PASS"
