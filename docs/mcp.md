# MCP (Model Context Protocol) access

`apm-mcp` is a stdio MCP server over the same SQLite ledger the CLI writes. It is
a thin adapter: every tool maps to exactly one `Store` operation, so claims,
optimistic versions, workflow guards and idempotency behave the same as they do
in `apm`. There is no daemon, no socket and no second source of truth.

```
MCP client ──stdio(JSON-RPC 2.0)──▶ apm-mcp ──Store──▶ .apm/apm.db
                                       │
                                       └─ claim tokens ▶ .apm/sessions/<session>.json (0600)
```

## 1. Run it

```bash
cargo build --release            # target/release/{apm,apm-mcp,apm-serve}
just install                     # → $HOME/.local/bin/{apm,apm-mcp,apm-serve} (BINDIR=… )
# point it at a ledger (either flag or env); a database must already exist
apm-mcp --db /path/to/repo/.apm/apm.db
```

| flag | env | meaning |
|---|---|---|
| `--db <path>` | `APM_DB` | database file; default: nearest `.apm/apm.db` walking up from cwd |
| `--project <name\|id>` | `APM_PROJECT` | default project for `apm_dashboard` when the DB holds several |
| `--agent <name>` | `APM_AGENT` | actor name recorded in the audit trail (default `apm-mcp`) |
| `--session <id>` | `APM_SESSION_ID` | session id; default `mcp-<uuid>` (one process = one session) |
| `--run <id>` | `APM_RUN_ID` | optional run id recorded with writes |
| `--now <epoch ms\|RFC3339>` | – | hidden clock override for tests/demos (same seam as the CLI): lets a lease expire deterministically instead of by sleeping |

Exit codes are the CLI's: `3` when no ledger is found, `1` for internal/transport
failures. A clean EOF on stdin exits `0`.

### Client configuration

```json
{
  "mcpServers": {
    "apm": {
      "command": "apm-mcp",
      "args": ["--db", "/path/to/repo/.apm/apm.db", "--agent", "claude"]
    }
  }
}
```

The server starts in any directory when `--db`/`APM_DB` is given; otherwise it
walks up from its working directory. It never creates a database — run
`apm init --name <project>` first.

## 2. Protocol surface

* Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout (MCP stdio). One
  message per line; stdout carries protocol frames only, diagnostics go to
  stderr.
* Methods: `initialize`, `notifications/initialized`, `ping`, `tools/list`,
  `tools/call`. Anything else is `-32601`.
* `initialize` echoes the client's `protocolVersion` when it is one of
  `2024-11-05`, `2025-03-26`, `2025-06-18`, and answers with `2025-06-18`
  otherwise. Capabilities advertise `tools` only (`listChanged: false`).
* Notifications never produce a frame, including unknown ones.
* Batches are rejected with `-32600`: this transport is one message per line.

## 3. Tools

| tool | maps to | notes |
|---|---|---|
| `apm_dashboard` | `Store::dashboard` | board buckets (waiting human, in progress, stalled, lost lease, blocked, recently done, ready/unclaimed) |
| `apm_get` | `Store::get_item` | handover context, acceptance criteria, evidence, dependencies, claim |
| `apm_claim` | `Store::claim` | takes the lease; the token is never returned (see §4) |
| `apm_progress` | `Store::record_progress` | substantive changes only; an identical request is a `no_op` |
| `apm_status` | `Store::set_status` | declarative workflow; the legal target states come from the item's workflow snapshot, so custom states (e.g. `qa`) need no schema change; `done` needs acceptance/evidence |
| `apm_context` | `Store::context_at` / `revise_context` | `action=show` or `action=revise` (a revise merges into the current document) |
| `apm_release` | `Store::release` | gives the lease back; token comes from the session file |
| `apm_sync` | `Store::sync_run` | liveness + lease renewal (heartbeat); never moves the progress clock |

Every tool's `inputSchema` sets `additionalProperties: false`. Unknown arguments
are rejected rather than ignored, because a mistyped `expect_version` is a lost
race waiting to happen.

### Custom workflow states

`apm_status.to` is a plain string, **not** an enum: the default workflow offers
`ready/doing/blocked/review/done/cancelled`, and a project may register a custom
workflow (extra states such as `qa`) and bind it to an item type. Items keep the
workflow snapshot they were created with, so the server never has to guess:

* a state the item's workflow does not declare → `workflow_violation`;
* a transition the workflow does not allow → `workflow_violation`;
* every transition into a closed state keeps the completion guards
  (`completion_evidence_required` while acceptance criteria are open).

The same rule applies to the Pi extension's `apm_status` tool. Both surfaces are
covered by tests (`tests/mcp.rs`, `pi-apm/tools.test.ts`).

### Expected versions

`apm_progress`, `apm_status` and `apm_context(action=revise)` accept
`expect_version`. When it is supplied it is checked by the store and a lost race
comes back as `version_conflict` with `details.expected` and `details.actual`;
when it is omitted, the store skips the check (last write wins) — pass it. Read
the version from `apm_get`, and on a conflict re-read and retry once.

### Idempotency

`apm_claim`, `apm_progress`, `apm_status`, `apm_context` and `apm_sync` accept
`idempotency_key`. Replaying a key with the identical (operation, item, payload)
returns the original response and writes no second event; reusing it for a
different request fails with `idempotency_conflict`.

## 4. Errors: two layers

* **Protocol errors** are JSON-RPC `error` objects: `-32700` (unparseable line),
  `-32600` (bad envelope), `-32601` (unknown method), `-32602` (unknown tool or
  non-object `arguments`).
* **Domain errors** are tool results with `isError: true` and a
  `structuredContent` shaped like the CLI's `--json` envelope, plus a `hint`:

```json
{
  "content": [{ "type": "text", "text": "apm error [claim_conflict]: ...\n  hint: ..." }],
  "structuredContent": {
    "ok": false,
    "error": {
      "code": "claim_conflict",
      "message": "claim conflict: work item APM-1 is claimed by apm-worker (expires ...)",
      "exit_code": 5,
      "details": { "item": "APM-1", "holder": "apm-worker", "expires_at": "..." },
      "hint": "another agent holds a live claim: do not write; report who holds it",
      "tool_surface": "mcp"
    }
  },
  "isError": true
}
```

Codes are the documented CLI codes (`claim_required`, `claim_invalid`,
`claim_conflict`, `version_conflict`, `workflow_violation`,
`completion_evidence_required`, `idempotency_conflict`, `not_found`,
`invalid_input`, ...). Branch on `code`, never on message text.

## 5. Claim tokens never cross the wire

A claim token is a bearer secret; the database stores only `sha256(token)`, and
`Store::claim` returns the plaintext exactly once.

* `apm-mcp` hands that plaintext straight to the same 0600 session file the CLI
  uses (`<data_dir>/sessions/<session>.json`) and keeps a process-local copy so a
  failed disk write cannot strand a live lease.
* `apm_claim` results always contain `"token": null` and
  `"token_suppressed": true`, plus `token_source`
  (`session_file` | `memory_only` | `unavailable`).
* Later writes resolve the token from that file; the caller never passes one, so
  a model cannot copy a token into `summary`/context by accident.
* Every tool result is also scrubbed with the same `apm_[0-9a-f]{32,}` redaction
  the Pi extension applies, and stderr diagnostics are token-free.
* `apm-mcp` never puts a token in an environment variable. It does not create
  child processes at all.

The trust boundary is therefore the local machine: possession of
`.apm/apm.db` plus the 0600 session file is possession of the ledger. That is the
same boundary the CLI has.

## 6. Limitations

* One server process serves one session. Two processes with distinct sessions
  contend correctly (the second claim gets `claim_conflict`), but there is no
  shared in-process cache — each process keeps its own session file.
* Long tasks: `apm_sync` renews the lease (the CLI's heartbeat path) without
  touching the progress clock, so a long task stays claimable without faking
  progress. If the process dies without releasing, the lease stays live until
  `expires_at`; recover with `takeover: true` from the same session, or from
  another session after expiry.
* Only `tools` is advertised: no resources, prompts, sampling or elicitation.
* `apm_create`, labels, dependencies, acceptance-criteria mutation, evidence
  attachment, attention requests and `doctor` are CLI-only for now. Use the CLI
  for those; the MCP surface is the working loop
  (read → claim → progress → status → hand off).

## 7. Tests

Three layers, all offline:

* `src/mcp.rs` unit tests — version negotiation, framing, catalogue shape
  (`apm_status.to` has no enum), the claim-token guarantee, optimistic versions,
  workflow guards, context merge, idempotent replay, lease expiry with an
  injected clock, JSON-RPC error shapes and redaction.
* `tests/mcp.rs` — the real binary end to end: the tool loop, two sessions
  fighting over one item, the 0600 session file, a **custom workflow state**
  (`qa`) flowing through `apm_status`, and the missing-database failure path.
* `pi-apm/mcp-client.test.ts` — a **real MCP client** (the official
  [`@modelcontextprotocol/sdk`](https://github.com/modelcontextprotocol/typescript-sdk),
  a dev-dependency) driving the real binary: handshake and `tools/list`,
  the full working loop, structured error paths (claim conflict, version
  conflict, idempotent replay, lease expiry + recovery), and an assertion that
  no token-shaped string ever reaches the client.

```bash
cargo test --all-targets                               # Rust side
cargo clippy --all-targets -- -D warnings
cargo fmt --check
npx vitest run pi-apm/mcp-client.test.ts               # real MCP client
npx vitest run                                         # all plugin tests
npm run verify                                         # everything non-interactive
```

The client test builds `apm-mcp` on demand (`cargo build --bin apm-mcp`) when
`target/debug/apm-mcp` is missing, and creates each fixture ledger with the real
`apm init`/`apm create` CLI, so it exercises the same artifacts a user runs.

### Manual check with Pi as the client

```bash
pi mcp add -l apm -- apm-mcp --db "$PWD/.apm/apm.db" --agent pi
pi mcp list
# apm: connected, 8 tools (codemode, project)
#   tools: apm_dashboard, apm_get, apm_claim, apm_progress, apm_status,
#          apm_context, apm_release, apm_sync
pi -p "Use apm_dashboard and report the READY / UNCLAIMED count."
```

The project-scope file `.pi/mcp.json` is only read in a trusted project
(`--approve`, `/trust`, or an entry in `~/.pi/agent/trust.json`); that is Pi's
protection against a checkout silently wiring up executables. Verified against
`pi mcp list` on this repository's `target/debug/apm-mcp`.
