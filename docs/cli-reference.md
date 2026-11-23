# CLI reference

The CLI is the primary interface: every command has a text rendering for humans
and a `--json` envelope for scripts and agents. This page is the condensed
reference — command shapes, the working loop, version/idempotency rules,
dashboard/doctor output and the error model.

For a copy-paste protocol an agent can follow without reading the source, see
[`agent-protocol.md`](agent-protocol.md). For workflow documents see
[`workflows.md`](workflows.md).

## Initialise and projects

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

## The working loop

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
  --summary "rewrote claim parsing; cargo test passes (167 tests)" \
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
apm evidence add APM-3 --kind test --value "cargo test --all" --note "167 passed"
apm status APM-3 --to done --expect-version 5 --note "verified by e2e; see #123"

# 9. or hand off / stop
apm release APM-3 --reason "handing to next agent"
```

## Version discipline

Every mutating response carries the new `version`, which is exactly the value
stored in the database and recorded in the event. An update whose fields all
match the current state is rejected (`invalid_input`) instead of burning a
version; `apm progress` reports the same situation as `no_op: true`. Pass the
version as `--expect-version` on the next write. A lost race returns
`version_conflict` (exit 4) with `expected`/`actual` — re-read and retry, never
blindly overwrite.

## Idempotency

Any mutating command accepts `--idempotency-key`. A key is bound to
`(operation, item, request payload)`:

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

## Progress is not idempotent-by-accident

`apm progress` compares the request with the current state:

| request vs current | effect |
|---|---|
| summary and all provided fields identical | **no-op**: no version bump, no event, `last_progress_at` untouched (`no_op: true`) |
| same summary, different `next_action`/`blocker`/waiting fields | bookkeeping update: version bumps, event `progress_updated`, `substantive: false`, **`last_progress_at` untouched** |
| new summary | substantive: version bumps, event `progress_recorded`, `last_progress_at`/`last_run_sync_at` advance |

That asymmetry is the point: repeating yourself cannot make an item look alive.
`apm sync` is the explicit way to say "this run ended, nothing changed".

## Humans

`apm --human <command>` (name from `--agent` or `$USER`) can write without a
claim, force-release a claim (`release --force --reason`), and force a live
takeover. Those actions are audited with `actor_kind=human`. Attention replies are
human-only in v1.

## Dashboard and doctor

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

## Command reference (condensed)

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

## Error model and exit codes

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

The REST and MCP adapters keep these codes and map them to HTTP status codes /
structured tool results respectively — see [`rest.md`](rest.md) and
[`mcp.md`](mcp.md).
