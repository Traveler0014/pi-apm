# AgentPM (apm) — local-first agent work ledger.
#
#   just build          cargo build --release
#   just install        install the release binaries (apm, apm-mcp, apm-serve) into $BINDIR
#   just verify         fmt + clippy + Rust tests + extension typecheck/tests
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
build:
    {{ CARGO }} build --release

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
    npx vitest run

# Cross-process contention checks: CLI-only, then the REST adapter variant
# (several servers, concurrent HTTP claims, mixed HTTP/CLI writers).
stress:
    bash scripts/stress.sh 10
    bash scripts/stress-rest.sh 10
