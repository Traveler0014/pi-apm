# AgentPM (apm) — local-first agent work ledger.
#
#   just build          cargo build --release
#   just install        install the release binary into $BINDIR (default ~/.local/bin)
#   just verify         fmt + clippy + Rust tests + extension typecheck/tests
#   just stress         multi-process claim/write contention check

CARGO := env_var_or_default("CARGO", "cargo")
BINDIR := env_var_or_default("BINDIR", env_var("HOME") / ".local/bin")
BINARY := "target/release/apm"

# List the available recipes.
default:
    @just --list

# Compile the release binary (target/release/apm).
build:
    {{ CARGO }} build --release

# Install the release binary as $BINDIR/apm (default: ~/.local/bin).
install: build
    mkdir -p "{{ BINDIR }}"
    install -m 0755 "{{ BINARY }}" "{{ BINDIR }}/apm"
    @echo "installed {{ BINDIR }}/apm"

# Full verification suite: formatting, lints, Rust tests, extension typecheck/tests.
verify:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all-targets
    npx tsc -p tsconfig.json
    npx vitest run

# Cross-process contention check (10 concurrent claims and writes).
stress:
    bash scripts/stress.sh 10
