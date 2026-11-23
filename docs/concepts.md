# Concepts and data model

What the ledger *means*: the difference between status and health, the clocks
that drive the dashboard, the claim rules every write depends on, the data model,
and what is (and is not) committed to git.

## Business status ≠ execution health

| Concept | Values | Who sets it |
|---|---|---|
| **status** (workflow) | `ready` `doing` `blocked` `review` `done` `cancelled` | agent/human via `apm status` |
| **health** (derived) | `active` `stale` `expired` `released` `superseded` `unclaimed` `ended` | computed from claims + clock |
| **progress** | free text + `last_progress_at` | only `apm progress` |
| **attention** | open/resolved requests | agents ask, humans answer |

Health is never writable. A heartbeat can keep an item `active` forever without
that meaning anything changed — which is exactly why `last_progress_at` is a
separate clock. Custom workflow states are legal (`qa`, `shipping`, …); their
*category* (`open`/`active`/`waiting`/`review`/`closed`) is what health, the
dashboard and claims actually use.

## Three clocks

| Field | Moved by | Never moved by |
|---|---|---|
| `last_heartbeat_at` | `apm claim`, `apm heartbeat`, `apm sync` | progress/status writes |
| `last_progress_at` | `apm progress` only | heartbeat, run sync, abort |
| `last_run_sync_at` | `apm progress`, `apm sync` | reading, listing |

`apm sync` exists so an agent can say *"this run ended"* without pretending it
*"made progress"*. The dashboard reports all three per item.

## Claims

A claim is a lease with a bearer token:

- **atomic**: one `active` claim per item, enforced by a partial unique index and
  `BEGIN IMMEDIATE`; concurrent claimers get `claim_conflict` (exit 5);
- **expiring**: default lease 900s (`--lease`, min 30s); after expiry every write
  is rejected with `claim_invalid` even if the token is right;
- **token-verified**: the database stores only `sha256(token)`; the plaintext is
  returned once at claim time and optionally persisted to
  `.apm/sessions/<session>.json` (mode 0600) for the agent session that owns it;
- **renewable**: `apm heartbeat` extends the lease without touching progress. Run
  sync (`apm sync`) also renews the lease, using the lease the claim was created
  with — a sync never shortens a custom lease to the project default;
- **releasable**: `apm release` requires the same token that claim returned;
- **expired work is free to claim**: the next claim supersedes the expired one and
  is recorded as `claim_taken_over`. `--takeover` is needed to take a *live*
  claim, and that is human-only (`apm --human claim <id> --takeover --reason …`);
- **recoverable only for its own latest claim**: a side-run's `--recover-only`
  succeeds only when the item's **globally latest** claim belongs to the same
  session and is `active` or `expired`. If another session took the item over —
  even if that session later released it — the old session can no longer recover
  from its stale claim. Recovery never becomes a shortcut to new work;
- **session-safe**: a session that lost its token file can re-claim its own live
  lease (it supersedes itself), never somebody else's.

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

## Context is versioned handover, not chat history

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

## Attention requests (waiting for a human)

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

Fields: question, context, options, recommendation, `blocking`, status
(`open`/`resolved`/`withdrawn`), response, requested_by + kind, session/run,
created/resolved timestamps. Every transition writes an event. Open requests put
the item into the dashboard's `waiting_human` bucket; `work_items.waiting_on_human`
remains a coarse flag for cases where no structured question exists yet.

## Data model

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

Schema versioning: the database carries `PRAGMA user_version` (currently 3). The
migration runs inside one `IMMEDIATE` transaction (DDL + data + version stamp);
a database already at the current version skips the DDL entirely, and a newer
database is never downgraded. Legacy `projects.workflow_json` documents are
imported into the versioned workflow table by the v2→v3 migration.

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

## The `.apm/` directory and git

**The active ledger is never tracked.** Add the whole runtime directory to
`.gitignore`:

```gitignore
# Database, WAL/SHM, session tokens and the REST token all live here.
# There is no supported "commit .apm/apm.db" workflow: WAL/SHM are invisible to
# git, and the token files are bearer credentials.
.apm/
```

What belongs in git is the *schema and configuration*, not the live database:
workflow documents, fixtures, `AGENTS.md` and scripts. `.apm/sessions/` and
`rest-token` hold 0600 credentials; never copy them into an artifact, a ticket or
a prompt. To share a ledger across machines use `apm-serve` (loopback + bearer
token, see [`rest.md`](rest.md)), not git sync.

## Backup and redacted export

```bash
apm backup --out apm-backup-2026-11-18.db   # WAL-consistent snapshot, refuses overwrite
apm export --out audit.ndjson               # redacted NDJSON, refuses overwrite (0600)
apm export | jq -c .                        # or stream it to stdout
```

- **Backup** uses SQLite `VACUUM INTO`, so committed WAL frames are included and
  the result is one self-contained file. It never overwrites an existing path.
  Restore with every writer stopped: replace `.apm/apm.db`, delete stale
  `apm.db-wal`/`apm.db-shm`, restart. Claim tokens from the old snapshot are
  stale credentials and will be rejected.
- **Export** is NDJSON with `schema_version: 1`: a `meta` line (project +
  timestamp), then `item`, `context` (latest version), `acceptance`, `evidence`,
  `dependency` and `event` records, all read inside one SQLite read transaction
  so a concurrent write cannot make an item and its history disagree. Claims,
  sessions, run ids and token files are excluded by construction; known
  session/run/token/idempotency keys are removed recursively (including nested
  event payloads), and every APM secret shape (`apm_[A-Za-z0-9_-]{32,}`) is
  redacted defensively. That replacement is **not** a general secret scanner:
  review an export before publishing it. It is for audit and archival — there is
  no importer and it is not a sync format. A file target is created exclusively
  with mode 0600 and is never overwritten.

Error codes and exit codes are documented in
[`cli-reference.md`](cli-reference.md#error-model-and-exit-codes); workflow states
and upgrade semantics in [`workflows.md`](workflows.md).
