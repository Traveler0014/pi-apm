# AgentPM agent protocol (copy-paste contract)

Use this file as `AGENTS.md` (or a section of it) in a repository that uses
`apm`. It is the short version of `README.md` for an agent that has just been
started and has no memory of previous runs.

## Before you touch anything

1. `apm dashboard --json` — read the board. Prefer `ready_unclaimed` work,
   `waiting_human` items you can unblock, and `lost_lease` items you are allowed
   to recover.
2. `apm get <id> --json` — read the **context** (`goal`, `current_state`,
   `constraints`, `references`), acceptance criteria, dependencies, and the
   current claim. The context is written for someone with no chat history: trust
   it, and fix it if it is wrong.
3. `apm claim <id> --json` — take the lease, then `apm status <id> --to doing`.
   If `claim_conflict` comes back, do **not** write: report who holds it.

## While working

```bash
apm heartbeat <id> --extend 900          # long task, no progress yet
apm progress <id> --expect-version V \
  --summary "what concretely changed" \
  --next-action "the next concrete step" [--blocker "..."]
```

* One progress write per meaningful unit of work. Repeating the same summary is
  rejected as a no-op — use `apm sync` for liveness.
* Never pass `--expect-version` you have not just read; on `version_conflict`,
  re-read and retry once.
* Context changed the plan? `apm context revise <id> --reason "why" ...`.
* Need a decision? `apm attention request --item <id> --question Q --context C`.
  Humans answer; you cannot.

## Finishing

* Evidence first: `apm evidence add <id> --kind test|commit|link|artifact ...`
* Acceptance: `apm accept set <id> --seq N --status met|waived`
* `apm status <id> --to done --note "verification"` — requires **all** acceptance
  criteria met/waived (or evidence if there are none) plus a progress summary.
* Or hand off: `apm context revise` with the current state, then
  `apm release <id> --reason "handoff"`.
* Or park it: `apm status <id> --to blocked --reason "..."` plus `--blocker`.

## Rules that are enforced, not conventions

* Every write needs a live claim: pass `--session` and the CLI reuses the 0600
  session token file (never paste a token into a field).
* Expired leases reject writes, even with the right token.
* Versions are optimistic: lost races fail loudly.
* `done` cannot be a bare status flip.
* Idempotency keys are bound to (operation, item, payload); reusing one for
  something else is an error, not a silent replay.
* A run-end side-run may update the items **this session claimed**; it never
  claims new work.

## What the run-end side-run does

After each run settles, the Pi extension starts a short-lived child agent with the
run digest and this session's claims. It writes only what the run verifiably
changed (progress, context, status, evidence, attention) using per-item
idempotency keys, then exits. It is not a substitute for your own reporting: if
you finish something important, record it yourself — the side-run is the safety
net, not the protocol.
