# AgentPM (`apm`)

A **local-first, agent-native work ledger**. Not a Jira: no server, no daemon, no
web UI to keep in sync. One SQLite file is the authority; the CLI and its JSON
output are the interface.

AgentPM exists to answer six questions about work performed by AI agents:

1. **Who is doing what?** — atomic claims with leases, not vibes.
2. **What actually happened?** — progress recorded separately from liveness.
3. **What is next?** — an explicit `next_action` per item.
4. **What is blocked, and on what?** — blockers and dependency edges.
5. **Who is waiting on a human?** — structured attention requests, not chat scrollback.
6. **Who vanished?** — expired leases and "no substantive progress" views.

`apm` gives you a Rust CLI, a SQLite schema, an audit trail, and a Pi extension
that runs a **side-run after every agent run** so the ledger stays honest even
when the main agent forgets (or chooses not to) update it. A deterministic
reconciliation step then guarantees that every claimed item carries a record of
the run, even if the child model does nothing or fails.

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

---

## 1. Install

### Rust CLI

Three binaries come out of the same crate: `apm` (the CLI), `apm-mcp` (the MCP
stdio server) and `apm-serve` (the optional loopback REST adapter) — see §8.

With [`just`](https://github.com/casey/just):

```bash
just build                       # cargo build --release → target/release/{apm,apm-mcp,apm-serve}
just install                     # → $HOME/.local/bin/{apm,apm-mcp,apm-serve}
BINDIR=/usr/local/bin just install   # or any directory on PATH
apm --version && apm-mcp --version && apm-serve --version
```

`just install` builds first and installs **all three** binaries with mode `0755`;
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
apm --version && apm-mcp --version && apm-serve --version
```

Requires Rust 1.85+ (edition 2024). SQLite is bundled; there is no system
dependency and no network access at runtime.

### Pi extension

```bash
# from this repository
pi install ./            # or: pi install git:github.com/Traveler0014/pi-apm
pi list                  # should show pi-apm

# try it without installing
pi -e ./pi-apm/index.ts
```

The extension is TypeScript loaded through Pi's jiti runtime. The modules Pi
supplies to extensions (`@earendil-works/pi-coding-agent`,
`@earendil-works/pi-ai`, `@earendil-works/pi-tui`, `typebox`) are declared as
`peerDependencies` with a
`"*"` range — the host-provided contract — so Pi maps them to its own copies and
a physical install cannot create duplicate runtime modules. Pinned versions for
local development live in `devDependencies` only; the package has no runtime
`dependencies`. The only external program it needs is `apm` (or an explicit
`--apm-bin /path/to/apm`).

If you configure `--apm-bin`, that binary wins: the extension compares real paths
against whatever `apm` is on `PATH` and, if they differ, creates a private shim
directory and prepends it to `PATH` for the session (restored on shutdown), so the
model can literally type `apm ...` in its bash tool. The extension also writes the
**current** `APM_RUN_ID`/`APM_SESSION_ID` into the process env while a run is
active — overriding stale values from an earlier run — and restores the previous
values on every settle path and on shutdown, so bash-invoked `apm` writes are
auditable exactly like tool calls without leaking across runs.

`pi install` writes to `~/.pi/agent/settings.json`; with `-l` it writes
`.pi/settings.json` in the current project, which Pi only honours after project
trust is granted (`--approve`). `pi -e ./pi-apm/index.ts` loads it for one
invocation without touching any settings file.

---

## 2. Initialise a project

```bash
cd ~/code/my-repo
apm init --name "My Repo"            # creates .apm/apm.db, derives prefix MYR (or --prefix)
apm init --name "My Repo" --lease-seconds 900 --stale-after 300 \
         --no-progress-after 86400    # optional thresholds, persisted per project
apm project show                      # resolved db path, settings
```

`apm init` is idempotent: running it again in the same database returns the
existing project instead of failing. Databases can hold several projects; pass
`--project <name>` (or `APM_PROJECT`) when they do.

Version-control guidance:

```gitignore
# Default: keep the ledger local (the session token files are bearer secrets).
.apm/

# Sharing a ledger through git is possible but opt-in: un-ignore the database
# only, and never commit the WAL/SHM or the token store.
#   .apm/*
#   !.apm/apm.db
```

`.apm/sessions/` holds claim tokens with mode 0600. They are local credentials,
not project data: never copy them into an artifact, a ticket, or a prompt that
leaves the machine.

---

## 3. Core concepts

### Business status ≠ execution health

| Concept | Values | Who sets it |
|---|---|---|
| **status** (workflow) | `ready` `doing` `blocked` `review` `done` `cancelled` | agent/human via `apm status` |
| **health** (derived) | `active` `stale` `expired` `released` `superseded` `unclaimed` `ended` | computed from claims + clock |
| **progress** | free text + `last_progress_at` | only `apm progress` |
| **attention** | open/resolved requests | agents ask, humans answer |

Health is never writable. A heartbeat can keep an item `active` forever without
that meaning anything changed — which is exactly why `last_progress_at` is a
separate clock.

### Three clocks

| Field | Moved by | Never moved by |
|---|---|---|
| `last_heartbeat_at` | `apm claim`, `apm heartbeat`, `apm sync` | progress/status writes |
| `last_progress_at` | `apm progress` only | heartbeat, run sync, abort |
| `last_run_sync_at` | `apm progress`, `apm sync` | reading, listing |

`apm sync` exists so an agent can say *"this run ended"* without pretending it
*"made progress"*. The dashboard reports all three per item.

### Claims

A claim is a lease with a bearer token:

- **atomic**: one `active` claim per item, enforced by a partial unique index and
  `BEGIN IMMEDIATE`; concurrent claimers get `claim_conflict` (exit 5);
- **expiring**: default lease 900s (`--lease`, min 30s); after expiry every write
  is rejected with `claim_invalid` even if the token is right;
- **token-verified**: the database stores only `sha256(token)`; the plaintext is
  returned once at claim time and optionally persisted to
  `.apm/sessions/<session>.json` (mode 0600) for the agent session that owns it;
- **renewable**: `apm heartbeat` extends the lease without touching progress;
- **releasable**: `apm release` requires the same token that claim returned;
- **recoverable**: an expired lease can be re-claimed (`--takeover`), which is
  recorded as `claim_taken_over`. Only a **human** (`--human --takeover --reason`)
  can take over a *live* claim;
- **session-safe**: a session that lost its token file can re-claim its own live
  lease (supersedes itself), never somebody else's.

Every work-item write — progress, status, context revision, labels, dependencies,
acceptance, evidence, attention requests — requires a valid claim. The exceptions
are exactly the operations that do not mutate an item: `create`, `get`, `list`,
`dashboard`, `events`, `claim` itself, `attention reply`, and `diag`.

Attention permissions are deliberately asymmetric: **agents ask, humans answer**.
`attention reply` is human-only (v1) — an agent must not resolve the question it
posed. `attention withdraw` is allowed for a human, for the requester of a
project-level request, or for an agent that still holds a valid claim on the item
*and* matches the requesting session/name. An attention request may only be
attached to a work item in the same project.

### Context is versioned handover, not chat history

Every item carries a structured context document:

```json
{
  "goal": "what done means",
  "current_state": "where things stand now",
  "constraints": ["no server", "rustfmt clean"],
  "references": ["src/store/claims.rs", "https://…"],
  "notes": "anything else"
}
```

`goal` and `current_state` are mandatory at creation and on every revision —
a successor must be able to act **without** the previous conversation. Every
revision writes a full snapshot row (`context_versions`) with author, actor kind,
session, run, reason, timestamp, content hash and `revises_version`. History is
therefore real snapshot history (before and after both recoverable), not a diff
that can rot. No-op revisions are rejected.

---

## 4. Agent standard workflow

```bash
# 0. what is going on?
apm dashboard --json

# 1. pick work and take the exclusive lease (token is returned once)
apm claim APM-3 --lease 900 --json
#    -> {"ok":true,"token":"apm_…","claim":{…},"session_token_saved":true}
#    With APM_SESSION_ID set, later commands reuse the saved token.

# 2. move it into flight (this is a workflow transition, not a heartbeat)
apm status APM-3 --to doing --expect-version 1

# 3. read the handover context before touching anything
apm context show APM-3
apm get APM-3 --json          # includes claim, health, deps, acceptance, attention

# 4. do the work …

# 5. record only what actually changed (bumps version + last_progress_at)
apm progress APM-3 --expect-version 2 \
  --summary "rewrote claim parsing; cargo test passes (32 tests)" \
  --next-action "run the Pi side-run e2e" --json

# 6. keep the lease alive during long work without faking progress
apm heartbeat APM-3 --extend 900

# 7. if a human decision is needed, ask structurally
apm attention request --item APM-3 \
  --question "Rotate staging credentials now?" \
  --context "Deploy is blocked; staging creds expire in 2h." \
  --option yes --option no --recommend yes --json

# 8. attach evidence, then complete (done requires all criteria met/waived, or
#    evidence when the item has no criteria — plus a progress summary)
apm evidence add APM-3 --kind test --value "cargo test --all" --note "32 passed"
apm status APM-3 --to done --expect-version 5 --note "verified by e2e; see #123"

# 9. or hand off / stop
apm release APM-3 --reason "handing to next agent"
```

**Version discipline.** Every mutating response carries the new `version`, which
is exactly the value stored in the database and recorded in the event. An update
whose fields all match the current state is rejected (`invalid_input`) instead of
burning a version; `apm progress` reports the same situation as `no_op: true`. Pass it as
`--expect-version` on the next write. A lost race returns `version_conflict`
(exit 4) with `expected`/`actual` — re-read and retry, never blindly overwrite.

**Idempotency.** Any mutating command accepts `--idempotency-key`. A key is bound
to `(operation, item, request payload)`:

* identical request → the first response is replayed and **no** new events are
  written;
* same key reused for another item, operation, or payload → `idempotency_conflict`
  (exit 2). Silent cross-item replays are exactly the bug this prevents.

Claim tokens are deliberately **not** part of the stored response, so the database
can never hand a token back. On a replay the CLI returns `replayed: true` with
`token: null` unless the *same local session* saved the token to its 0600 session
file, in which case it returns that copy with `token_source: "session_file"`.
That keeps a retried claim recoverable without ever persisting the plaintext in
SQLite (verified by a raw-bytes assertion over the DB and WAL).

**Progress is not idempotent-by-accident.** `apm progress` compares the request
with the current state:

| request vs current | effect |
|---|---|
| summary and all provided fields identical | **no-op**: no version bump, no event, `last_progress_at` untouched (`no_op: true`) |
| same summary, different `next_action`/`blocker`/waiting fields | bookkeeping update: version bumps, event `progress_updated`, `substantive: false`, **`last_progress_at` untouched** |
| new summary | substantive: version bumps, event `progress_recorded`, `last_progress_at`/`last_run_sync_at` advance |

That asymmetry is the point: repeating yourself cannot make an item look alive.
`apm sync` is the explicit way to say "this run ended, nothing changed".

**Humans.** `apm --human <command>` (name from `--agent` or `$USER`) can write
without a claim, force-release a claim (`release --force --reason`), and force a
live takeover. Those actions are audited with `actor_kind=human`.

---

## 5. Declarative workflow

```bash
apm workflow --json
```

The default state machine (v1) is data, not `match` arms:

| from \ to | ready | doing | blocked | review | done | cancelled |
|---|---|---|---|---|---|---|
| **ready** | — | ✓ | ✓ `reason` | — | — | ✓ `reason` |
| **doing** | ✓ `reason` | — | ✓ `reason` | ✓ | ✓ `evidence+progress` | ✓ `reason` |
| **blocked** | ✓ `reason` | ✓ `reason` | — | — | — | ✓ `reason` |
| **review** | — | ✓ `reason` | ✓ `reason` | — | ✓ `evidence+progress` | ✓ `reason` |
| **done** | — | ✓ `reason` (reopen) | — | — | — | — |
| **cancelled** | ✓ `reason` (reopen) | — | — | — | — | — |

`done` requires **both**:
1. `evidence_or_acceptance` — if the item defines acceptance criteria, **all** of
   them must be `met` or `waived` (one green criterion does not close a
   five-criterion item; evidence cannot substitute for an unmet criterion). If
   the item defines **no** criteria, at least one evidence record is required;
2. `progress_or_note` — a non-empty progress summary or completion note.

Closing an item auto-releases its claim (no zombie leases on finished work).
Agents cannot claim a closed item, so reopening is a human action:
`apm --human status <id> --to <state> --reason "..."`; the item then behaves
normally again (keeping its workflow snapshot).

**Workflow schema (v2).** States, transitions and guards are a versioned
document, stored per project as `(id, version)`:

```json
{
  "id": "qa-flow", "version": 1, "name": "QA flow", "initial": "ready",
  "states": [
    { "key": "ready", "label": "Ready", "category": "open",   "terminal": false },
    { "key": "qa",    "label": "QA",    "category": "review", "terminal": false },
    { "key": "done",  "label": "Done",  "category": "closed", "terminal": true },
    { "key": "cancelled", "label": "Cancelled", "category": "closed", "terminal": true, "cancel": true }
  ],
  "transitions": [
    { "from": "ready", "to": "qa", "requires": [] },
    { "from": "qa", "to": "done", "requires": ["progress_or_note", "evidence_or_acceptance"] },
    { "from": "qa", "to": "cancelled", "requires": ["reason"] }
  ],
  "guards": {
    "reason": { "kind": "reason" },
    "evidence_or_acceptance": { "kind": "evidence_or_acceptance" },
    "progress_or_note": { "kind": "progress_or_note" }
  }
}
```

- **Categories** (`open`/`active`/`waiting`/`review`/`closed`) are the contract
  shared with the dashboard, health and claim logic; custom state keys never leak
  into those decisions.
- **`terminal`** must agree with `category: closed`; **`cancel`** marks a closed
  cancel outcome (a `reason`-kind guard is mandatory on transitions into it).
- Guards are a **named library**: a transition references names, and validation
  works on the guard *kind*, so `completion_evidence` (kind
  `evidence_or_acceptance`) is equivalent to the built-in name.
- Validation rejects: duplicate/invalid keys, an initial state that is missing or
  closed, unreachable states, non-closed dead ends, unknown guard references,
  unknown guard kinds, same `(id, version)` with different content, and any
  attempt to weaken completion — transitions into a completion state must
  require an evidence-kind **and** a progress-kind guard.
- Items **snapshot** `workflow id + version` at creation. Transitions are always
  validated against that snapshot, so re-binding a type never reinterprets
  existing items. The default `apm-default` v1 workflow reproduces the original
  six-state machine exactly (same transitions and completion rules).

### CLI configuration and upgrades

```bash
apm workflow                       # project default document (legacy alias of `show`)
apm workflow show [--type TYPE] [--id ID --version V]
apm workflow list                  # registered versions, marks the project default
apm workflow types                 # item-type bindings + project default
apm workflow validate --file wf.json          # pure: no DB/project needed (CI-friendly)
apm workflow set --file wf.json --bind spike,bug [--default] [--dry-run]
apm workflow bind --type spike --id qa-flow --version 2
apm workflow upgrade --type spike --to-version 2 --migrate-items \
                     [--map qa=reviewing] [--dry-run]
```

`set` is **atomic and idempotent**: the document, every `--bind` and `--default`
are validated first and applied in one transaction, so an invalid second type
leaves nothing behind (no row, no binding, no event); replaying the same content
writes nothing and adds no events. `upgrade` plans before it writes: an open item
whose state is not declared by the target and has no `--map` blocks the whole
upgrade (`workflow_violation`, exit 6). `--migrate-items` re-snapshots open items,
records a `workflow_migrated` event carrying previous/new workflow, state and
item version, and bumps the item version. Closed items **never** migrate — they
keep their historical snapshot even when the target declares the same state
(reported as `skipped_closed`); reopen one through the human path
(`apm --human status ...`) before upgrading it.

Mappings that would land an open item in a **closed** state are refused: that
would close the item without running the completion guards, so upgrade to a
non-closed state and finish it with `apm status`. Mapping targets are validated
against the target workflow, so a typo is reported as such. Applying a plan is a
compare-and-swap on each item's `(version, state)`: if another process changed an
item in between, the whole transaction rolls back with `version_conflict`
(exit 4) and nothing is half-migrated. Re-running a completed upgrade is a no-op.
`--dry-run` prints the plan without writing.

**Upgrade playbook:** `validate` the new document → `set` it (no binding) →
`upgrade --dry-run --migrate-items [--map ...]` → apply → `apm dashboard` and
`apm doctor` to confirm. An invalid document always fails loudly
(`invalid_input`, exit 2); `apm doctor` reports `workflow_valid: false` if a
stored document cannot be parsed. Legacy `projects.workflow_json` documents are
imported into the versioned workflow table by the v2→v3 migration.

---

## 6. Dashboard and doctor

```bash
apm dashboard            # human view
apm dashboard --json     # machine view
```

Buckets (an item may appear in several — these are views, not exclusive states):

| bucket | meaning |
|---|---|
| `waiting_human` | open attention requests or `waiting_on_human` flag |
| `lost_lease` | health `stale` or `expired` |
| `recovery_failed` | side-run reported it could not recover a lease (see doctor) |
| `stalled_no_progress` | open item whose `last_progress_at` is older than the threshold (heartbeats do not help) |
| `waiting_dependencies` | open item with an upstream dependency that is not done/cancelled |
| `in_progress` | status `doing`/`review` **or** a live lease is attached |
| `recently_done` | done inside `done_recent_seconds` |
| `ready_unclaimed` | `ready` and not actively held — the "pick me up" queue |

Every row includes `progress_summary`, `next_action`, `health`,
`last_heartbeat_at`, `last_progress_at`, `last_run_sync_at`, the last run outcome
(`completed` / `aborted`), open attention questions, and dependency states.

```bash
apm doctor --json   # db path, open items, active claims, expired-not-swept claims,
                    # open attention, unresolved side-run diagnostics, workflow validity
apm expire          # sweep expired leases into status=expired (idempotent)
```

---

## 7. Pi plugin

### 7.1 What it adds

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
| entry renderer | `apm-run-sync` | compact TUI card for each settled run (see §7.6) |

Tokens are never placed in the model's tool results: the CLI persists them to the
session file, and the tools just add `--session`.

### 7.2 Flags

| Flag | Default | Meaning |
|---|---|---|
| `--apm-bin <path>` | `$APM_BIN` or `apm` | CLI binary |
| `--apm-agent <name>` | `pi` | actor name for tool writes (the side-run appends `-side-run`) |
| `--apm-disabled` | false | keep the tools, disable the run-end side-run |
| `--apm-side-run-timeout <s>` | `180` | hard timeout for the child |
| `--apm-side-run-model <m>` | inherit | cheaper/faster model for the side-run |

Environment: `APM_BIN`, `APM_SIDE_RUN=1` (set by the plugin for the child;
**disables** the hook), `APM_SIDE_RUN_DISABLED=1`, and the test hook
`APM_SIDE_RUN_PI` (override the child command).

### 7.3 Side-run lifecycle

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
report are scrubbed of `apm_[0-9a-f]{32,}` tokens before they are used. The child
is started with `APM_BIN` and a private `PATH` entry containing `apm`, so a
`--apm-bin` configuration still works inside its bash tool. The system prompt forbids creating items, claiming new work,
releasing claims, inventing progress, and closing items without evidence.

**Deterministic reconciliation (the guarantee, not the hope).** After the child
exits — and also when it fails or times out — the hook queries this run's events
and checks every claimed item for a `progress_*`/`run_*` event carrying the run
id. Missing items get a baseline `apm sync` (liveness only) with a stable
`…:reconcile:sync` key; items whose lease died get an `apm diag` instead. Retries
replay instead of duplicating, and the reconciliation never writes progress,
status, or a release.

After the child exits, the hook appends an `apm-run-sync` session entry
(`{ran, mode, reason, runId, sessionId, items, calls, childEvents, output,
errors, durationMs}`; `calls` counts deterministic CLI calls on the abort path,
`childEvents` counts the child's JSONL events) and, when something went wrong, writes a project-level
`side_run_failed` diagnostic plus a UI warning. **The hook never throws**, so a
broken plugin cannot break the main reply.

### 7.4 Guarantees the Rust side enforces (so the plugin cannot cheat)

| Requirement | Mechanism |
|---|---|
| Side-run reuses the current claim | every write passes `--session`; the CLI reads the token from the 0600 session file and verifies it against the live claim |
| Expired lease must be re-acquired explicitly | writes fail with `claim_invalid`; the child may call `apm claim --recover-only` |
| Recovery cannot become new work | `--recover-only` only succeeds for an item this same session previously claimed; `APM_SIDE_RUN=1` sets that policy automatically for the CLI |
| Retries cannot duplicate events | idempotency key per (session, run, item, operation) + `idempotency` table + unique index on event keys |
| Heartbeat ≠ progress | `apm sync` moves only `last_run_sync_at`/`last_heartbeat_at` |
| Abort is not progress | abort path is deterministic, never calls `apm progress`, never changes status |
| No recursion | `--no-extensions` child + `APM_SIDE_RUN=1` guard |
| Tokens never reach the child | `APM_SIDE_RUN=1` suppresses the plaintext in `claim`/`claims --with-tokens` stdout; the child authenticates via `--session` |
| One run, many items | keys include `item_id`; a run with two claims syncs both |

### 7.5 Known gaps / honest limitations

- The side-run is **one extra model call per run** unless you set
  `--apm-side-run-model`. It is a full agent turn: on slow providers it can hit the
  timeout (in this environment the default model once burned the whole 180s), in
  which case reconciliation still records the run and the diagnostics show the
  timeout. A fast model is strongly recommended.
- If the model is unavailable the ledger still gets the baseline sync plus a
  diagnostic. That is visible in `/apm-doctor` and `apm doctor --json`.
- The child cannot choose its own model cost policy beyond the flag; there is no
  batching across sessions.
- A side-run that hangs is killed after the timeout, and the partial writes it
  already made remain (they are idempotent, not transactional across CLI calls).
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
- Custom workflow *states* are rejected in v1 (the six business states are fixed);
  only transitions and guards are data (see §5).
- **Side-runs never see a token.** With `APM_SIDE_RUN=1`, `apm claim` still writes
  the token to the 0600 session file but prints `token: null`
  (`token_suppressed: true`, `token_source: session_file`) in both text and JSON,
  and `apm claims --with-tokens` is force-suppressed. A child therefore cannot
  paste a token into `progress_summary`/context even if it echoes command output.
  The extension additionally redacts `apm_[0-9a-f]{32,}` from digests, prompts,
  child output and reports.
- Ordinary CLI/agent usage (not a side-run) still prints the token **once** on
  `claim` and supports `claims --with-tokens` — that is the documented local
  contract for humans and scripts. `apm` does not try to stop a model from reading
  local files; it only closes the accidental "command output lands in the ledger"
  channel described above.


### 7.6 Reading and triaging side-run reports

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
| `[apm] skipped (…)` + reason | nothing was claimed, recursion guard, or the side-run is disabled |
| `aborted run · liveness only` | the run was aborted; only a liveness sync was recorded, never progress |
| `duration · N item(s) · run …` | wall time, how many claims were in play, short run id |
| `items: …` | the item identifiers this run touched (`+N more` beyond six) |
| `✓ APM-7 sync` | a write landed for that item |
| `· APM-7 already recorded` | the item was already covered by this run (idempotent replay) |
| `✗ APM-7 failed (claim_invalid)` | the write for that item failed; the code is the CLI code |
| `! …` | errors reported by the child, redacted and truncated |
| indented plain lines | the child's summary of what it did, truncated |

Claim tokens never appear: output and errors are scrubbed with the same
`apm_[0-9a-f]{32,}` redaction the rest of the plugin uses.

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

Tests behind the card: `pi-apm/render.test.ts` (text formatting),
`pi-apm/render.snapshot.test.ts` (component structure, width behaviour, timeout
and partial-reconciliation cases, expanded snapshot, token redaction), and
`pi-apm/side-run.e2e.test.ts` (hook → child process → SQLite).

---

## 8. Programmatic access

Two optional adapters sit on top of the same ledger and the same `Store` rules:
`apm-mcp` (MCP over stdio, §8.1–§8.4) and `apm-serve` (loopback REST, §8.5).
Neither is required: the CLI works with both stopped.

### 8.1 MCP over stdio

`apm-mcp` exposes the ledger to any [Model Context Protocol](https://modelcontextprotocol.io)
client over stdio. It is a thin adapter: each tool is one `Store` call, so claims,
optimistic versions, workflow guards and idempotency behave exactly as in the CLI.
There is no daemon, no socket, and no second source of truth.

```bash
cargo build --release          # → target/release/apm and target/release/apm-mcp
apm init --name my-project     # a ledger must exist; apm-mcp never creates one
```

### 8.2 Client configuration

Copy-paste for clients that load JSON configuration:

```json
{
  "mcpServers": {
    "apm": {
      "command": "/absolute/path/to/target/release/apm-mcp",
      "args": ["--db", "/absolute/path/to/repo/.apm/apm.db", "--agent", "my-agent"]
    }
  }
}
```

Pi has first-class MCP support, so a repository can register the server without
editing JSON by hand:

```bash
# from the repository, with apm-mcp on PATH (or use absolute paths)
pi mcp add -l apm -- apm-mcp --db "$PWD/.apm/apm.db" --agent pi
pi mcp list
# apm: connected, 8 tools (codemode, project)
#   tools: apm_dashboard, apm_get, apm_claim, apm_progress, apm_status,
#          apm_context, apm_release, apm_sync
```

`-l` writes the project file `.pi/mcp.json`, which Pi only loads in a **trusted**
project (`--approve`, or `/trust` inside the project). Without trust,
`pi mcp list` says "No MCP servers configured" and ignores the file.

Flags (all also available as environment variables): `--db` (`APM_DB`),
`--project` (`APM_PROJECT`), `--agent` (`APM_AGENT`), `--session`
(`APM_SESSION_ID`, default `mcp-<uuid>`), `--run` (`APM_RUN_ID`). Without `--db`
the server walks up from its working directory looking for `.apm/apm.db`.

### 8.3 Tools

| tool | does |
|---|---|
| `apm_dashboard` | the board: waiting-human, in progress, stalled, lost lease, blocked, recently done, ready/unclaimed |
| `apm_get` | handover context, acceptance criteria, evidence, dependencies, claim |
| `apm_claim` | take the lease (token never crosses the wire) |
| `apm_progress` | record what concretely changed + `next_action` (an identical request is a `no_op`) |
| `apm_status` | workflow transition; target states come from the item's workflow, so custom states need no schema change |
| `apm_context` | `action=show` or `action=revise` (a revise merges into the current document) |
| `apm_release` | give the lease back |
| `apm_sync` | liveness + lease renewal, never the progress clock |

Writes take an optional `expect_version` (optimistic lock) and an optional
`idempotency_key`. Domain failures come back as tool results with
`isError: true` and the CLI's structured envelope (`code`, `details`, `hint`), not
as transport errors; protocol problems use JSON-RPC codes instead (`-32601`
unknown method, `-32602` unknown tool, `-32700` unparseable JSON).

Claim tokens are bearer secrets. `apm-mcp` writes the plaintext from `claim` into
the same 0600 session file the CLI uses (`<data_dir>/sessions/<session>.json`),
keeps it in process memory, and returns `token: null` / `token_suppressed: true`
in every result. Results and diagnostics are additionally scrubbed of
`apm_[0-9a-f]{32,}`.

### 8.4 Verify the connection

```bash
# 1. does the transport answer at all? (handshake + tool list, no model needed)
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | ./target/release/apm-mcp --db "$PWD/.apm/apm.db" | head -2

# 2. real-client end-to-end (official @modelcontextprotocol/sdk, 4 tests)
npx vitest run pi-apm/mcp-client.test.ts

# 3. through Pi itself
pi mcp list
```

Full details, error model and limitations: [`docs/mcp.md`](docs/mcp.md).

### 8.5 REST over loopback (optional)

`apm-serve` serves the same ledger over HTTP/1.1 on loopback, for local tools that
speak neither the CLI nor MCP. It is a foreground process — start it, stop it with
Ctrl-C — and the CLI keeps working while it is down.

```bash
apm-serve                                     # http://127.0.0.1:7717
apm-serve --listen 127.0.0.1:0 --print-token  # ephemeral port, token on stdout
```

| protection | behaviour |
|---|---|
| bind | loopback only; a non-loopback `--listen` needs `--allow-remote` + a warning |
| auth | `Authorization: Bearer <token>` on everything except `GET /health`; the token lives in `.apm/rest-token` (0600) |
| browser | any `Origin` header is refused (`403 forbidden_origin`) and `Host` must name loopback (`403 forbidden_host`) — no CORS, no DNS-rebinding hole |
| limits | 8 KiB headers, 64 KiB body, 10 s per connection, one request per connection |

Routes: `GET /health`, `GET /dashboard`, `GET /items`, `GET /items/{id}`,
`GET /claims`, `POST /items/{id}/claim|release|progress|status`. Writes take
`expect_version` and `Idempotency-Key`; domain errors keep the CLI codes with the
matching HTTP status (`409 version_conflict`, `409 claim_conflict`,
`422 workflow_violation`, `404 not_found`, `400 invalid_input`). Claim tokens are
never returned.

```bash
TOKEN=$(cat .apm/rest-token)
curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:7717/items
curl -sS -X POST -H "Authorization: Bearer $TOKEN" -d '{"summary":"did the thing"}' \
  http://127.0.0.1:7717/items/RST-1/progress
```

Client configuration, the full route/error tables, the token model and the
"why not a framework" trade-off: [`docs/rest.md`](docs/rest.md). Operating it —
single-machine boundary, hard limits, multi-instance behaviour, backup/restore,
token rotation and troubleshooting — is in [`docs/rest-ops.md`](docs/rest-ops.md).

Concurrency is enforced by the store, not by the HTTP layer, so it holds across
processes: exactly one claim winner per item, exactly one writer per
`expect_version`, and HTTP + CLI writers racing on the same item still produce one
version bump. Several `apm-serve` instances may share one database and one token
file (startup publishes the token atomically, so they all converge on the same
one).

```bash
bash scripts/stress-rest.sh 10   # 10 servers + 10 HTTP racers + mixed HTTP/CLI writers
bash scripts/stress.sh 10        # CLI-only baseline
```

---

## 9. Data model

```
projects(id, name, prefix, root_path, created_at, settings_json, workflow_json)
work_items(id, project_id, number, identifier, title, item_type,
           status, state_category, workflow_id, workflow_version, priority,
           parent_id, version, context_version, progress_summary, next_action,
           blocker, waiting_on_human, waiting_reason, completion_note,
           created_by/at, updated_at, last_heartbeat_at, last_progress_at,
           last_run_sync_at, done_at, cancelled_at)
workflows(project_id, id, version, name, definition_json, created_at, created_by)
type_workflows(project_id, item_type, workflow_id, workflow_version,
               updated_at, updated_by)
context_versions(work_item_id, version, goal, current_state, constraints_json,
                 references_json, notes, doc_hash, revises_version, reason, author,
                 actor_kind, session_id, run_id, created_at)   ← full snapshots
claims(id, work_item_id, token_hash, token_prefix, holder, holder_kind, session_id,
       run_id, status, lease_seconds, leased_at, heartbeat_at, expires_at,
       released_at, release_reason)
events(id, at, project_id, work_item_id, kind, actor, actor_kind, session_id,
       run_id, prev_version, new_version, idempotency_key, payload_json)  ← append-only
idempotency(key, work_item_id, op, fingerprint, response_json, created_at)
labels / item_labels / dependencies / acceptance_criteria / evidence
attention_requests(id, project_id, work_item_id, question, context, options_json,
                   recommendation, blocking, status, response, requested_by,
                   requested_kind, session_id, run_id, created_at, resolved_at,
                   resolved_by)
```

Event kinds: `project_initialised`, `settings_updated`, `item_created`,
`context_revised`, `item_updated`, `status_changed`, `progress_recorded`,
`progress_updated` (bookkeeping only), `run_synced`, `run_aborted`, `claimed`,
`claim_renewed`, `claim_released`,
`claim_expired`, `claim_taken_over`, `claim_superseded`, `label_added/removed`,
`dependency_added/removed`, `acceptance_added/updated`, `evidence_added`,
`attention_requested/replied/withdrawn`, `diagnostic`.

### Concurrency boundary

- `PRAGMA journal_mode=WAL`, `busy_timeout=10s`, `foreign_keys=ON`;
- every mutation runs inside `BEGIN IMMEDIATE`, so read-check-write is serialised
  across processes (CLI invocations, agents, side-runs);
- every mutating path bumps the item version in the same statement it writes
  (returned `version`, stored `version` and the event's `new_version` always
  agree); the optimistic check makes lost updates impossible — the loser gets
  `version_conflict`;
- expiry is a *conditional* update (`WHERE status='active' AND expires_at<=?`),
  so a lease renewed after a scan can never be marked expired by a stale sweep;
- the `claims_one_active` partial unique index makes double-claiming impossible
  even if application logic had a hole;
- idempotency is transactional with the write it protects: a crash cannot
  consume a key without applying the write, and cannot apply the write twice.

**What is *not* transactional:** a multi-step agent operation (progress + status +
context) is three transactions. Each is individually atomic and idempotent; a
crash between them leaves a consistent, partially-updated ledger. That is
intentional — each step is meaningful on its own, and the audit trail shows where
it stopped.

### Error model

| code | exit | meaning |
|---|---|---|
| `invalid_input` | 2 | malformed argument/value |
| `missing_context` | 2 | empty goal/current_state (or empty title → `invalid_input`) |
| `not_found` | 3 | item/project/attention id unknown |
| `version_conflict` | 4 | optimistic lock lost; re-read and retry |
| `claim_required` | 5 | agent write without a claim |
| `claim_invalid` | 5 | wrong token, expired lease, closed item |
| `claim_conflict` | 5 | another holder has a live claim |
| `workflow_violation` | 6 | illegal transition or missing `--reason` |
| `completion_evidence_required` | 6 | `done` without evidence/progress |
| `dependency_invalid` | 6 | self-dependency, cross-project, or cycle |
| `idempotency_conflict` | 2 | same key reused for a different item/operation/payload |
| `forbidden` | 7 | agent attempted a human-only action |
| `database_error` / `internal_error` | 1 | bug or broken file |

`--json` errors are `{"ok":false,"error":{"code","message","exit_code","details"}}`
on **stderr**; stdout stays machine-parseable for success paths.

---

## 10. Attention requests (waiting for a human)

```bash
apm attention request --item APM-3 \
  --question "Ship the migration before Friday?" \
  --context "Schema change is additive; rollback is a single DROP." \
  --option "ship friday" --option "ship monday" --recommend "ship friday" \
  --blocking --json

apm attention list --status open
apm attention reply atn_… --response "ship friday, I'll watch the deploy"   # human only
apm attention withdraw atn_… [--token T]   # human, requester, or claimant
```

`reply` is human-only by design: an agent may not resolve the question it asked.
`withdraw` for an item-scoped request requires a live claim on that item and a
matching session/requester identity; a human may always withdraw.

Fields: question, context, options, recommendation, `blocking`, status
(`open`/`resolved`/`withdrawn`), response, requested_by + kind, session/run,
created/resolved timestamps. Every transition writes an event. Open requests put
the item into the dashboard's `waiting_human` bucket; `work_items.waiting_on_human`
remains a coarse flag for cases where no structured question exists yet.

---

## 11. CLI reference (condensed)

```
apm init --name <n> [--prefix P] [--root DIR] [--lease-seconds N]
         [--stale-after N] [--no-progress-after N] [--done-recent N]

apm project show | list | settings [--lease-seconds …]

apm create --title T --goal G --current-state C
           [--constraint …] [--reference …] [--notes …]
           [--context-json JSON | --context-file FILE]
           [--type task|bug|story|chore|spike|question] [--priority lowest..highest]
           [--parent ID] [--depends-on ID]… [--label L]… [--acceptance TEXT]…
           [--idempotency-key K]

apm get <item>            apm list [--status S] [--all] [--label L] [--parent ID]
                                   [--held-by SESSION] [--by AGENT] [--match KW]
                                   [--attention] [--limit N]

apm update <item> --expect-version V [--token T] [--title T] [--type T]
           [--priority P] [--next-action N] [--blocker B]
           [--waiting-human|--not-waiting-human] [--waiting-reason R] [--parent ID|-]

apm claim <item> [--lease N] [--takeover] [--reason R] [--recover-only]
apm heartbeat <item> [--extend N]
apm release <item> [--reason R] | --force --reason R     (human)
apm claims [--held-by SESSION] [--include-expired] [--with-tokens]   # tokens: normal mode only

apm status <item> --to S --expect-version V [--reason R] [--note N]
apm progress <item> --summary S --expect-version V [--next-action N] [--blocker B]
             [--waiting-human] [--waiting-reason R]
apm sync <item> [--outcome completed|aborted|unknown] [--note N]

apm context show <item> [--version N]
apm context history <item> [--limit N]
apm context revise <item> --reason R --expect-version V
    (--goal G --current-state C | --context-json JSON | --context-file F)

apm label add|remove|list        apm dep add|remove|list
apm accept add|set|list          apm evidence add|list
apm attention request|list|reply|withdraw [--token T]

apm dashboard [--no-progress-after N] [--done-recent N]
apm events [item] [--limit N]
apm workflow [show|list|types|validate|set|bind|upgrade] [...]
apm expire
apm doctor
apm diag [--item ID] --code C --message M [--idempotency-key K]
```

Global flags (before or after the subcommand): `--db PATH`, `--project NAME`,
`--agent NAME`, `--session ID`, `--run ID`, `--human`, `--json`. `--now <epoch
ms|RFC3339>` is a hidden clock override used by tests and demos.

---

## 12. Testing

```bash
just verify                # everything below that runs without a model
just stress                # scripts/stress.sh 10

cargo test                 # 123 tests: 45 lib unit + 17 CLI + 32 core invariants
                           #   + 9 workflow-CLI + 4 MCP + 7 REST + 9 REST concurrency
cargo clippy --all-targets -- -D warnings
cargo fmt --check
bash scripts/stress.sh 10  # cross-process claim/write race (10 real CLI processes)
bash scripts/stress-rest.sh 10  # same race over HTTP + mixed HTTP/CLI writers

npm install
npx tsc -p tsconfig.json   # plugin type check
npx vitest run             # 51 tests: 29 plugin unit + 5 tool-schema + 4 real MCP client + 13 e2e

npm run verify             # all of the above that can run non-interactively
```

`scripts/stress.sh` spawns 10 independent `apm` processes against one database and
asserts exactly one claim winner, exactly one successful write, and the expected
number of `claim_conflict` / `version_conflict` results.

### Real Pi smoke (distinct from the fake-child tests)

The vitest e2e tests drive the real extension handlers and the real `apm` binary,
but replace the child `pi` process with a script. Separately, the extension was
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

What the Rust suite covers (see `tests/core.rs`, `tests/cli.rs`):

- creating without context fails (`missing_context`), blank title fails;
- agent writes require a live claim; humans are exempt but audited;
- forged and expired tokens are rejected; expired lease blocks writes;
- two agents cannot claim the same item; release/re-claim; human force takeover;
- context revisions keep readable history and reject no-ops and stale versions;
- generic updates bump the version, and a second write with the same
  `--expect-version` fails with `version_conflict` while the event version matches
  the stored version;
- concurrent writers cannot lose an update (`version_conflict`);
- idempotency keys are bound to operation + item + payload (cross-item,
  cross-operation and changed-payload reuse all raise `idempotency_conflict`), and
  a byte-identical retry replays without a second event;
- plaintext claim tokens never appear in the database, WAL, or a replayed
  response (a raw sqlite/bytes assertion covers this);
- an empty `update` is rejected instead of burning a version; a shared
  idempotency key across items fails with `idempotency_conflict`;
- expiry cannot clobber a lease renewed after the scan (`expire_claim` returns
  `None`, the claim stays `active`);
- `--recover-only` also rejects items the session *released* or had *superseded*
  (not just foreign/never-claimed ones);
- repeating an identical `progress` is a no-op (no version, no event, no
  `last_progress_at` move), while a bookkeeping-only change writes but still does
  not advance the progress clock;
- attention replies are human-only; item-scoped withdraw requires a live claim and
  a matching session/requester; cross-project attention is rejected;
- one run syncing two items produces exactly one event per item; retries replay
  the first response without new events;
- `sync` never moves `last_progress_at`; heartbeat keeps health `active` while the
  item still shows up as "no substantive progress";
- aborted runs record `run_aborted`, keep status/claim/progress intact, are
  idempotent, and are distinguishable in `get`/dashboard;
- completion requires **all** acceptance criteria (met/waived) or, when there are
  none, evidence — always plus progress; it auto-releases the claim, and closed
  items cannot be re-claimed;
- `--recover-only` recovers own expired lease, rejects foreign and never-claimed
  items;
- dashboard buckets, attention lifecycle, dependency cycles, diagnostics/doctor.

What the plugin suite covers (`pi-apm/lib.test.ts`, `pi-apm/side-run.e2e.test.ts`):

- idempotency keys are unique per item and operation;
- the run digest is bounded and tolerant of malformed messages;
- preflight/decision matrix including the recursion guard;
- abort plan contains no progress/status calls and degrades to a diagnostic when
  the lease is gone;
- prompt content carries versions and per-item keys (never tokens), and the hard rules;
- end-to-end (hook): the `agent_settled` hook loads the session's claims, spawns
  the child with `--mode json -p --no-session --no-extensions` and
  `APM_SIDE_RUN=1`, the child's writes reach SQLite, the retried sync does not
  duplicate events, an aborted run takes the deterministic path without spawning
  a child, and a missing binary degrades to a persisted error report instead of
  throwing;
- end-to-end (tools): `apm_claim`, `apm_progress` (including the no-op repeat),
  `apm_attention`, `apm_context` (show + revise), `apm_status` and
  `apm_dashboard` work through the real CLI; failures map to real codes
  (`claim_required`, `version_conflict`, `not_found`, `workflow_violation`) with
  recovery hints; no tool result leaks a claim token; tool writes carry
  `--session/--run/--agent`;
- end-to-end (reconciliation): a child that writes nothing is backed by a
  deterministic baseline sync, a two-item run where the child updates only one
  item syncs the other, a second settle for the same run is a no-op, and a
  configured `--apm-bin` is exposed as `apm` on the child's and the main agent's
  `PATH` — winning over a pre-existing `apm` on PATH;
- end-to-end (env lifecycle): stale `APM_RUN_ID`/`APM_SESSION_ID` are replaced by
  the current run and restored after the settle (including the disabled/skip
  path), with a fresh run id per run and no leakage.

The e2e test uses a fake `pi` child (`APM_SIDE_RUN_PI`) plus the real `apm`
binary, so no model call and no network are involved.

---

## 13. Repository layout

```
src/
  model.rs        status / health / actor / context document / JSON views
  error.rs        stable error codes and exit codes
  db.rs           schema + v1→v2 migration (idempotency binding)
  workflow.rs     declarative default state machine + guards
  dashboard.rs    dashboard buckets + doctor
  session_store.rs  0600 per-session claim-token file
  store/          items, claims, context, workflows (versioned documents,
                  bindings, upgrades), misc (labels/deps/acceptance/evidence/
                  attention), events, project, workflows
  mcp.rs          MCP stdio adapter: JSON-RPC framing + the eight tool mappings
  rest.rs         optional loopback REST adapter: HTTP/1.1 subset + routes
  main.rs         clap CLI, text rendering, JSON envelopes
  bin/apm-mcp.rs  the `apm-mcp` server binary (flags, db discovery, stdio loop)
  bin/apm-serve.rs  the optional `apm-serve` REST binary (bind rules, auth, token)
Justfile          build / install / verify / stress recipes
scripts/
  stress.sh       CLI cross-process claim/write race
  stress-rest.sh  REST variant: servers, HTTP racers, mixed HTTP/CLI writers
tests/
  core.rs         invariants (claims, versions, context, sync, completion, ...)
  cli.rs          exit codes, JSON contract, session-token reuse, guards
  mcp.rs          apm-mcp end to end: tool loop, token handling, custom states
  rest.rs         apm-serve end to end: auth, routing, errors, limits
  rest_concurrency.rs  races: HTTP claims/versions, HTTP+CLI, token bootstrap
pi-apm/
  index.ts        hooks, tools, commands (the only file that imports pi)
  lib.ts          pure logic: digest, decisions, keys, prompts, parsing, report text
  render.ts       the `apm-run-sync` entry renderer (pi-tui components)
  lib.test.ts     unit tests
  render.test.ts  entry-renderer text tests
  tools.test.ts   the tool schemas a model actually sees
  render.snapshot.test.ts  component/snapshot: structure, widths, timeout, partial failure
  mcp-client.test.ts     real MCP client (@modelcontextprotocol/sdk) → apm-mcp
  side-run.e2e.test.ts   hook → child process → SQLite, with a fake `pi`
docs/agent-protocol.md   copy-paste AGENTS.md contract for repos using apm
docs/mcp.md              MCP client guide: config, tools, errors, tokens
docs/rest.md             REST guide: routes, protections, error mapping
docs/rest-ops.md         REST operations: limits, multi-instance, backup, triage
```

## 14. Design notes and boundaries

- **Local-first, single writer file.** SQLite in WAL mode is the authority. There
  is no HTTP layer in v1; MCP/REST are additive future work because the CLI/JSON
  contract already exists.
- **Agent-native, human-friendly.** Every command has a text rendering and a
  `--json` envelope; humans get `apm dashboard`, `/apm-status`, `/apm-doctor`.
- **Claims over assignment.** No central scheduler: an agent claims work, and the
  lease makes abandonment visible instead of silent.
- **Context is a document, not a transcript.** Summarising chat into a ledger is
  lossy; requiring goal/current_state forces the handover to be written down.
- **The hook does the bookkeeping.** The main agent may forget; the run-end
  side-run exists precisely because "the agent will remember to update the ticket"
  is not a protocol.
- **Explicit non-goals (v1):** sprints/boards, time tracking, custom state
  machines, notifications/email, multi-user auth, cross-project dependencies,
  attachments, and a web UI.

MCP/REST, custom workflow states, and a TUI renderer for
`apm-run-sync` entries are the natural next steps; none of them require changing
the invariants above.
