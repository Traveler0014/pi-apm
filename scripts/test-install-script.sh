#!/usr/bin/env bash
# Offline tests for scripts/install.sh.
#
# Everything is local: fixture `apm-<version>-<target>.tar.gz` archives with real
# SHA-256 checksums, a fake `curl` that serves those files from a temp directory
# (and answers the `/releases/latest` redirect query), and injected `uname`
# values. No network, no gh CLI, no GitHub account.
#
# Covered:
#   * the four supported platform triples and clear failures for unsupported ones;
#   * latest-tag resolution plus explicit --version with and without the `v`;
#   * a normal install (three binaries, mode 0755, version output, PATH hint);
#   * upgrading an existing older binary in place;
#   * rejecting a mismatched aggregate checksum and a tampered in-package
#     SHA256SUMS, without touching the destination directory;
#   * input validation for --version/--repo (no path or URL injection).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

VERSION="0.2.0"
TARGETS=(x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin)

fail() {
    echo "FAIL $*" >&2
    exit 1
}
pass() { echo "PASS $*"; }
mode_of() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }

# ---------------------------------------------------------------------------
# Fake curl: serves fixtures by URL basename and answers the latest-redirect
# probe. Records every real download so the tests can assert the requested
# archive name.
# ---------------------------------------------------------------------------
FAKE_CURL="$WORK/bin/curl"
mkdir -p "$WORK/bin"
cat > "$FAKE_CURL" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
dest=""
url=""
want_effective=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o)
            dest="${2:-}"
            shift 2
            ;;
        -w)
            case "${2:-}" in *url_effective*) want_effective=1 ;; esac
            shift 2
            ;;
        --retry)
            shift 2
            ;;
        -*)
            shift
            ;;
        *)
            url="$1"
            shift
            ;;
    esac
done
if [ "$want_effective" = 1 ]; then
    printf 'LATEST %s\n' "${FAKE_LATEST_URL:?FAKE_LATEST_URL is required}" >> "${FAKE_CURL_LOG:-/dev/null}"
    printf '%s\n' "$FAKE_LATEST_URL"
    exit 0
fi
[ -n "$dest" ] || { echo "fake curl: missing -o" >&2; exit 2; }
printf '%s\n' "$url" >> "${FAKE_CURL_LOG:-/dev/null}"
name="${url##*/}"
src="${FAKE_RELEASES_DIR:?FAKE_RELEASES_DIR is required}/$name"
[ -f "$src" ] || { echo "fake curl: 404 $url" >&2; exit 22; }
cp "$src" "$dest"
FAKE
chmod +x "$FAKE_CURL"

# Fake wget: the install.sh fallback path. `--spider` prints a CRLF-terminated
# redirect and exits 8 (what wget really does for an unfollowed 302); anything
# else is a plain download from the fixture directory.
FAKE_WGET="$WORK/bin/wget"
cat > "$FAKE_WGET" <<'FAKE'
#!/usr/bin/env bash
set -euo pipefail
spider=0
for arg in "$@"; do
    [ "$arg" = "--spider" ] && spider=1
done
if [ "$spider" = 1 ]; then
    printf '  HTTP/1.1 302 Found\r\n  Location: %s\r\n  Content-Length: 0\r\n\r\n' \
        "${FAKE_LATEST_URL:?FAKE_LATEST_URL is required}" >&2
    exit 8
fi
dest=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -O)
            dest="${2:-}"
            shift 2
            ;;
        -*)
            shift
            ;;
        *)
            url="$1"
            shift
            ;;
    esac
done
[ -n "$dest" ] || { echo "fake wget: missing -O" >&2; exit 2; }
name="${url##*/}"
src="${FAKE_RELEASES_DIR:?FAKE_RELEASES_DIR is required}/$name"
[ -f "$src" ] || { echo "fake wget: 404 $url" >&2; exit 22; }
cp "$src" "$dest"
FAKE
chmod +x "$FAKE_WGET"

# ---------------------------------------------------------------------------
# Fixtures: one archive per target with fake executables and real checksums.
# `tamper_target` rewrites a binary *after* SHA256SUMS is written, so the
# aggregate hash is correct but the in-package list no longer matches.
# ---------------------------------------------------------------------------
build_releases() {
    local dir="$1" tamper_target="${2:-}" target pkg binary
    mkdir -p "$dir"
    for target in "${TARGETS[@]}"; do
        pkg="$dir/apm-$VERSION-$target"
        mkdir -p "$pkg"
        for binary in apm apm-mcp apm-serve; do
            printf '#!/usr/bin/env bash\necho "%s %s-%s"\n' "$binary" "$VERSION" "$target" > "$pkg/$binary"
            chmod 0755 "$pkg/$binary"
        done
        printf 'README for %s\n' "$target" > "$pkg/README.md"
        printf 'MIT License\n' > "$pkg/LICENSE"
        ( cd "$pkg" && sha256sum apm apm-mcp apm-serve README.md LICENSE > SHA256SUMS )
        if [ -n "$tamper_target" ] && [ "$target" = "$tamper_target" ]; then
            printf '#!/usr/bin/env bash\necho "tampered"\n' > "$pkg/apm"
        fi
        tar -czf "$dir/apm-$VERSION-$target.tar.gz" -C "$dir" "apm-$VERSION-$target"
    done
    # Same shape as .github/workflows/release.yml: `sha256sum ./*.tar.gz`, which
    # prefixes every filename with "./". The installer must still find its line.
    ( cd "$dir" && sha256sum ./*.tar.gz > SHA256SUMS.txt )
}

REL_OK="$WORK/releases-ok"
build_releases "$REL_OK"

LINUX_ARCHIVE="apm-$VERSION-x86_64-unknown-linux-gnu.tar.gz"

# Aggregate list with a wrong hash for one archive (layer 1 must fail).
REL_BADSUM="$WORK/releases-badsum"
cp -a "$REL_OK" "$REL_BADSUM"
awk -v want="$LINUX_ARCHIVE" '
    {
        name = $2
        sub(/^\.\//, "", name)
        if (name == want) {
            print "00000000000000000000000000000000000000000000000000000000000000  " $2
            next
        }
        print
    }
' "$REL_BADSUM/SHA256SUMS.txt" > "$REL_BADSUM/SHA256SUMS.txt.new"
mv "$REL_BADSUM/SHA256SUMS.txt.new" "$REL_BADSUM/SHA256SUMS.txt"

# Correct aggregate, tampered package contents (layer 2 must fail).
REL_BADINNER="$WORK/releases-badinner"
build_releases "$REL_BADINNER" x86_64-unknown-linux-gnu

# Aggregate list without an entry for this target.
REL_NOMATCH="$WORK/releases-nomatch"
cp -a "$REL_OK" "$REL_NOMATCH"
awk -v want="$LINUX_ARCHIVE" '
    { name = $2; sub(/^\.\//, "", name); if (name != want) print }
' "$REL_NOMATCH/SHA256SUMS.txt" > "$REL_NOMATCH/SHA256SUMS.txt.new"
mv "$REL_NOMATCH/SHA256SUMS.txt.new" "$REL_NOMATCH/SHA256SUMS.txt"

# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------
CURL_LOG="$WORK/curl.log"
INSTALL_HOME="$WORK/home"

run_install() { # <releases-dir> <uname-s> <uname-m> [installer args...]
    local releases="$1" os="$2" arch="$3"
    shift 3
    env -u APM_VERSION -u APM_BIN_DIR -u BINDIR -u APM_REPO \
        HOME="$INSTALL_HOME" \
        APM_UNAME_S="$os" \
        APM_UNAME_M="$arch" \
        APM_CURL="$FAKE_CURL" \
        FAKE_RELEASES_DIR="$releases" \
        FAKE_LATEST_URL="https://github.com/Traveler0014/pi-apm/releases/tag/v$VERSION" \
        FAKE_CURL_LOG="$CURL_LOG" \
        bash "$ROOT/scripts/install.sh" "$@"
}

os_arch_for() {
    case "$1" in
        x86_64-unknown-linux-gnu) printf 'Linux x86_64' ;;
        aarch64-unknown-linux-gnu) printf 'Linux aarch64' ;;
        x86_64-apple-darwin) printf 'Darwin x86_64' ;;
        aarch64-apple-darwin) printf 'Darwin arm64' ;;
        *) fail "no uname mapping for $1" ;;
    esac
}

mkdir -p "$INSTALL_HOME"

# 1. The four supported platforms install the right target triple.
for target in "${TARGETS[@]}"; do
    read -r os arch <<< "$(os_arch_for "$target")"
    dir="$WORK/bin-$target"
    : > "$CURL_LOG"
    if ! run_install "$REL_OK" "$os" "$arch" --version "v$VERSION" --bin-dir "$dir" > "$WORK/out-$target" 2>&1; then
        fail "$target: installer failed: $(cat "$WORK/out-$target")"
    fi
    for binary in apm apm-mcp apm-serve; do
        [ -x "$dir/$binary" ] || fail "$target: $binary missing or not executable"
        [ "$(mode_of "$dir/$binary")" = "755" ] || fail "$target: $binary has mode $(mode_of "$dir/$binary")"
        grep -q "$target" "$dir/$binary" || fail "$target: $binary is not the fixture for this platform"
    done
    pass "platform $target"
done

# 2. Unsupported platforms fail with a clear message (and mention Windows).
for pair in "Linux:i686" "Linux:armv7l" "MINGW64_NT:x86_64"; do
    os="${pair%%:*}"
    arch="${pair##*:}"
    : > "$CURL_LOG"
    if run_install "$REL_OK" "$os" "$arch" --bin-dir "$WORK/bin-unsupported" > "$WORK/out-unsupported" 2>&1; then
        fail "unsupported platform $os/$arch must fail"
    fi
    grep -q "unsupported platform" "$WORK/out-unsupported" || fail "$os/$arch: missing 'unsupported platform'"
    grep -qi "windows" "$WORK/out-unsupported" || fail "$os/$arch: message should mention the Windows zip"
    [ ! -s "$CURL_LOG" ] || fail "$os/$arch: must fail before any network request (got: $(cat "$CURL_LOG"))"
    pass "unsupported platform $os/$arch"
done

# 3. Latest-tag resolution and explicit versions with and without `v`.
for spec in latest v0.2.0 0.2.0; do
    dir="$WORK/bin-version-$spec"
    : > "$CURL_LOG"
    args=()
    if [ "$spec" != latest ]; then args=(--version "$spec"); fi
    if ! run_install "$REL_OK" Linux x86_64 "${args[@]}" --bin-dir "$dir" > "$WORK/out-$spec" 2>&1; then
        fail "version form $spec failed: $(cat "$WORK/out-$spec")"
    fi
    grep -q "install: release    v$VERSION" "$WORK/out-$spec" || fail "$spec: tag was not normalised to v$VERSION"
    grep -q "$LINUX_ARCHIVE" "$CURL_LOG" || fail "$spec: installer did not request $LINUX_ARCHIVE"
    [ -x "$dir/apm" ] || fail "$spec: apm was not installed"
    pass "version form $spec"
done

# 3b. wget fallback: the 302 headers are CRLF and wget exits non-zero; the
#     installer must still parse the tag and download through wget.
dir="$WORK/bin-wget"
: > "$CURL_LOG"
if ! env -u APM_VERSION -u APM_BIN_DIR -u BINDIR -u APM_REPO \
    HOME="$INSTALL_HOME" \
    APM_UNAME_S=Linux APM_UNAME_M=x86_64 \
    APM_NO_CURL=1 APM_WGET="$FAKE_WGET" \
    FAKE_RELEASES_DIR="$REL_OK" \
    FAKE_LATEST_URL="https://github.com/Traveler0014/pi-apm/releases/tag/v$VERSION" \
    bash "$ROOT/scripts/install.sh" --bin-dir "$dir" > "$WORK/out-wget" 2>&1; then
    fail "wget fallback failed: $(cat "$WORK/out-wget")"
fi
grep -q "install: release    v$VERSION" "$WORK/out-wget" || fail "wget fallback: CRLF Location was not parsed"
[ -x "$dir/apm" ] || fail "wget fallback: apm was not installed"
pass "wget fallback (CRLF Location + exit 8)"

# 3c. An aggregate generated in binary mode (`sha256sum -b`) emits `*./name`:
#     the installer strips CR, then `*`, then `./` in that order.
REL_BINARY="$WORK/releases-binary"
cp -a "$REL_OK" "$REL_BINARY"
( cd "$REL_BINARY" && sha256sum -b ./*.tar.gz > SHA256SUMS.txt )
if grep -q ' \*\./' "$REL_BINARY/SHA256SUMS.txt"; then
    :
else
    fail "fixture does not exercise the '*./' aggregate form"
fi
dir="$WORK/bin-binary-mode"
if ! run_install "$REL_BINARY" Linux x86_64 --version "v$VERSION" --bin-dir "$dir" > "$WORK/out-binary" 2>&1; then
    fail "binary-mode aggregate failed: $(cat "$WORK/out-binary")"
fi
[ -x "$dir/apm" ] || fail "binary-mode aggregate: apm was not installed"
pass "binary-mode aggregate (*./ prefix)"

# 4. Normal install: three binaries, version output, PATH hint, no temp debris.
dir="$WORK/bin-normal"
: > "$CURL_LOG"
if ! run_install "$REL_OK" Linux x86_64 --version "v$VERSION" --bin-dir "$dir" > "$WORK/out-normal" 2>&1; then
    fail "normal install failed: $(cat "$WORK/out-normal")"
fi
for binary in apm apm-mcp apm-serve; do
    grep -q "^$binary $VERSION-x86_64-unknown-linux-gnu$" "$WORK/out-normal" ||
        fail "normal install: missing $binary --version output"
done
grep -q "is not on your PATH" "$WORK/out-normal" || fail "normal install: missing PATH hint"
grep -q "export PATH=" "$WORK/out-normal" || fail "normal install: PATH hint lacks an export line"
if find "$dir" -name '.*.tmp.*' -print -quit | grep -q .; then
    fail "normal install: temp files left behind"
fi
pass "normal install (three binaries, 0755, version output, PATH hint)"

# 4b. Default destination is $HOME/.local/bin.
default_home="$WORK/home-default"
mkdir -p "$default_home"
if ! INSTALL_HOME="$default_home" run_install "$REL_OK" Linux x86_64 --version "v$VERSION" > "$WORK/out-default" 2>&1; then
    fail "default bin-dir install failed: $(cat "$WORK/out-default")"
fi
[ -x "$default_home/.local/bin/apm" ] || fail "default bin-dir: $HOME/.local/bin/apm missing"
pass "default destination \$HOME/.local/bin"

# 5. Upgrade replaces an existing older binary in place.
dir="$WORK/bin-upgrade"
mkdir -p "$dir"
printf '#!/usr/bin/env bash\necho "apm 0.1.0-old"\n' > "$dir/apm"
chmod 0755 "$dir/apm"
if ! run_install "$REL_OK" Linux x86_64 --version "v$VERSION" --bin-dir "$dir" > "$WORK/out-upgrade" 2>&1; then
    fail "upgrade failed: $(cat "$WORK/out-upgrade")"
fi
grep -q "apm $VERSION-x86_64-unknown-linux-gnu" "$dir/apm" || fail "upgrade: old apm was not replaced"
if grep -q "0.1.0-old" "$dir/apm"; then fail "upgrade: old binary still in place"; fi
[ -x "$dir/apm-mcp" ] && [ -x "$dir/apm-serve" ] || fail "upgrade: missing new binaries"
pass "upgrade replaces an older installed binary"

# 6. A mismatched aggregate checksum aborts the install and leaves the
#    destination untouched.
dir="$WORK/bin-badsum"
mkdir -p "$dir"
printf '#!/usr/bin/env bash\necho "apm 0.1.0-keep"\n' > "$dir/apm"
chmod 0755 "$dir/apm"
: > "$CURL_LOG"
if run_install "$REL_BADSUM" Linux x86_64 --version "v$VERSION" --bin-dir "$dir" > "$WORK/out-badsum" 2>&1; then
    fail "tampered aggregate checksum must fail the install"
fi
grep -q "SHA-256 mismatch" "$WORK/out-badsum" || fail "badsum: missing SHA-256 mismatch message"
grep -q "0.1.0-keep" "$dir/apm" || fail "badsum: existing binary was modified"
[ "$(find "$dir" -mindepth 1 | wc -l)" -eq 1 ] || fail "badsum: destination was polluted"
pass "aggregate checksum mismatch rejected; destination untouched"

# 7. A tampered in-package SHA256SUMS aborts after layer 1 and leaves the
#    destination empty.
dir="$WORK/bin-badinner"
mkdir -p "$dir"
if run_install "$REL_BADINNER" Linux x86_64 --version "v$VERSION" --bin-dir "$dir" > "$WORK/out-badinner" 2>&1; then
    fail "tampered package checksums must fail the install"
fi
grep -q "own SHA256SUMS did not verify" "$WORK/out-badinner" || fail "badinner: missing in-package failure message"
[ -z "$(find "$dir" -mindepth 1 -print -quit)" ] || fail "badinner: destination was polluted"
pass "tampered in-package checksums rejected; destination untouched"

# 8. A missing aggregate entry is refused (never "verify what we found").
dir="$WORK/bin-nomatch"
mkdir -p "$dir"
if run_install "$REL_NOMATCH" Linux x86_64 --version "v$VERSION" --bin-dir "$dir" > "$WORK/out-nomatch" 2>&1; then
    fail "missing checksum entry must fail the install"
fi
grep -q "has no entry for $LINUX_ARCHIVE" "$WORK/out-nomatch" || fail "nomatch: missing 'no entry' message"
[ -z "$(find "$dir" -mindepth 1 -print -quit)" ] || fail "nomatch: destination was polluted"
pass "missing aggregate entry rejected"

# 9. Input validation: path/URL injection is refused before any download.
: > "$CURL_LOG"
if run_install "$REL_OK" Linux x86_64 --version "../../evil" --bin-dir "$WORK/bin-inject" > "$WORK/out-badver" 2>&1; then
    fail "invalid --version must fail"
fi
grep -q "invalid version" "$WORK/out-badver" || fail "bad version: missing error"
[ ! -s "$CURL_LOG" ] || fail "bad version: downloaded before validating"

if run_install "$REL_OK" Linux x86_64 --repo "../../etc/passwd" --version "v$VERSION" --bin-dir "$WORK/bin-inject" > "$WORK/out-badrepo" 2>&1; then
    fail "invalid --repo must fail"
fi
grep -q "invalid --repo" "$WORK/out-badrepo" || fail "bad repo: missing error"
[ ! -s "$CURL_LOG" ] || fail "bad repo: downloaded before validating"
pass "invalid version/repo rejected before download"

# 10. --help is a zero-exit, no-network path.
if ! run_install "$REL_OK" Linux x86_64 --help > "$WORK/out-help" 2>&1; then
    fail "--help must exit 0"
fi
grep -q "usage: install.sh" "$WORK/out-help" || fail "--help: missing usage text"
pass "--help"

echo "test-install-script: PASS"
