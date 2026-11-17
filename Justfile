# AgentPM (apm) — local-first agent work ledger.
#
#   just build          cargo build --release (embeds ui/dist, no Node needed)
#   just install        install the release binaries (apm, apm-mcp, apm-serve) into $BINDIR
#   just ui-build       rebuild ui/dist with Vite (needs Node; committed after each build)
#   just verify         fmt + clippy + Rust tests + extension/UI typecheck + tests
#   just ui-check       frontend typecheck + vitest for the ui/ workspace
#   just lint-workflows   actionlint the GitHub Actions YAML (needs actionlint)
#   just release-package  build a release archive for this machine (dist/)
#   just test-release-scripts  offline tests for the publish/create-or-update logic
#   just release-check ARCHIVE  verify structure, checksums and CLI smoke
#   just stress         multi-process claim/write contention check (CLI + REST)

CARGO := env_var_or_default("CARGO", "cargo")
BINDIR := env_var_or_default("BINDIR", env_var("HOME") / ".local/bin")
BINARY := "target/release/apm"
# Optional adapters, same crate, installed alongside the CLI so the documented
# configurations (`apm-mcp` for MCP clients, `apm-serve` for the REST adapter)
# work without an extra step.
MCP_BINARY := "target/release/apm-mcp"
SERVE_BINARY := "target/release/apm-serve"

# List the available recipes.
default:
    @just --list

# Compile the release binaries (apm, apm-mcp, apm-serve).
#
# `ui/dist` is committed and embedded at compile time (see build.rs), so this
# never needs Node. Run `just ui-build` after changing the frontend.
build:
    {{ CARGO }} build --release

# Rebuild the dashboard bundle into ui/dist (Vite root: ui/). Requires Node and
# an `npm install` in the repository root. Commit ui/dist with the source.
ui-build:
    npm run ui:build

# Frontend checks: TS typecheck plus the ui/**/*.test.ts suite.
ui-check:
    npm run ui:typecheck
    npm run ui:test

# Real-browser smoke of `apm ui` (needs Chromium and a release build).
# Set APM_CHROME=/path/to/chrome when it is not in Playwright's cache or PATH.
ui-browser: build
    node scripts/ui-browser-check.mjs

# Install all three release binaries into $BINDIR (default: ~/.local/bin).
install: build
    mkdir -p "{{ BINDIR }}"
    install -m 0755 "{{ BINARY }}" "{{ BINDIR }}/apm"
    install -m 0755 "{{ MCP_BINARY }}" "{{ BINDIR }}/apm-mcp"
    install -m 0755 "{{ SERVE_BINARY }}" "{{ BINDIR }}/apm-serve"
    @echo "installed {{ BINDIR }}/apm {{ BINDIR }}/apm-mcp {{ BINDIR }}/apm-serve"

# Full verification suite: formatting, lints, Rust tests, extension typecheck/tests.
verify:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all-targets
    npx tsc -p tsconfig.json
    npm run ui:typecheck
    npx vitest run
    bash scripts/test-release-scripts.sh

# Static validation of the GitHub Actions YAML. Install actionlint with
# `go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.7`.
lint-workflows:
    actionlint .github/workflows/*.yml

# Offline tests for scripts/publish-release.sh (fake gh; create vs --clobber).
test-release-scripts:
    bash scripts/test-release-scripts.sh

# Build a distributable archive for the host target into dist/ (no tag, no
# publish). CI builds the full platform matrix; see docs/release.md.
release-package:
    bash scripts/package-release.sh --outdir dist

# Verify a release archive or unpacked directory: layout, SHA256SUMS, and — when
# it targets this machine — --version plus a real CLI round-trip.
release-check ARCHIVE:
    bash scripts/verify-package.sh "{{ ARCHIVE }}"

# Cross-process contention checks: CLI-only, then the REST adapter variant
# (several servers, concurrent HTTP claims, mixed HTTP/CLI writers).
stress:
    bash scripts/stress.sh 10
    bash scripts/stress-rest.sh 10
