#!/usr/bin/env bash
# Build a distributable archive for one target triple.
#
# The package contains the three binaries, README.md, LICENSE and a SHA256SUMS
# file covering exactly those files — never a database, token file, build
# directory or VCS metadata.
#
#   bash scripts/package-release.sh                        # host target
#   bash scripts/package-release.sh --target aarch64-apple-darwin \
#        --bin-dir target/aarch64-apple-darwin/release --outdir dist
#
# Archives are `.tar.gz` everywhere except Windows targets, which use `.zip`.
# The script prints the archive path and its sha256 on the last two lines.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET=""
BIN_DIR=""
OUT_DIR="$ROOT/dist"
VERSION=""

usage() {
  sed -n '2,14p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while (($#)); do
  case "$1" in
    --target) TARGET="${2:?--target needs a value}"; shift 2 ;;
    --bin-dir) BIN_DIR="${2:?--bin-dir needs a value}"; shift 2 ;;
    --outdir) OUT_DIR="${2:?--outdir needs a value}"; shift 2 ;;
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "package-release: unknown argument: $1" >&2; exit 2 ;;
  esac
done

command -v rustc >/dev/null || { echo "package-release: rustc is required to detect the host target" >&2; exit 1; }
HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
if [[ -z "$TARGET" ]]; then
  TARGET="$HOST_TARGET"
fi
if [[ -z "$VERSION" ]]; then
  VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
fi
[[ -n "$VERSION" ]] || { echo "package-release: cannot read the version from Cargo.toml" >&2; exit 1; }

if [[ -z "$BIN_DIR" ]]; then
  if [[ "$TARGET" == "$HOST_TARGET" && -x "$ROOT/target/release/apm" ]]; then
    BIN_DIR="$ROOT/target/release"
  else
    BIN_DIR="$ROOT/target/$TARGET/release"
  fi
fi

EXE=""
case "$TARGET" in
  *windows*) EXE=".exe" ;;
esac

PKG="apm-$VERSION-$TARGET"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DEST="$STAGE/$PKG"
mkdir -p "$DEST"

for bin in apm apm-mcp apm-serve; do
  source="$BIN_DIR/$bin$EXE"
  if [[ ! -f "$source" ]]; then
    echo "package-release: missing $source (build with --target $TARGET first)" >&2
    exit 1
  fi
  cp "$source" "$DEST/"
done
cp "$ROOT/README.md" "$ROOT/LICENSE" "$DEST/"

sha256_file() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$@"
  elif command -v shasum >/dev/null; then
    shasum -a 256 "$@"
  elif command -v certutil >/dev/null; then
    # Git Bash on Windows normally ships sha256sum; certutil is the native
    # fallback if a stripped PATH hides it. Output matches `sha256sum` format
    # so the packaged SHA256SUMS stays portable.
    local file hash
    for file in "$@"; do
      hash="$(certutil -hashfile "$file" SHA256 | sed -n '2p' | tr -d ' \r')"
      if [[ ${#hash} -ne 64 ]]; then
        echo "package-release: certutil did not return a SHA256 for $file" >&2
        exit 1
      fi
      printf '%s  %s\n' "$hash" "$file"
    done
  else
    echo "package-release: no sha256sum/shasum/certutil available for checksums" >&2
    exit 1
  fi
}

# A native Windows tool (7z.exe, bsdtar, python.exe) cannot open the POSIX
# `/d/a/...` paths Git Bash uses. Convert only when cygpath is present; on Linux
# and macOS this is the identity, which keeps the regression test meaningful.
native_path() {
  if command -v cygpath >/dev/null; then
    cygpath -w "$1"
  else
    printf '%s' "$1"
  fi
}

build_windows_zip() {
  local archive="$1" native
  native="$(native_path "$archive")"
  if command -v 7z >/dev/null; then
    (cd "$STAGE" && 7z a -tzip "$native" "$PKG" >/dev/null)
  elif [[ -x "/c/Program Files/7-Zip/7z.exe" ]]; then
    (cd "$STAGE" && "/c/Program Files/7-Zip/7z.exe" a -tzip "$native" "$PKG" >/dev/null)
  elif command -v bsdtar >/dev/null; then
    (cd "$STAGE" && bsdtar -a -c -f "$native" "$PKG")
  elif command -v tar >/dev/null && tar --version 2>/dev/null | head -1 | grep -qi bsdtar; then
    (cd "$STAGE" && tar -a -c -f "$native" "$PKG")
  elif command -v python3 >/dev/null; then
    (cd "$STAGE" && python3 - "$native" "$PKG" <<'PY'
import os, sys, zipfile
archive, package = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
    for directory, _, files in os.walk(package):
        for name in sorted(files):
            full = os.path.join(directory, name)
            bundle.write(full, os.path.relpath(full, "."))
PY
    )
  elif command -v python >/dev/null; then
    (cd "$STAGE" && python - "$native" "$PKG" <<'PY'
import os, sys, zipfile
archive, package = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as bundle:
    for directory, _, files in os.walk(package):
        for name in sorted(files):
            full = os.path.join(directory, name)
            bundle.write(full, os.path.relpath(full, "."))
PY
    )
  elif command -v zip >/dev/null; then
    (cd "$STAGE" && zip -qr "$archive" "$PKG")
  else
    echo "package-release: cannot build the Windows zip: install 7-Zip, or provide bsdtar, Python 3 or zip on PATH" >&2
    exit 1
  fi
}

# Checksums cover the executables and the two documents, relative to the
# package root so `sha256sum -c SHA256SUMS` works after extraction.
(
  cd "$DEST"
  # shellcheck disable=SC2086  # $EXE is intentionally empty on non-Windows
  sha256_file "apm$EXE" "apm-mcp$EXE" "apm-serve$EXE" "README.md" "LICENSE" > SHA256SUMS
)

mkdir -p "$OUT_DIR"
ARCHIVE=""
case "$TARGET" in
  *windows*)
    ARCHIVE="$OUT_DIR/$PKG.zip"
    rm -f "$ARCHIVE"
    build_windows_zip "$ARCHIVE"
    ;;
  *)
    ARCHIVE="$OUT_DIR/$PKG.tar.gz"
    tar -czf "$ARCHIVE" -C "$STAGE" "$PKG"
    ;;
esac

echo "package: $PKG"
echo "archive: $ARCHIVE"
sha256_file "$ARCHIVE"
