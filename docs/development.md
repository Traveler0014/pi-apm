# Development

Build from source, run the test matrix, and find your way around the repository.

## Requirements

- Rust **1.85+** (edition 2024; the repository pins `1.100.0` in
  `rust-toolchain.toml` with `rustfmt`/`clippy` — CI uses the same).
- Node.js 22 + npm for the TypeScript extension, the dashboard and the CI-like
  verification script.
- Optional: [`just`](https://github.com/casey/just) for the recipes below, and a
  Chromium/Playwright cache for the real-browser smoke test.

SQLite is compiled from the bundled `rusqlite` amalgamation; there is no system
dependency and no network access at runtime.

## Build and install

```bash
just build                       # cargo build --release → target/release/{apm,apm-mcp,apm-serve}
just install                     # → $HOME/.local/bin/{apm,apm-mcp,apm-serve}
BINDIR=/usr/local/bin just install   # or any directory on PATH
apm --version && apm-mcp --version && apm-serve --version
```

`just install` builds first and installs all three binaries with mode `0755`;
`BINDIR` defaults to `$HOME/.local/bin`. If that directory is not on your `PATH`,
add it:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

The manual equivalent, without `just`:

```bash
cargo build --release
mkdir -p "$HOME/.local/bin"
install -m 0755 target/release/apm "$HOME/.local/bin/apm"
install -m 0755 target/release/apm-mcp "$HOME/.local/bin/apm-mcp"
install -m 0755 target/release/apm-serve "$HOME/.local/bin/apm-serve"
```

The binaries embed `ui/dist`, which is committed — no Node is needed to build or
run them. After changing the dashboard, rebuild and commit the bundle:

```bash
npm install            # once
just ui-build          # = npm run ui:build
just ui-dist-check     # rebuild and fail if the committed bundle differs
```

CI rebuilds the bundle and requires `git status --porcelain ui/dist` to be empty,
so a change under `ui/src` without a matching rebuild cannot merge.

## Test matrix

```bash
just verify                # everything below that runs without a model
just stress                # scripts/stress.sh 10 + scripts/stress-rest.sh 10

cargo test --all-targets   # 167 tests:
                           #   64 lib unit + 17 CLI + 37 core invariants
                           #   + 9 workflow-CLI + 4 MCP + 11 REST
                           #   + 9 REST concurrency + 4 transfer + 12 UI
cargo clippy --all-targets -- -D warnings
cargo fmt --check

bash scripts/stress.sh 10        # cross-process claim/write race (10 real CLI processes)
bash scripts/stress-rest.sh 10   # same race over HTTP + mixed HTTP/CLI writers

npm install
npx tsc -p tsconfig.json   # extension type check
npm run ui:typecheck       # dashboard (ui/) type check
npx vitest run             # 276 tests across 20 files: the extension
                           #   (unit/schema/mcp/e2e) plus the ui/ renderer,
                           #   api, poller, topology, dag, workflow, lazy,
                           #   focus and selection suites
npm run ui:browser         # real Chromium against a throwaway ledger (needs
                           #   Chromium; prints SKIP when none is installed)

npm run verify             # all of the above that can run non-interactively,
                           #   plus the three release-script suites and the
                           #   offline installer tests
bash scripts/test-install-script.sh  # installer only: platforms, versions,
                                     #   checksum tampering, atomic upgrade
just ui-dist-check         # committed ui/dist matches a fresh build
python3 scripts/check-doc-links.py   # README + docs/ relative links and anchors

just release-package       # host release archive in dist/
just release-check dist/apm-<version>-<target>.tar.gz  # layout + checksums + CLI smoke
```

GitHub Actions runs the same verify workflow on every push/PR
(`.github/workflows/ci.yml` → `verify.yml`). A `v*` tag builds the five-platform
matrix and publishes the archives with a GitHub Release; `workflow_dispatch`
builds the same artifacts without publishing. See [`release.md`](release.md).

`scripts/stress.sh` spawns 10 independent `apm` processes against one database and
asserts exactly one claim winner, exactly one successful write, and the expected
number of `claim_conflict` / `version_conflict` results.

### What the Rust suite covers

See `tests/core.rs`, `tests/cli.rs`, `tests/transfer.rs`:

- creating without context fails (`missing_context`), blank title fails;
- agent writes require a live claim; humans are exempt but audited;
- forged and expired tokens are rejected; an expired lease blocks writes;
- two agents cannot claim the same item; release/re-claim; human force takeover;
- `--recover-only` recovers an own active/expired lease, rejects foreign and
  never-claimed items, and rejects recovery after another session took the item
  over and released it again;
- context revisions keep readable history and reject no-ops and stale versions;
- generic updates bump the version; a second write with the same
  `--expect-version` fails with `version_conflict` while the event version matches
  the stored version;
- concurrent writers cannot lose an update;
- idempotency keys are bound to operation + item + payload (cross-item,
  cross-operation and changed-payload reuse all raise `idempotency_conflict`), and
  a byte-identical retry replays without a second event;
- plaintext claim tokens never appear in the database, WAL, or a replayed
  response (a raw sqlite/bytes assertion covers this);
- an empty `update` is rejected instead of burning a version;
- expiry cannot clobber a lease renewed after the scan;
- repeating an identical `progress` is a no-op, while a bookkeeping-only change
  writes but still does not advance the progress clock;
- attention replies are human-only; item-scoped withdraw requires a live claim and
  a matching session/requester; cross-project attention is rejected;
- one run syncing two items produces exactly one event per item; retries replay;
- `sync` never moves `last_progress_at` and keeps the claim's own lease;
- aborted runs record `run_aborted`, keep status/claim/progress intact, are
  idempotent, and are distinguishable in `get`/dashboard;
- completion requires **all** acceptance criteria (met/waived) or, when there are
  none, evidence — always plus progress; it auto-releases the claim, and closed
  items cannot be re-claimed;
- reparenting rejects closed parents and parent cycles of any depth; a second
  completion replaces the stale completion note; keyword search treats
  `%`/`_`/`\` literally;
- backup/export: 0600 files, refuse-overwrite, business data present, credentials
  absent, and every APM secret shape redacted;
- dashboard buckets, attention lifecycle, dependency cycles, diagnostics/doctor.

### What the plugin suite covers

`pi-apm/lib.test.ts`, `pi-apm/side-run.e2e.test.ts`, `pi-apm/extension.test.ts`:

- idempotency keys are unique per item and operation;
- the run digest is bounded and tolerant of malformed messages;
- preflight/decision matrix including the recursion guard;
- abort plan contains no progress/status calls and degrades to a diagnostic when
  the lease is gone;
- prompt content carries versions and per-item keys (never tokens), and the hard
  rules; every APM token shape is redacted;
- end-to-end (hook): `agent_settled` loads the session's claims, spawns the child
  with `--mode json -p --no-session --no-extensions` and `APM_SIDE_RUN=1`, the
  child's writes reach SQLite, a retried sync does not duplicate events, an
  aborted run takes the deterministic path without spawning a child, and a
  missing binary degrades to a persisted error report instead of throwing;
- end-to-end (tools): `apm_claim`, `apm_progress` (including the no-op repeat),
  `apm_attention`, `apm_context` (show + revise), `apm_status` and
  `apm_dashboard` work through the real CLI; failures map to real codes with
  recovery hints; no tool result leaks a claim token; tool writes carry
  `--session/--run/--agent`;
- end-to-end (reconciliation): a child that writes nothing is backed by a
  deterministic baseline sync, a two-item run where the child updates only one
  item syncs the other, a second settle for the same run is a no-op, and a
  configured `--apm-bin` is exposed as `apm` on the child's and the main agent's
  `PATH` — winning over a pre-existing `apm` on PATH;
- end-to-end (env lifecycle): stale `APM_RUN_ID`/`APM_SESSION_ID` are replaced by
  the current run and restored after the settle (including the disabled/skip
  path), with a fresh run id per run and no leakage;
- instance isolation: two extension instances in one process do not share a run
  id, a side-run guard, the injected env list or the PATH shim;
- tool cancellation forwards the `AbortSignal` to the child; stdout is capped at
  5,000,000 characters keeping the newest output; abort listeners are removed on
  both completion paths.

The e2e test uses a fake `pi` child (`APM_SIDE_RUN_PI`) plus the real `apm`
binary, so no model call and no network are involved.

## Real Pi smoke (distinct from the fake-child tests)

The vitest e2e tests drive the real extension handlers and the real `apm` binary,
but replace the child `pi` process with a script. Separately, the extension can be
verified inside a real Pi session:

```bash
# tool recognition + a real model call (side-run disabled for isolation)
APM_BIN=$PWD/target/debug/apm APM_SIDE_RUN_DISABLED=1 \
  pi -ne -e ./pi-apm/index.ts --mode json --no-session \
  -p "Call the apm_dashboard tool once, then report the READY / UNCLAIMED count."
# → tool_execution_start/end apm_dashboard (isError:false); model answered from it

# full run-end pipeline: real hook → real child pi → real SQLite writes
APM_BIN=$PWD/target/debug/apm pi -ne -e ./pi-apm/index.ts --mode json \
  -p "Call apm_claim on SMK-4 and nothing else. Then reply: DONE"
# → apm-run-sync entry: ran=true, childEvents=1016, errors=[], duration≈19s
# → events: run_synced by pi-side-run carrying the run id and the child's key
```

That second run is also where a real bug was found and fixed: the child's bash
tool had no `apm` on `PATH` when the extension was configured with `--apm-bin`.
The side-run now exposes the resolved binary in a private temp dir prepended to
the child's `PATH` (and as `$APM_BIN`), which is covered by an e2e assertion that
`apm --version` resolves inside the child.

## Repository layout

```
src/
  model.rs        status / health / actor / context document / JSON views
  error.rs        stable error codes and exit codes
  db.rs           schema + migrations (v3: workflows, snapshots, categories)
  workflow.rs     declarative state machine + guards + validation
  dashboard.rs    dashboard buckets + doctor
  session_store.rs  0600 per-session claim-token file (atomic replace)
  redact.rs       shared APM token scrub (MCP results, NDJSON export)
  store/          items, claims, context, workflows (versioned documents,
                  bindings, upgrades), transfer (backup/export), misc
                  (labels/deps/acceptance/evidence/attention), events, project
  mcp.rs          MCP stdio adapter: JSON-RPC framing + the eight tool mappings
  rest.rs         optional loopback REST adapter: HTTP/1.1 subset + routes
  main.rs         clap CLI, text rendering, JSON envelopes
  bin/apm-mcp.rs  the `apm-mcp` server binary (flags, db discovery, stdio loop)
  bin/apm-serve.rs  the optional `apm-serve` REST binary (bind rules, auth, token)
Justfile          build / install / verify / stress / ui recipes
scripts/
  stress.sh       CLI cross-process claim/write race
  stress-rest.sh  REST variant: servers, HTTP racers, mixed HTTP/CLI writers
  ui-browser-check.mjs  real Chromium run over a throwaway ledger
  install.sh            one-line Linux/macOS installer (release download + verify)
  test-install-script.sh  offline installer tests: 4 platforms, versions,
                        checksum tampering, atomic upgrade, no bin-dir pollution
  check-doc-links.py    README/docs relative-link and anchor checker
  package-release.sh / verify-package.sh  release archive + offline verification
  test-release-scripts.sh / test-package-windows.sh / test-release-gate.sh
tests/
  core.rs         invariants (claims, versions, context, sync, completion, ...)
  cli.rs          exit codes, JSON contract, session-token reuse, guards
  mcp.rs          apm-mcp end to end: tool loop, token handling, custom states
  rest.rs         apm-serve end to end: auth, routing, errors, limits
  rest_concurrency.rs  races: HTTP claims/versions, HTTP+CLI, token bootstrap
  transfer.rs     backup/export: redaction, 0600, refuse-overwrite
  ui.rs           apm ui end to end over raw TCP
  workflow_cli.rs workflows: bind/upgrade/migrate/validate
pi-apm/
  index.ts        hooks, tools, commands (the only file that imports pi)
  lib.ts          pure logic: digest, decisions, keys, prompts, parsing, report text
  render.ts       the `apm-run-sync` entry renderer (pi-tui components)
  *.test.ts       unit, schema, renderer, MCP-client, extension, side-run e2e
ui/
  index.html, vite.config.ts, tsconfig.json
  src/            vanilla-TS dashboard: api, poller, transform, render, detail,
                  topology, dag, workflow, selection, lazy, focus + happy-dom tests
  dist/           committed build output, embedded by build.rs (no Node needed)
docs/             user and operator documentation (see the map in README.md)
.github/workflows/  ci.yml (verify), verify.yml (shared), release.yml (matrix + tag)
```

## Design notes and boundaries

- **Local-first, single writer file.** SQLite in WAL mode is the authority. The
  CLI and its `--json` envelope are the contract; MCP, REST and the dashboard are
  optional adapters over the same `Store`, never a second source of truth.
- **Agent-native, human-friendly.** Every command has a text rendering and a
  `--json` envelope; humans get `apm dashboard`, `/apm-status`, `/apm-doctor`.
- **Claims over assignment.** No central scheduler: an agent claims work, and the
  lease makes abandonment visible instead of silent.
- **Context is a document, not a transcript.** Summarising chat into a ledger is
  lossy; requiring goal/current_state forces the handover to be written down.
- **The hook does the bookkeeping.** The main agent may forget; the run-end
  side-run exists precisely because "the agent will remember to update the ticket"
  is not a protocol.
- **Workflows are data.** States, transitions and guards are a versioned document
  per project; items snapshot it at creation, so a rebinding never reinterprets
  existing work.
- **Explicit non-goals:** sprints/boards, time tracking, notifications/email,
  multi-user auth, cross-project dependencies, attachments, and a hosted/hive
  service. The ledger is a local file; sharing means a loopback REST server or a
  backup, not a cloud account.

Known limitations (mostly about the optional adapters) are listed where they
belong: [`pi-plugin.md`](pi-plugin.md#known-gaps--honest-limitations),
[`mcp.md`](mcp.md#6-limitations), [`rest-ops.md`](rest-ops.md),
[`release.md`](release.md).
