# Optional loopback REST access

`apm-serve` serves the ledger over HTTP/1.1 on loopback for local tools that
speak HTTP but not MCP or the CLI. It is **optional and explicit**: you start it,
you stop it, and while it is down the CLI and `apm-mcp` keep working, because
SQLite is still the only source of truth.

```
local tool ──HTTP──▶ apm-serve ──Store──▶ .apm/apm.db
                        │
                        └─ claim tokens ▶ .apm/sessions/<session>.json (0600)
                          REST bearer token ▶ .apm/rest-token (0600)
```

The adapter is thin by construction: every route is one `Store` call, so claims,
optimistic versions, workflow guards and idempotency behave exactly as in the
CLI. Nothing in this layer re-implements a state machine or a permission rule.

## 1. Start and stop

```bash
cargo build --release            # target/release/{apm,apm-mcp,apm-serve}
just install                     # → $HOME/.local/bin/{apm,apm-mcp,apm-serve}

cd ~/code/my-repo
apm-serve                        # foreground, http://127.0.0.1:7717
apm-serve --listen 127.0.0.1:9000 --print-token
```

Startup output (real):

```
apm-serve listening on http://127.0.0.1:7788 (loopback: true)
  auth: Authorization: Bearer <token from /tmp/rest-smoke/.apm/rest-token>
listening on 127.0.0.1:7788
```

* **Stop** with `Ctrl-C` (or your supervisor's signal). There is no pid file, no
  daemonization and no auto-start. Each request is one `BEGIN IMMEDIATE`
  transaction, so a killed process leaves either the old state or the new state,
  never half a write.
* `--listen 127.0.0.1:0` binds an ephemeral port and prints the real address
  (`listening on 127.0.0.1:PORT`) — that line is stable and script-friendly.
* Binding a non-loopback address requires `--allow-remote` and prints a warning.

| flag | env | meaning |
|---|---|---|
| `--db <path>` | `APM_DB` | database file; default: nearest `.apm/apm.db` walking up from cwd |
| `--project <name\|id>` | `APM_PROJECT` | default project when the DB holds several |
| `--agent <name>` | `APM_AGENT` | actor recorded in the audit trail (default `apm-rest`) |
| `--listen <addr>` | `APM_REST_LISTEN` | bind address (default `127.0.0.1:7717`) |
| `--allow-remote` | – | required to bind a non-loopback address |
| `--session <id>` | `APM_SESSION_ID` | default session when the client sends no `X-APM-Session` |
| `--run <id>` | `APM_RUN_ID` | run id recorded with writes |
| `--token <value>` | `APM_REST_TOKEN` | bearer token (default: read/create `<data_dir>/rest-token`) |
| `--token-file <path>` | – | where the generated token lives |
| `--print-token` | – | print the bearer token on startup |
| `--timeout <secs>` | – | per-connection read/write timeout (default 10) |
| `--now <epoch ms\|RFC3339>` | – | hidden clock override for tests/demos (same seam as the CLI): lets a lease expire deterministically |

## 2. Local access protection

The ledger is local-first, so the HTTP surface assumes "the caller can read the
SQLite file anyway" — and makes it hard for anything *else* to drive it:

| protection | behaviour |
|---|---|
| loopback bind | `--listen` must resolve to a loopback address unless `--allow-remote` is passed; the default is `127.0.0.1:7717` |
| bearer token | every route except `GET /health` needs `Authorization: Bearer <token>`; the token is generated at first start into `<data_dir>/rest-token` with mode `0600` and compared in constant time |
| `Host` check | with `--allow-remote` off (the default) `Host` must be `127.0.0.1`, `localhost` or `::1` (any case, IPv6 bracketed or bare) — otherwise `403 forbidden_host`; a missing or empty `Host` is `400 bad_request` in both modes because HTTP/1.1 requires it. This is what stops DNS-rebinding |
| `Origin` check | any request carrying an `Origin` header is refused (`403 forbidden_origin`), so a web page in your browser cannot POST to the ledger |
| body limits | 8 KiB of headers, 64 KiB body, read through a bounded socket reader (an endless header line is cut off at the cap instead of buffered); `Transfer-Encoding` is rejected, `Content-Length` is required for bodies |
| strict bodies | unknown JSON fields are rejected (`400 invalid_input` listing the allowed keys), so a typo like `expect_versoin` cannot silently turn a locked write into an unlocked one |
| timeouts | 10 s read/write per connection, one request per connection (`Connection: close`) |
| no CORS | no `Access-Control-Allow-*` headers are ever sent |

`GET /health` is the only unauthenticated route and it reveals nothing about the
ledger:

```bash
curl -sS http://127.0.0.1:7788/health
{"ok": true, "service": "apm-rest", "version": "0.1.0"}
```

## 3. Routes

All request bodies are JSON objects; responses are JSON with `"ok": true` plus
the resource, shaped exactly like the CLI's `--json` output. Body fields are a
closed set per route: anything else is a `400 invalid_input` naming the allowed
fields (a mistyped `expect_version` must not be ignored).

| method | path | body / query | maps to |
|---|---|---|---|
| `GET` | `/health` | – | liveness only (no auth) |
| `GET` | `/dashboard` | `project`, `no_progress_after_seconds`, `done_recent_seconds` | `Store::dashboard` |
| `GET` | `/items` | `status` (comma list), `all`, `label`, `parent`, `session`, `agent`, `keywords`, `attention`, `limit`, `project` | `Store::list_items` |
| `GET` | `/items/{id}` | – | `Store::get_item` (context, acceptance, evidence, dependencies, claim) |
| `GET` | `/claims` | `session`, `include_expired` | `Store::session_claims` |
| `POST` | `/items/{id}/claim` | `lease_seconds`, `takeover`, `reason`, `idempotency_key` | `Store::claim` |
| `POST` | `/items/{id}/release` | `reason` | `Store::release` |
| `POST` | `/items/{id}/progress` | `summary` (required), `next_action`, `blocker`, `waiting_on_human`, `waiting_reason`, `expect_version`, `idempotency_key` | `Store::record_progress` |
| `POST` | `/items/{id}/status` | `to` (required), `reason`, `note`, `expect_version`, `idempotency_key` | `Store::set_status` |

`{id}` accepts an identifier (`RST-1`), a uuid or a number, exactly like the CLI.

### Walkthrough (real output)

```bash
TOKEN=$(cat .apm/rest-token)
AUTH="Authorization: Bearer $TOKEN"

curl -sS -H "$AUTH" http://127.0.0.1:7788/items
# {"count":1,"items":[{"identifier":"RSM-1","status":"ready","health":"unclaimed",...}]}

curl -sS -X POST -H "$AUTH" -d '{"lease_seconds":120}' \
  http://127.0.0.1:7788/items/RSM-1/claim
# {"ok":true,"claim":{"identifier":"RSM-1","status":"active","expires_at":"...","lease_seconds":120,...},
#  "token":null,"token_suppressed":true,"token_source":"session_file","item_version":1}

VERSION=$(curl -sS -H "$AUTH" http://127.0.0.1:7788/items/RSM-1 \
  | python3 -c 'import json,sys;print(json.load(sys.stdin)["item"]["item"]["version"])')

curl -sS -X POST -H "$AUTH" -d "{\"to\":\"doing\",\"expect_version\":$VERSION}" \
  http://127.0.0.1:7788/items/RSM-1/status
# {"ok":true,"item":{"identifier":"RSM-1","status":"doing","version":2,...}}

curl -sS -X POST -H "$AUTH" -d '{"summary":"REST adapter drove the loop","next_action":"document it"}' \
  http://127.0.0.1:7788/items/RSM-1/progress
# {"ok":true,"no_op":false,"substantive":true,"changed_fields":["progress_summary","next_action"],"item":{...}}

curl -sS -X POST -H "$AUTH" -d '{"summary":"raced","expect_version":1}' \
  http://127.0.0.1:7788/items/RSM-1/progress
# HTTP 409 {"ok":false,"error":{"code":"version_conflict","details":{"expected":1,"actual":2},...}}
```

`item.status` for the item resource is the item's workflow state; custom states
work exactly as in the CLI and MCP (`{"to": "qa"}` is accepted when the item's
workflow declares it, `422 workflow_violation` otherwise).

### Idempotency

`Idempotency-Key: <key>` on the request, or `"idempotency_key": "<key>"` in the
body. The key is bound to (operation, item, payload) by the store: a retry with
the same payload replays the original response and writes no second event, while
reusing the key for a different payload fails with `idempotency_conflict`.

## 4. Error model

Domain errors keep the CLI's `code` and add the transport status:

| HTTP | codes |
|---|---|
| `400` | `invalid_input`, `missing_context`, `idempotency_conflict` |
| `401` | `unauthorized` (missing/wrong bearer token — transport-level, not a ledger code) |
| `403` | `forbidden`, `forbidden_origin`, `forbidden_host` |
| `404` | `not_found` (unknown item or unknown route) |
| `405` | `method_not_allowed` |
| `409` | `version_conflict`, `claim_required`, `claim_invalid`, `claim_conflict` |
| `413` | `payload_too_large` |
| `422` | `workflow_violation`, `completion_evidence_required`, `dependency_invalid` |
| `431` | `headers_too_large` |
| `500` | `database_error`, `serialization_error`, `internal_error` |

The envelope is always the CLI's:

```json
{
  "ok": false,
  "error": {
    "code": "claim_conflict",
    "message": "claim conflict: work item RST-1 is claimed by agent (expires ...)",
    "exit_code": 5,
    "details": { "item": "RST-1", "holder": "agent", "expires_at": "..." }
  }
}
```

Branch on `code`, not on the message or the status: the status is a hint, the
code is the contract.

## 5. Claim tokens never cross the wire

Identical to the MCP adapter: `POST /items/{id}/claim` hands the plaintext token
straight to the 0600 session file (and process memory) and answers with
`"token": null`, `"token_suppressed": true` and `"token_source":
"session_file" | "memory_only" | "unavailable"`. Later writes resolve the token
server-side, so a caller never handles one. `GET /claims` never returns a token
map.

The **REST bearer token** is a different secret: it authenticates the client to
the adapter. It is generated at startup, stored at `<data_dir>/rest-token` with
mode `0600`, and printed only with `--print-token`. Bootstrapping is atomic: the
winner publishes the file with `hard_link` (no clobbering `rename`), so several
instances starting together all end up holding the one token that is on disk.

## 6. Concurrency and shutdown (summary)

* One `Store` behind a mutex plus SQLite `BEGIN IMMEDIATE` means concurrent
  requests are serialised the same way concurrent CLI processes are: exactly one
  claim wins, the loser gets `409 claim_conflict`, and a stale write gets
  `409 version_conflict` instead of overwriting.
* Thread per connection, capped at 128 concurrent connections (further clients get
  `503 server_busy` immediately instead of an unbounded thread and buffer
  budget), `Connection: close`, bounded bodies: a slow or hostile client cannot
  hold the server hostage beyond the 10 s timeout.
* Stopping mid-request is safe: either the transaction committed or it did not.
  There is no in-memory state worth flushing beyond the claim-token cache, which
  is rebuilt from the session file on the next `claim` and is pruned (TTL + hard
  cap) so a long-running server cannot grow it forever.

Operational depth — single-machine boundary, limits rationale, multi-instance
behaviour, backup/restore/migration, token rotation and a troubleshooting table —
lives in [`docs/rest-ops.md`](rest-ops.md). This page covers the contract you code
against.

What the adapter guarantees when several clients and CLI processes race:

* exactly one claim winner per item (`409 claim_conflict` for the rest);
* exactly one writer per `expect_version` (`409 version_conflict` otherwise);
* HTTP and CLI writers mixed → still one winner and one version bump;
* sessions never borrow each other's tokens (the cache is keyed by
  `(session, item)`, and the session file is `TokenStore`-locked);
* several `apm-serve` instances converge on one bearer token.

## 7. Why no web framework

The transport is a small HTTP/1.1 subset: one request per connection,
`Content-Length` bodies, no chunked encoding, no keep-alive, no TLS, no
WebSockets, no CORS. That is deliberate:

* the adapter's only job is "parse a small request, call `Store`, serialise JSON"
  on loopback, and the ledger must keep its "no daemon, no network dependency"
  property;
* `axum`/`hyper` would add an async runtime and a large dependency tree to a
  binary that never needs concurrency beyond thread-per-request;
* the whole parser is ~200 lines with hard caps and no cleverness.

If this surface ever grows beyond local tooling, replace `src/rest.rs`'s transport
with a real server **and keep the handler functions**: they are already the thin
translation the product promises. Do not add TLS or auth schemes here — put a
reverse proxy in front instead.

## 8. Tests

```bash
cargo test --test rest               # 11 end-to-end tests against the real binary
cargo test --test rest_concurrency   # 9 race tests (claims, versions, HTTP+CLI, token bootstrap,
                                     # sessions, expired lease, strict fields, server-gone CLI)
cargo test rest::                    # 17 unit tests (bind rules, host/origin guards, bounded reads,
                                     # cookie matching, token-cache pruning, strict fields)
bash scripts/stress-rest.sh 10       # multi-process: 10 servers, 10 HTTP racers, mixed writers
npx vitest run                       # unrelated: plugin + MCP client tests
```

`tests/rest.rs` spawns `apm-serve` on an ephemeral loopback port and drives it
with a real TCP client: auth (401/403), the full item loop (claim → status →
progress → release), `claim_conflict`, `version_conflict`, idempotent replay,
`workflow_violation` (422), malformed input (400), unknown route (404), wrong
method (405), body/header limits, the 0600 token file, and the
`--allow-remote`/missing-database failure paths.
