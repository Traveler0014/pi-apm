# Declarative workflows

States, transitions and guards are **data**, not code. The default state machine
is registered like any other document, items snapshot the workflow they were
created with, and rebinding a type never reinterprets existing items.

```bash
apm workflow --json
```

## Default state machine (v1)

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

Closing an item auto-releases its claim (no zombie leases on finished work) and a
new completion note replaces the previous one, so a reopen followed by a second
`done` does not keep the first completion's text. Agents cannot claim a closed
item, so reopening is a human action:
`apm --human status <id> --to <state> --reason "..."`; the item then behaves
normally again (keeping its workflow snapshot).

## Workflow schema (v2)

States, transitions and guards are a versioned document, stored per project as
`(id, version)`:

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
  existing items (the snapshot is read inside the transition's `BEGIN IMMEDIATE`
  transaction, so the guard and the write it authorises see the same document).
  The default `apm-default` v1 workflow reproduces the original six-state machine
  exactly (same transitions and completion rules).

## CLI: configure and upgrade

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
writes nothing and adds no events.

`upgrade` plans before it writes: an open item whose state is not declared by the
target and has no `--map` blocks the whole upgrade (`workflow_violation`, exit 6).
`--migrate-items` re-snapshots open items, records a `workflow_migrated` event
carrying previous/new workflow, state and item version, and bumps the item
version. Closed items **never** migrate — they keep their historical snapshot
even when the target declares the same state (reported as `skipped_closed`);
reopen one through the human path (`apm --human status ...`) before upgrading it.

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
stored document cannot be parsed.

## Item types

The default item types are `task`, `bug`, `story`, `chore`, `spike`, `question`.
A type is bound to a workflow version (`type_workflows`); creating an item of a
bound type snapshots that document. Changing `item_type` later does **not**
migrate the workflow snapshot. New types are accepted as slugs and must be bound
(`workflow set --bind <type>`) before items of that type can be created.

Related: [`concepts.md`](concepts.md#business-status--execution-health) explains
how `category` feeds health and the dashboard.
