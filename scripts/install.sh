#!/usr/bin/env bash
# One-line installer for the AgentPM binaries (Linux and macOS).
#
#   curl -fsSL https://raw.githubusercontent.com/Traveler0014/pi-apm/main/scripts/install.sh | bash
#   curl -fsSL .../install.sh | bash -s -- --version v0.3.0 --bin-dir "$HOME/bin"
#
# Overrides: --version/-v, --bin-dir/-d, --repo/-r, or the environment variables
# APM_VERSION, APM_BIN_DIR (BINDIR also accepted), APM_REPO.
#
# Design notes that matter for review:
#
#   * **Everything runs from main()**, called on the last line. With
#     `curl ... | bash` the shell executes what it has received so far; if the
#     transfer dies mid-script, nothing has run because the function body is
#     incomplete.
#   * **No gh, no jq, no Python.** Only bash, curl (wget fallback), tar and
#     sha256sum (shasum -a 256 on macOS).
#   * **Latest is resolved from the /releases/latest redirect**, not the JSON
#     API: anonymous API calls are rate-limited to 60/hour, a redirect is not.
#   * **Two independent checksum layers**: the aggregate SHA256SUMS.txt must
#     name the archive with its exact hash, then the package's own SHA256SUMS
#     must verify the three binaries plus the two documents.
#   * **Atomic install**: each binary is copied to a temp file in the
#     destination, chmod 0755, then renamed over the target. Replacing a running
#     binary with `cp` can fail with ETXTBSY; rename(2) does not.
#   * The destination is only touched after both checksum layers pass, so a
#     failed or tampered download leaves an existing installation untouched.
#
# Test seams (used by scripts/test-install-script.sh, harmless in production):
# APM_UNAME_S / APM_UNAME_M override platform detection, APM_CURL overrides the
# downloader, APM_RELEASES_URL overrides the release-download base URL.

set -euo pipefail

main() {
    die() {
        printf 'install: %s\n' "$*" >&2
        exit 1
    }

    note() {
        printf '%s\n' "$*"
    }

    usage() {
        cat <<'USAGE'
usage: install.sh [options]

Installs the AgentPM binaries (apm, apm-mcp, apm-serve) from a GitHub Release.

Options:
  -v, --version <tag>    Release tag, with or without the leading "v"
                         (default: the latest release, resolved via GitHub's
                         /releases/latest redirect)
  -d, --bin-dir <dir>    Destination directory (default: $APM_BIN_DIR, else
                         $BINDIR, else $HOME/.local/bin)
  -r, --repo <owner/name> GitHub repository
                         (default: $APM_REPO, else Traveler0014/pi-apm)
  -h, --help             Show this help

Environment: APM_VERSION, APM_BIN_DIR, BINDIR, APM_REPO.
Supported hosts: Linux x86_64/arm64 and macOS x86_64/arm64.
USAGE
    }

    need_value() {
        [ "$#" -ge 2 ] || die "$1 needs a value"
    }

    # --- arguments ---------------------------------------------------------
    local repo="${APM_REPO:-Traveler0014/pi-apm}"
    local version_input="${APM_VERSION:-}"
    local bin_dir="${APM_BIN_DIR:-${BINDIR:-$HOME/.local/bin}}"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            -v | --version)
                need_value "$@"
                version_input="$2"
                shift 2
                ;;
            -d | --bin-dir)
                need_value "$@"
                bin_dir="$2"
                shift 2
                ;;
            -r | --repo)
                need_value "$@"
                repo="$2"
                shift 2
                ;;
            -h | --help)
                usage
                exit 0
                ;;
            --)
                shift
                break
                ;;
            -*)
                die "unknown option: $1 (try --help)"
                ;;
            *)
                die "unexpected argument: $1 (try --help)"
                ;;
        esac
    done
    [ "$#" -eq 0 ] || die "unexpected argument: $1 (try --help)"

    # --- input validation (no path/URL injection) --------------------------
    # A repository is exactly "owner/name" over a conservative charset.
    if ! [[ "$repo" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]]; then
        die "invalid --repo '$repo' (expected owner/name, e.g. Traveler0014/pi-apm)"
    fi

    normalize_version() {
        local raw="$1" version
        version="${raw#v}"
        if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+][A-Za-z0-9._-]+)?$ ]]; then
            die "invalid version '$raw' (expected vX.Y.Z or X.Y.Z)"
        fi
        printf '%s' "$version"
    }

    # --- platform detection -------------------------------------------------
    local uname_s="${APM_UNAME_S:-$(uname -s)}"
    local uname_m="${APM_UNAME_M:-$(uname -m)}"

    detect_target() {
        local os="$1" arch="$2"
        case "$os:$arch" in
            Linux:x86_64 | Linux:amd64) printf 'x86_64-unknown-linux-gnu' ;;
            Linux:aarch64 | Linux:arm64) printf 'aarch64-unknown-linux-gnu' ;;
            Darwin:x86_64 | Darwin:amd64) printf 'x86_64-apple-darwin' ;;
            Darwin:arm64 | Darwin:aarch64) printf 'aarch64-apple-darwin' ;;
            *)
                die "unsupported platform: $os/$arch. Supported: Linux x86_64, Linux arm64, macOS x86_64, macOS arm64. On Windows download the .zip archive from https://github.com/$repo/releases and extract apm.exe/apm-mcp.exe/apm-serve.exe."
                ;;
        esac
    }

    # --- tool detection -----------------------------------------------------
    local curl_bin="${APM_CURL:-}"
    if [ -z "$curl_bin" ] && [ "${APM_NO_CURL:-0}" != "1" ] && command -v curl >/dev/null 2>&1; then
        curl_bin="curl"
    fi
    local wget_bin="${APM_WGET:-}"
    if [ -z "$wget_bin" ] && command -v wget >/dev/null 2>&1; then
        wget_bin="wget"
    fi

    download() {
        local url="$1" dest="$2"
        if [ -n "$curl_bin" ]; then
            "$curl_bin" -fsSL --retry 2 -o "$dest" "$url"
        elif [ -n "$wget_bin" ]; then
            "$wget_bin" -q -O "$dest" "$url"
        else
            die "neither curl nor wget is available; install one or download the archive manually (see docs/release.md)"
        fi
    }

    resolve_latest() {
        local latest_url="https://github.com/$repo/releases/latest" effective=""
        if [ -n "$curl_bin" ]; then
            effective="$("$curl_bin" -fsSL -o /dev/null -w '%{url_effective}' "$latest_url")" ||
                die "cannot resolve the latest release of $repo (network error?)"
        elif [ -n "$wget_bin" ]; then
            # GitHub answers with a 302 to /releases/tag/vX.Y.Z. wget exits
            # non-zero for an unfollowed redirect (exit 8), so capture its
            # headers with `|| true`; they are CRLF-terminated, hence the
            # `\r` strip before parsing the URL.
            local headers=""
            headers="$("$wget_bin" -q -S --spider --max-redirect=0 "$latest_url" 2>&1 || true)"
            effective="$(printf '%s\n' "$headers" |
                awk 'tolower($0) ~ /^[[:space:]]*location:/ { loc = $2; sub(/\r$/, "", loc); print loc }' |
                tail -n 1)"
            [ -n "$effective" ] ||
                die "cannot resolve the latest release of $repo (no redirect); pass --version explicitly"
        else
            die "neither curl nor wget is available; pass --version explicitly or install one of them"
        fi
        local tag="${effective##*/}"
        [ -n "$tag" ] || die "could not parse the latest release tag from '$effective'; pass --version explicitly"
        printf '%s' "$tag"
    }

    # Detect the platform first: an unsupported host must fail before we spend
    # a network request resolving the latest release.
    local target
    target="$(detect_target "$uname_s" "$uname_m")"

    # --- versions and URLs --------------------------------------------------
    local tag version
    if [ -n "$version_input" ]; then
        version="$(normalize_version "$version_input")"
        tag="v$version"
    else
        tag="$(resolve_latest)"
        version="$(normalize_version "$tag")"
        tag="v$version"
    fi

    local archive="apm-$version-$target.tar.gz"

    local releases_url="${APM_RELEASES_URL:-https://github.com/$repo/releases/download/$tag}"
    releases_url="${releases_url%/}"

    # --- checksum helpers ---------------------------------------------------
    sha256_of() {
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "$1" | awk '{ print $1 }'
        elif command -v shasum >/dev/null 2>&1; then
            shasum -a 256 "$1" | awk '{ print $1 }'
        else
            die "need sha256sum or shasum to verify the download"
        fi
    }

    verify_package_checksums() {
        local dir="$1"
        if command -v sha256sum >/dev/null 2>&1; then
            ( cd "$dir" && sha256sum -c SHA256SUMS )
        elif command -v shasum >/dev/null 2>&1; then
            ( cd "$dir" && shasum -a 256 -c SHA256SUMS )
        else
            die "need sha256sum or shasum to verify the package"
        fi
    }

    # --- fetch ---------------------------------------------------------------
    # Deliberately global: EXIT traps run after main() returns, when its locals
    # are gone; `${WORK_DIR:-}` keeps the cleanup safe under `set -u`.
    WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/apm-install.XXXXXX")" ||
        die "cannot create a temporary directory"
    trap 'rm -rf "${WORK_DIR:-}"' EXIT

    note "install: repository $repo"
    note "install: release    $tag ($target)"
    note "install: download   $releases_url"

    download "$releases_url/$archive" "$WORK_DIR/$archive"
    download "$releases_url/SHA256SUMS.txt" "$WORK_DIR/SHA256SUMS.txt"

    # --- layer 1: the archive's hash from the aggregate list -----------------
    # Exact field match (never a regex), so a filename containing regex
    # metacharacters cannot match the wrong line and `foo.tar.gz.bak` cannot
    # match `foo.tar.gz`.
    local expected
    # The release workflow generates the aggregate with
    # `sha256sum ./*.tar.gz ./*.zip`, so the filename field is `./apm-…`; accept
    # that, a bare name, and sha256sum's binary-mode `*name` marker.
    expected="$(awk -v want="$archive" '
        {
            name = $2
            sub(/\r$/, "", name)
            sub(/^\*/, "", name)
            sub(/^\.\//, "", name)
            if (name == want) { print $1; exit }
        }
    ' "$WORK_DIR/SHA256SUMS.txt")"
    [ -n "$expected" ] || die "SHA256SUMS.txt has no entry for $archive"
    local actual
    actual="$(sha256_of "$WORK_DIR/$archive")"
    if [ "$actual" != "$expected" ]; then
        die "SHA-256 mismatch for $archive (expected $expected, got $actual); nothing was installed"
    fi
    note "install: checksum   archive OK"

    # --- unpack and layer 2: the package's own SHA256SUMS --------------------
    tar -xzf "$WORK_DIR/$archive" -C "$WORK_DIR" ||
        die "cannot extract $archive"
    local package="$WORK_DIR/apm-$version-$target"
    [ -d "$package" ] || die "archive did not contain apm-$version-$target/"
    [ -f "$package/SHA256SUMS" ] || die "package is missing SHA256SUMS"
    local binary
    for binary in apm apm-mcp apm-serve; do
        [ -f "$package/$binary" ] || die "package is missing $binary"
    done
    verify_package_checksums "$package" >/dev/null ||
        die "the package's own SHA256SUMS did not verify; nothing was installed"
    note "install: checksum   binaries OK"

    # --- atomic install ------------------------------------------------------
    mkdir -p "$bin_dir" || die "cannot create $bin_dir"
    for binary in apm apm-mcp apm-serve; do
        local staged="$bin_dir/.$binary.tmp.$$"
        cp "$package/$binary" "$staged" || {
            rm -f "$staged"
            die "cannot stage $binary in $bin_dir"
        }
        chmod 0755 "$staged"
        mv -f "$staged" "$bin_dir/$binary" || {
            rm -f "$staged"
            die "cannot install $binary into $bin_dir"
        }
    done

    note "install: installed  $bin_dir/{apm,apm-mcp,apm-serve} ($tag)"

    case ":$PATH:" in
        *":$bin_dir:"*) ;;
        *)
            note ""
            note "NOTE: $bin_dir is not on your PATH. Add it with:"
            note "  export PATH=\"$bin_dir:\$PATH\""
            note "and put that line in your shell profile to keep it."
            ;;
    esac

    note ""
    for binary in apm apm-mcp apm-serve; do
        if "$bin_dir/$binary" --version 2>/dev/null; then
            :
        else
            note "  ($binary --version could not run on this host)"
        fi
    done
    note ""
    note "Next: cd into a repository and run: apm init --name \"My Repo\""
}

main "$@"
