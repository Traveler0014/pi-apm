#!/usr/bin/env bash
# Verify a release package locally: structure, checksums, then — when the
# package matches this machine — `--version` for all three binaries and a real
# CLI round-trip (init → create → get → dashboard).
#
#   bash scripts/verify-package.sh dist/apm-0.1.0-x86_64-unknown-linux-gnu.tar.gz
#   bash scripts/verify-package.sh dist/package-dir --skip-run
#
# Never needs network access and never publishes anything.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INPUT="${1:-}"
SKIP_RUN=0
if [[ "${2:-}" == "--skip-run" || "${1:-}" == "--skip-run" ]]; then
  SKIP_RUN=1
fi
if [[ -z "$INPUT" || "$INPUT" == "--skip-run" || "$INPUT" == "-h" || "$INPUT" == "--help" ]]; then
  sed -n '2,12p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
fi
if [[ "$INPUT" != /* ]]; then
  INPUT="$PWD/$INPUT"
fi
if [[ ! -e "$INPUT" ]]; then
  echo "verify-package: no such file or directory: $INPUT" >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

archive_name="$(basename "$INPUT")"
case "$archive_name" in
  *.tar.gz|*.tgz) mkdir -p "$WORK/unpacked"; tar -xzf "$INPUT" -C "$WORK/unpacked" ;;
  *.zip)
    mkdir -p "$WORK/unpacked"
    if command -v unzip >/dev/null; then
      unzip -q "$INPUT" -d "$WORK/unpacked"
    else
      python3 -c 'import sys,zipfile;zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' "$INPUT" "$WORK/unpacked"
    fi
    ;;
  *)
    if [[ -d "$INPUT" ]]; then
      mkdir -p "$WORK/unpacked"
      cp -R "$INPUT" "$WORK/unpacked/"
    else
      echo "verify-package: expected .tar.gz, .zip or a directory" >&2
      exit 1
    fi
    ;;
esac

# bash 3.2 (macOS) has no mapfile.
tops=()
while IFS= read -r dir; do
  tops+=("$dir")
done < <(find "$WORK/unpacked" -mindepth 1 -maxdepth 1 -type d | sed 's|.*/||' | sort)
if [[ ${#tops[@]} -ne 1 ]]; then
  echo "verify-package: expected exactly one top-level directory, found: ${tops[*]:-none}" >&2
  exit 1
fi
PKG="${tops[0]}"
DIR="$WORK/unpacked/$PKG"
echo "package: $PKG"

EXE=""
case "$PKG" in
  *windows*) EXE=".exe" ;;
esac

missing=0
for required in "apm$EXE" "apm-mcp$EXE" "apm-serve$EXE" README.md LICENSE SHA256SUMS; do
  if [[ ! -f "$DIR/$required" ]]; then
    echo "MISSING $required" >&2
    missing=1
  fi
done
[[ $missing -eq 0 ]] || { echo "verify-package: incomplete package" >&2; exit 1; }
echo "structure: three binaries + README.md + LICENSE + SHA256SUMS"

# Nothing that looks like local state or a build artefact may ship.
unexpected="$(find "$DIR" -maxdepth 2 \( -name '.git' -o -name '.apm' -o -name 'target' -o -name 'node_modules' -o -name '*.db' -o -name '*.db-wal' -o -name '*.db-shm' -o -name '*token*' -o -name '*.log' \) -print)"
if [[ -n "$unexpected" ]]; then
  echo "verify-package: package contains forbidden files:" >&2
  echo "$unexpected" >&2
  exit 1
fi
# The only files allowed at the top level are the documented ones.
extra="$(find "$DIR" -mindepth 1 -maxdepth 1 ! -name "apm$EXE" ! -name "apm-mcp$EXE" ! -name "apm-serve$EXE" ! -name README.md ! -name LICENSE ! -name SHA256SUMS -print)"
if [[ -n "$extra" ]]; then
  echo "verify-package: unexpected top-level entries:" >&2
  echo "$extra" >&2
  exit 1
fi
echo "contents: no local state, build artefacts or extra files"

if command -v sha256sum >/dev/null; then
  ( cd "$DIR" && sha256sum -c SHA256SUMS >/dev/null )
elif command -v shasum >/dev/null; then
  ( cd "$DIR" && shasum -a 256 -c SHA256SUMS >/dev/null )
else
  echo "verify-package: no sha256sum/shasum available" >&2
  exit 1
fi
echo "checksums: SHA256SUMS matches every packaged file"

HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
run_ok=0
if [[ "$PKG" == *-"$HOST_TARGET" ]]; then
  run_ok=1
fi
if [[ $SKIP_RUN -eq 1 ]]; then
  echo "run: skipped (--skip-run)"
elif [[ $run_ok -eq 0 ]]; then
  echo "run: package targets a different platform than $HOST_TARGET — skipped"
else
  for bin in apm apm-mcp apm-serve; do
    "$DIR/$bin$EXE" --version >/dev/null
    echo "run: $bin --version ok"
  done
  SMOKE="$(mktemp -d)"
  trap 'rm -rf "$WORK" "$SMOKE"' EXIT
  export APM_DB="$SMOKE/.apm/apm.db"
  "$DIR/apm$EXE" init --name ReleaseCheck --prefix RELCM --json >/dev/null
  ID="$("$DIR/apm$EXE" create --title "release smoke" --goal "verify the package" \
        --current-state "fresh" --json | python3 -c "import json,sys;print(json.load(sys.stdin)['item']['identifier'])")"
  "$DIR/apm$EXE" get "$ID" --json >/dev/null
  "$DIR/apm$EXE" dashboard --json >/dev/null
  "$DIR/apm$EXE" ui --help >/dev/null
  echo "run: CLI smoke ok (init → create → get → dashboard, ui --help)"
fi

echo "verify-package: PASS ($PKG)"
