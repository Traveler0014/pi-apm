# Pi plugin (`pi-apm`)

The extension makes the ledger self-maintaining inside a Pi session: it exposes
tools for the working loop, and after every run it spawns a short-lived child
agent that records what actually happened. The main agent is never trusted to
remember the bookkeeping — that is the point.

```bash
pi install git:github.com/Traveler0014/pi-apm   # into ~/.pi/agent/settings.json
pi list                                        # should show pi-apm
pi install -l ./                               # project-scope (.pi/settings.json)
pi -e ./pi-apm/index.ts                        # try it for one invocation
```

`-l` writes a project file that Pi only honours after project trust is granted
(`--approve`). The extension is TypeScript loaded through Pi's jiti runtime. The
modules Pi supplies to extensions (`@earendil-works/pi-coding-agent`,
`@earendil-works/pi-ai`, `@earendil-works/pi-tui`, `typebox`) are declared as
`peerDependencies` with a `"*"` range — the host-provided contract — so Pi maps
them to its own copies and an install cannot create duplicate runtime modules.
Pinned versions for local development live in `devDependencies` only; the package
has no runtime `dependencies`. The only external program it needs is `apm` (or an
explicit `--apm-bin /path/to/apm`).

## What it adds

| Surface | Name | Purpose |
|---|---|---|
| tool | `apm_dashboard` | read the status board |
| tool | `apm_claim` | take the lease before working |
| tool | `apm_release` | give the lease back |
| tool | `apm_progress` | record substantive progress (+ next/blocker/waiting) |
| tool | `apm_sync` | liveness only, never progress |
| tool | `apm_status` | workflow transition (guards enforced) |
| tool | `apm_context` | read or revise the handover document |
| tool | `apm_attention` | structured question for a human |
| command | `/apm-status` | print the board into the session |
| command | `/apm-doctor` | db/claims/diagnostics summary |
| hook | `agent_settled` | run the end-of-run side-run |
| entry renderer | `apm-run-sync` | compact TUI card for each settled run |

Tokens are never placed in the model's tool results: the CLI persists them to the
session file, and the tools just add `--session`.

## Flags and environment

| Flag | Default | Meaning |
|---|---|---|
| `--apm-bin <path>` | `$APM_BIN` or `apm` | CLI binary |
| `--apm-agent <name>` | `pi` | actor name for tool writes (the side-run appends `-side-run`) |
| `--apm-disabled` | false | keep the tools, disable the run-end side-run |
| `--apm-side-run-timeout <s>` | `180` | hard timeout for the child (exact: no hidden minimum, so `5` really means 5s) |
| `--apm-side-run-model <m>` | inherit | cheaper/faster model for the side-run |

Environment: `APM_BIN`, `APM_SIDE_RUN=1` (set by the plugin for the child;
**disables** the hook), `APM_SIDE_RUN_DISABLED=1`, and the test hook
`APM_SIDE_RUN_PI` (override the child command).

### `--apm-bin` and the PATH shim

If you configure `--apm-bin`, that binary wins: the extension compares real paths
against whatever `apm` is on `PATH` and, if they differ, creates a private shim
directory and prepends it to `PATH` for the session (restored on shutdown), so the
model can literally type `apm ...` in its bash tool. The child side-run gets the
same treatment (its own temp `bin/` directory plus `$APM_BIN`), because the child
only has a bash tool.

While a run is active the extension writes the **current**
`APM_RUN_ID`/`APM_SESSION_ID` into the process env — overriding stale values from
an earlier run — and restores the previous values on every settle path and on
shutdown, so bash-invoked `apm` writes are auditable exactly like tool calls
without leaking across runs. Each extension instance owns that state in a
closure: two pi-apm instances in one process cannot share a run id, a side-run
guard, the injected-env list or the PATH shim.

## Side-run lifecycle

```
agent_start ──▶ run id + empty digest
agent_end  ──▶ fold messages/tool calls/errors into the digest (bounded)
agent_settled
   ├─ preflight: disabled? APM_SIDE_RUN? session id? project db?      → skip + entry
   ├─ load this session's claims (active + expired; tokens stay in the 0600 file)
   │     none → skip ("side-run never claims new work") + entry
   ├─ aborted run  → deterministic abort sync (no LLM):
   │     per live claim:  apm sync --outcome aborted  (liveness only)
   │     per dead claim:  apm diag  --code claim_expired_after_abort
   └─ normal run   → child pi --mode json -p --no-session --no-extensions
                        --tools bash --append-system-prompt <rules> <task>
```

The child prompt contains the session/run ids, the claimed items **with their
versions** (never tokens — the CLI resolves them from the 0600 session file), the
run digest (user prompts, final assistant texts, tool calls, touched files, tool
errors), and the **per-item, per-operation idempotency keys** to use. Every
digest field, the task prompt, the child's reported output and the persisted
report are scrubbed of every APM secret shape (`apm_[A-Za-z0-9_-]{32,}`: claim
tokens plus `apm_rest_`/`apm_ui_`/`apm_boot_`) before they are used. The child is
started with `APM_BIN` and a private `PATH` entry containing `apm`, so a
`--apm-bin` configuration still works inside its bash tool. The system prompt
forbids creating items, claiming new work, releasing claims, inventing progress,
and closing items without evidence.

**Deterministic reconciliation (the guarantee, not the hope).** After the child
exits — and also when it fails or times out — the hook queries this run's events
and checks every claimed item for a `progress_*`/`run_*` event carrying the run
id. Missing items get a baseline `apm sync` (liveness only) with a stable
`…:reconcile:sync` key; items whose lease died get an `apm diag` instead. Retries
replay instead of duplicating, and reconciliation never writes progress, status,
or a release.

After the child exits, the hook appends an `apm-run-sync` session entry
(`{ran, mode, reason, runId, sessionId, items, calls, childEvents, output,
errors, durationMs}`; `calls` counts deterministic CLI calls on the abort path,
`childEvents` counts the child's JSONL events) and, when something went wrong,
writes a project-level `side_run_failed` diagnostic plus a UI warning. **The hook
never throws**, so a broken plugin cannot break the main reply.

## Guarantees the Rust side enforces (so the plugin cannot cheat)

| Requirement | Mechanism |
|---|---|
| Side-run reuses the current claim | every write passes `--session`; the CLI reads the token from the 0600 session file and verifies it against the live claim |
| Expired lease must be re-acquired explicitly | writes fail with `claim_invalid`; the child may call `apm claim --recover-only` |
| Recovery cannot become new work | `--recover-only` only succeeds while the item's **latest** claim is this same session's and is active/expired; a takeover by another session (even if released again) ends that right. `APM_SIDE_RUN=1` sets the policy automatically for the CLI |
| Retries cannot duplicate events | idempotency key per (session, run, item, operation) + `idempotency` table + unique index on event keys |
| Heartbeat ≠ progress | `apm sync` moves only `last_run_sync_at`/`last_heartbeat_at`, and renews the claim's own lease |
| Abort is not progress | abort path is deterministic, never calls `apm progress`, never changes status |
| No recursion | `--no-extensions` child + `APM_SIDE_RUN=1` guard |
| Tokens never reach the child | `APM_SIDE_RUN=1` suppresses the plaintext in `claim`/`claims --with-tokens` stdout; the child authenticates via `--session` |
| One run, many items | keys include `item_id`; a run with two claims syncs both |

## Implementation notes

- **Abort signals are forwarded.** Every tool `execute` passes Pi's `AbortSignal`
  to the `apm` child (including the version prefetch used by `progress`/`status`/
  `context`), so cancelling a tool call or shutting down the session kills the
  child instead of leaving it running.
- **Child output is bounded.** `runProcess` keeps at most 5,000,000 characters of
  stdout (the newest output, which is what the summary needs); a runaway child
  cannot exhaust the host's memory. Normal `apm --json` payloads and side-run
  event streams are far below the cap.
- **No listener leaks.** The abort listener is removed when the child finishes,
  on the abort path and on spawn failure.

## Known gaps / honest limitations

- The side-run is **one extra model call per run** unless you set
  `--apm-side-run-model`. It is a full agent turn: on slow providers it can hit the
  timeout, in which case reconciliation still records the run and the diagnostics
  show the timeout. A fast model is strongly recommended.
- If the model is unavailable the ledger still gets the baseline sync plus a
  diagnostic. That is visible in `/apm-doctor` and `apm doctor --json`.
- The child cannot choose its own model cost policy beyond the flag; there is no
  batching across sessions.
- A side-run that hangs is killed after the timeout, and the partial writes it
  already made remain (they are idempotent, not transactional across CLI calls).
- **Quitting never waits for a side-run.** `session_shutdown` aborts the in-flight
  side-run: the child is killed through its own process group with SIGKILL
  immediately (a wedged child ignores SIGTERM, and waiting 3s would stall every
  `/quit`), so `apm` grandchildren cannot escape either. The ordinary timeout path
  still escalates politely (SIGTERM, SIGKILL after 3s). In-flight `apm` calls are
  cancelled too. The run-sync entry is recorded as `skipped` + `aborted: true`
  (reason "session shutdown"), including when the shutdown lands during
  reconciliation — a run that wrote nothing must never render as `run synced`.
  Cancellation is not a failure, so no `side_run_failed` diagnostic is written and
  no warning is shown.
- The main agent's tools do not retry `version_conflict` automatically; they
  return the structured conflict so the model can re-read and retry.
- `apm-run-sync` entries are rendered as a compact TUI card (mode, items,
  reconciliation, errors, duration); expanded entries add the run/session ids and
  up to six output lines. Output is truncated and claim tokens are redacted. In
  non-interactive (`-p`, JSON, RPC) runs the entry is still persisted but no
  component is drawn.
- There is no "run the side-run now" command: recovery of a missed run is manual
  (`apm claim --recover-only` plus normal agent work), and `/apm-doctor` shows
  what is stale.
- **Side-runs never see a token.** With `APM_SIDE_RUN=1`, `apm claim` still writes
  the token to the 0600 session file but prints `token: null`
  (`token_suppressed: true`, `token_source: session_file`) in both text and JSON,
  and `apm claims --with-tokens` is force-suppressed. A child therefore cannot
  paste a token into `progress_summary`/context even if it echoes command output.
  The extension additionally redacts every APM secret shape
  (`apm_[A-Za-z0-9_-]{32,}`) from digests, prompts, child output and reports.
- Ordinary CLI/agent usage (not a side-run) still prints the token **once** on
  `claim` and supports `claims --with-tokens` — that is the documented local
  contract for humans and scripts. `apm` does not try to stop a model from reading
  local files; it only closes the accidental "command output lands in the ledger"
  channel described above.

## Reading and triaging side-run reports

Every settled run appends an `apm-run-sync` entry. In an interactive session the
extension renders it as a card; in `-p`/JSON/RPC runs the entry is still
persisted, it is simply not drawn (and nothing else changes).

**Collapsed** — one line of state, then the facts that usually matter:

```
[apm] run synced (20.8s · 1 item(s)) · run 2026-11-16T12:37:2…
  items: TUI-2
· TUI-2 already recorded
  Ledger synchronised. The run's claim and progress were already
  persisted via the agent's tool calls, …
```

**Expanded** (the same key that expands tool output, `ctrl+o`) adds up to twelve
reconciled items, six errors, six output lines and the metadata line:

```
  at 2026-11-16T12:37:29.997Z · session 01a1e45c-26fe-7506… · run 2026-11-16T12:37:2…
```

| element | meaning |
|---|---|
| `[apm] run synced` | the side-run ran and reconciled the session's claims |
| `[apm] run synced with N issue(s)` | it ran, but the child reported N errors (see `!` lines) |
| `[apm] run not synced` / `… with N issue(s)` | it did **not** sync: ran=false, e.g. a timeout. Never reads as success |
| `[apm] skipped (…)` + reason | nothing was claimed, recursion guard, the side-run is disabled, or the session shut down before it finished (`aborted: true`, including a shutdown during reconciliation) |
| `aborted run · liveness only` | the run was aborted; only a liveness sync was recorded, never progress |
| `duration · N item(s) · run …` | wall time, how many claims were in play, short run id |
| `items: …` | the item identifiers this run touched (`+N more` beyond six) |
| `✓ APM-7 sync` | a write landed for that item |
| `· APM-7 already recorded` | the item was already covered by this run (idempotent replay) |
| `✗ APM-7 failed (claim_invalid)` | the write for that item failed; the code is the CLI code |
| `! …` | errors reported by the child, redacted and truncated |
| indented plain lines | the child's summary of what it did, truncated |

Claim tokens never appear: output and errors are scrubbed with the same
`apm_[A-Za-z0-9_-]{32,}` redaction the rest of the plugin (and the Rust side)
uses.

**Where to look when something is off**

| symptom | diagnosis |
|---|---|
| no card at all | side-run disabled (`--apm-disabled`, `APM_SIDE_RUN_DISABLED=1`), the session holds no claim, or a non-interactive run. The entry is persisted either way — check `apm events --limit 20` for `run_synced`/`run_aborted` |
| `[apm] skipped … session holds no claim` | the agent never claimed anything, so there is nothing to reconcile: claim before working |
| `✗ … failed (claim_invalid)` | the lease expired mid-run: `apm claim <item> --takeover --reason "…"` |
| `✗ … failed (version_conflict)` | another writer won the race: re-read the item and retry once |
| `✗ … failed (claim_required)` | a completion write was attempted after the claim was released |
| `… timed out after Nms` | raise `--apm-side-run-timeout`, or set a faster `--apm-side-run-model`; reconciliation still records the run |
| `side_run_failed` diagnostic | `apm doctor --json` lists pending diagnostics, `/apm-doctor` prints the summary |
| card says issues but the ledger looks right | the card is a view: trust `apm get <item> --json`, `apm events`, and `apm doctor` |

## Tests behind the extension

`pi-apm/lib.test.ts` (pure logic, digest, decisions, keys, redaction),
`pi-apm/tools.test.ts` (the tool schemas a model sees), `pi-apm/extension.test.ts`
(instance isolation, abort forwarding, stdout cap, listener cleanup),
`pi-apm/render.test.ts` / `render.snapshot.test.ts` (card text and component
structure), `pi-apm/mcp-client.test.ts` (real MCP client), and
`pi-apm/side-run.e2e.test.ts` (hook → child process → SQLite, with a fake `pi`).
