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
  else
    echo "package-release: no sha256sum/shasum available" >&2
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
    if command -v 7z >/dev/null; then
      (cd "$STAGE" && 7z a -tzip "$ARCHIVE" "$PKG" >/dev/null)
    elif [[ -x "/c/Program Files/7-Zip/7z.exe" ]]; then
      (cd "$STAGE" && "/c/Program Files/7-Zip/7z.exe" a -tzip "$ARCHIVE" "$PKG" >/dev/null)
    elif command -v zip >/dev/null; then
      (cd "$STAGE" && zip -qr "$ARCHIVE" "$PKG")
    else
      echo "package-release: need 7z or zip to build a Windows archive" >&2
      exit 1
    fi
    ;;
  *)
    ARCHIVE="$OUT_DIR/$PKG.tar.gz"
    tar -czf "$ARCHIVE" -C "$STAGE" "$PKG"
    ;;
esac

echo "package: $PKG"
echo "archive: $ARCHIVE"
sha256_file "$ARCHIVE"
