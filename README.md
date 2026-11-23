# AgentPM (`apm`)

A **local-first, agent-native work ledger**. Not a Jira: no server, no daemon, no
account. One SQLite file is the authority; the CLI and its JSON output are the
interface.

Agents forget what they did, claim the same task twice, or call something "done"
without evidence. AgentPM makes the work visible:

1. **Who is doing what?** — atomic claims with leases, not vibes.
2. **What actually happened?** — progress recorded separately from liveness.
3. **What is next?** — an explicit `next_action` per item.
4. **What is blocked, and on what?** — blockers and dependency edges.
5. **Who is waiting on a human?** — structured attention requests, not chat scrollback.
6. **Who vanished?** — expired leases and "no substantive progress" views.

```
┌────────────────────────────┐        ┌──────────────────────────────┐
│ pi (main agent session)    │        │ apm CLI                      │
│  claims item, edits code   │──bash─▶│ claim / progress / status    │
│  agent_settled hook        │        │ context / attention / events │
└─────────────┬──────────────┘        └──────────────┬───────────────┘
              │ side-run (child pi, isolated)        │
              ▼                                      ▼
┌────────────────────────────┐        ┌──────────────────────────────┐
│ side-run agent             │───────▶│ .apm/apm.db  (SQLite, WAL)   │
│ reads run digest + claims  │        │ claims, context versions,    │
│ writes only what happened  │        │ events, attention, evidence  │
└────────────────────────────┘        └──────────────────────────────┘
```

The Pi extension runs a **side-run after every agent run** so the ledger stays
honest even when the main agent forgets to update it, then reconciles the run
deterministically. Use the CLI alone if you prefer — everything works without the
extension, and the MCP/REST adapters are optional.

## Install

The distribution point is the GitHub repository:
**<https://github.com/Traveler0014/pi-apm>**. Released binaries are attached to
[GitHub Releases](https://github.com/Traveler0014/pi-apm/releases); source builds
are covered in [`docs/development.md`](docs/development.md).

### Binaries from a GitHub Release

Three binaries come out of the same crate: `apm` (the CLI), `apm-mcp` (the MCP
stdio server) and `apm-serve` (the optional loopback REST adapter). Each release
attaches one archive per platform (Linux x64/arm64, macOS x64/arm64, Windows x64)
with all three binaries, `README.md`, `LICENSE`, a per-package `SHA256SUMS`, plus
an aggregate `SHA256SUMS.txt`.

```bash
# 1. Download the archive for your platform plus the aggregate checksum list.
#    Replace v0.2.0 with the latest release tag if a newer one exists.
gh release download v0.2.0 --repo Traveler0014/pi-apm \
  --pattern 'apm-0.2.0-x86_64-unknown-linux-gnu.tar.gz' \
  --pattern 'SHA256SUMS.txt'

# 2. Verify that archive against the aggregate list (the list covers every
#    platform, so check one line instead of `sha256sum -c` on the whole file).
grep ' apm-0.2.0-x86_64-unknown-linux-gnu.tar.gz$' SHA256SUMS.txt | sha256sum -c -
# macOS has no sha256sum: use `shasum -a 256 -c -` instead.

# 3. Unpack, check the per-package list, install all three binaries.
tar -xzf apm-0.2.0-x86_64-unknown-linux-gnu.tar.gz
(cd apm-0.2.0-x86_64-unknown-linux-gnu && sha256sum -c SHA256SUMS)
install -m 0755 apm-0.2.0-x86_64-unknown-linux-gnu/{apm,apm-mcp,apm-serve} ~/.local/bin/

apm --version && apm-mcp --version && apm-serve --version
```

Windows: `Expand-Archive apm-0.2.0-x86_64-pc-windows-msvc.zip`, then copy the
three `.exe` files somewhere on `PATH`. If `~/.local/bin` is not on your `PATH`:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

**Upgrade** = install the newer release over the same paths (the ledger is
unchanged). **Rollback** = reinstall the previous tag's archive; never edit the
database to roll back. The full policy, checksum fallbacks and troubleshooting
live in [`docs/release.md`](docs/release.md#download-install-upgrade-rollback).

### Pi extension

```bash
pi install git:github.com/Traveler0014/pi-apm   # into ~/.pi/agent/settings.json
pi list                                         # should show pi-apm

pi install -l ./            # project scope (.pi/settings.json, needs trust)
pi -e ./pi-apm/index.ts     # try it for one invocation
```

The extension needs `apm` on `PATH`, or an explicit `--apm-bin /path/to/apm`.
How the tools, side-run, flags and token handling work:
[`docs/pi-plugin.md`](docs/pi-plugin.md).

### From source

```bash
cargo build --release    # SQLite is bundled; no system dependency, no network at runtime
just install             # → $HOME/.local/bin/{apm,apm-mcp,apm-serve}
```

Requires Rust 1.85+ (edition 2024). The committed `ui/dist` is embedded, so a
binary build needs no Node. Development setup, the full test matrix and the
repository layout: [`docs/development.md`](docs/development.md).

## Quick start

```bash
cd ~/code/my-repo
apm init --name "My Repo"        # creates .apm/apm.db, derives prefix MYR (or --prefix)
apm dashboard                    # what is going on?
apm ui                           # optional read-only dashboard, loopback only

# The agent loop (see docs/cli-reference.md for the full sequence):
apm claim APM-1 --json           # token returned once, saved to the 0600 session file
apm status APM-1 --to doing --expect-version 1
apm progress APM-1 --expect-version 2 --summary "what concretely changed"
apm status APM-1 --to done --note "verified by …"   # guards require evidence/acceptance
```

`apm init` is idempotent. Keep the runtime directory **out of git**:

```gitignore
.apm/
```

Workflow documents, fixtures and `AGENTS.md` belong in git; the live database and
the 0600 token files do not. Backup and redacted export:

```bash
apm backup --out apm-backup.db    # WAL-consistent snapshot, refuses overwrite
apm export --out audit.ndjson     # redacted, refuses overwrite (0600)
```

Why the ledger is safe to keep locally (WAL, leases, versions, idempotency), what
`.apm/` contains, and how backup/export behave:
[`docs/concepts.md`](docs/concepts.md).

## Documentation

| Guide | What it covers |
|---|---|
| [`docs/concepts.md`](docs/concepts.md) | status vs health, the three clocks, claim rules (incl. `--recover-only` and expired leases), versioned context, attention, data model, `.apm/` + git, backup/export |
| [`docs/workflows.md`](docs/workflows.md) | the default state machine (data, not code), custom workflow JSON, guards, per-type bindings, upgrade/migrate |
| [`docs/cli-reference.md`](docs/cli-reference.md) | the working loop, version/idempotency rules, dashboard/doctor, every command, error codes + exit codes |
| [`docs/agent-protocol.md`](docs/agent-protocol.md) | copy-paste `AGENTS.md` contract for a repository that uses `apm` |
| [`docs/pi-plugin.md`](docs/pi-plugin.md) | Pi extension: tools, run-end side-run and deterministic reconciliation, flags/env, `--apm-bin` PATH shim, token redaction, side-run report triage |
| [`docs/mcp.md`](docs/mcp.md) | `apm-mcp`: client config, eight tools, protocol surface, two-layer errors, no tokens on the wire |
| [`docs/rest.md`](docs/rest.md) | `apm-serve`: routes, loopback/Host/Origin protection, error mapping, why no framework |
| [`docs/rest-ops.md`](docs/rest-ops.md) | running REST for real: limits, multi-instance, backup/restore, token rotation, troubleshooting |
| [`docs/ui.md`](docs/ui.md) | `apm ui`: ticket → HttpOnly cookie, CSP, embedded assets, views/graph, header metrics, troubleshooting |
| [`docs/release.md`](docs/release.md) | CI matrix, packaging, download/verify/install/upgrade/rollback, supply-chain policy |
| [`docs/development.md`](docs/development.md) | build from source, test matrix (Rust/Vitest/stress/browser), real Pi smoke, repository layout, design boundaries |

Local checks: `just verify` (Rust + TypeScript + tests + release-script tests),
`just ui-dist-check` (committed dashboard bundle matches a fresh build),
`just stress` (multi-process claim/write races).

## License

MIT — see [`LICENSE`](LICENSE).
