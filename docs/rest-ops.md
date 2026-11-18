# REST adapter: operations guide

This is the run-it-in-production page for `apm-serve`. The API contract lives in
[`docs/rest.md`](rest.md); this file covers the single-machine boundary, limits,
start/stop, backup/restore, token rotation, multi-instance behaviour and
troubleshooting.

## 1. What you are running (and what you are not)

`apm-serve` is **not a service the ledger depends on**. It is an optional adapter
process over the same SQLite file the CLI writes:

* if it is down, `apm` (CLI) and `apm-mcp` keep working normally — no queue, no
  pending state, nothing to replay;
* it holds no state that is not derivable from the database (the claim-token
  cache is rebuilt from the 0600 session file on the next `claim`);
* it never starts itself, never forks into the background and never writes a pid
  file. Foreground only.

### Single-machine boundary (read this before sharing it)

| assumption | consequence |
|---|---|
| one machine, one local filesystem | SQLite in WAL mode is the concurrency primitive; `BEGIN IMMEDIATE` + `busy_timeout=10s` is what makes claims and versions safe |
| the database file is on a **local** filesystem | a network filesystem (NFS/SMB) may not honour POSIX locking reliably; do **not** treat this as a multi-machine solution |
| loopback bind | the default is `127.0.0.1:7717`; a non-loopback bind needs `--allow-remote` and is a deliberate, documented risk |
| one writer at a time per item | enforced by the claim lease + optimistic versions, not by the HTTP layer |

If you need remote access, put a reverse proxy (TLS termination, auth, rate
limits) in front and keep `--listen 127.0.0.1:...`. The adapter has no TLS on
purpose: adding a hand-rolled TLS stack to a local tool is a worse trade than a
proxy.

## 2. Start and stop

```bash
# foreground, default address
apm-serve

# explicit address + token on stdout (handy for scripts)
apm-serve --listen 127.0.0.1:9000 --print-token

# ephemeral port for tests; the "listening on" line is stable output
apm-serve --listen 127.0.0.1:0
```

```
apm-serve listening on http://127.0.0.1:9000 (loopback: true)
  auth: Authorization: Bearer <token from /repo/.apm/rest-token>
listening on 127.0.0.1:9000
```

Stop it with `Ctrl-C` (or `TERM` from your supervisor). There is no graceful
drain to wait for: each request is one transaction, so an interrupted request left
either the old state or the new one.

Under a supervisor, keep it simple and let the supervisor restart it:

```ini
# /etc/systemd/system/apm-serve.service (example)
[Service]
User=me
WorkingDirectory=/srv/repo
ExecStart=/usr/local/bin/apm-serve --db /srv/repo/.apm/apm.db --listen 127.0.0.1:7717
Restart=on-failure
# no Type=forking, no daemonize flag: it is a foreground process
```

`tmux`/`screen` is fine too — anything that keeps a foreground process alive.

## 3. Hard limits (by design, not tuning knobs)

| limit | value | why |
|---|---|---|
| request headers | 8 KiB | a header bigger than this is a bug or an attack |
| request body | 64 KiB | progress summaries are the largest legitimate payload |
| transfer encoding | `Content-Length` only | no chunked parsing surface |
| connections | one request per connection, `Connection: close` | no keep-alive state machine |
| read/write timeout | 10 s per connection (`--timeout`) | a slow client cannot pin a thread indefinitely |
| framing | HTTP/1.1 only, no TLS, no WebSocket, no CORS headers | local-only tool; a proxy terminates TLS |
| concurrency | thread per connection + SQLite `BEGIN IMMEDIATE` | no async runtime, no connection pool to tune |

When you hit a limit you get a structured error, never a silent truncation:
`413 payload_too_large`, `431 headers_too_large`, `400 bad_request` (malformed or
`Transfer-Encoding`), `405 method_not_allowed`, `404 not_found`.

## 4. Local access protection

| control | default | how to change |
|---|---|---|
| bind address | `127.0.0.1:7717` | `--listen` (non-loopback also needs `--allow-remote`) |
| bearer token | `<data_dir>/rest-token`, mode 0600, generated on first start | `--token` / `APM_REST_TOKEN`, `--token-file` |
| `Host` | must be `127.0.0.1`, `localhost` or `::1` | not configurable (DNS-rebinding guard) |
| `Origin` | any `Origin` header is refused | not configurable (browser guard) |
| `/health` | the only unauthenticated route, and it says nothing about the ledger | – |

The token is created with an **atomic publish** (`hard_link`), so several
instances starting at the same moment converge on one token instead of each
keeping its own: whichever process publishes first wins, everybody else reads the
same file, and the file always matches what the running servers hold.

```bash
# rotate: stop the servers, remove the file, start again
systemctl stop apm-serve            # or Ctrl-C
rm .apm/rest-token
apm-serve --print-token             # new token, new file (0600)
```

Rotating while servers run is not supported (they keep the token they read at
startup; restart them).

## 5. Multi-instance behaviour

Several `apm-serve` processes may share one database (different ports, same
token file, same machine). What the ledger guarantees:

* **claims**: exactly one active claim per item across processes; losers get
  `409 claim_conflict`;
* **writes**: `expect_version` makes lost updates impossible; the loser gets
  `409 version_conflict` with `details.expected`/`details.actual`;
* **idempotency**: keys are stored in the database, so a retry through a
  *different* instance still replays instead of double-writing;
* **claim tokens**: each session's tokens live in its own 0600 file
  (`sessions/<session>.json`) with a per-file lock, so two processes writing the
  same session file cannot lose each other's tokens;
* **sessions**: a token held for session A never authenticates session B's
  writes, even inside one server process (`X-APM-Session` is part of the key).

Verify it yourself:

```bash
bash scripts/stress-rest.sh 10   # 10 servers, 1 token, 10 HTTP claim racers, mixed HTTP+CLI writes
bash scripts/stress.sh 10        # CLI-only baseline
```

Expected output shape:

```
item: RSP-1   contenders: 10
servers up: 10   token length: 41
token file: one token accepted by all 10 servers
claims: 1 winner(s), 9 conflict(s)
writes: 1 success(es), 9 version_conflict(s)
final: version=2 summary=cli writer 1
PASS
```

## 6. Backup, restore, migration

The REST adapter adds nothing to storage — you back up the ledger, not the
server.

```bash
# 1. Consistent snapshot without stopping the server (includes committed WAL):
apm backup --out /var/backups/apm-$(date +%F).db

# 2. Redacted audit copy (no claims/session tokens; refuse overwrite):
apm export --out /var/backups/apm-$(date +%F).ndjson
```

`apm backup` uses SQLite `VACUUM INTO`; the destination must not exist. Treat the
snapshot like the ledger itself: it is still private state, so never commit it or
copy it to a shared ticket.

* **Restore**: stop the server, replace `.apm/apm.db` with the snapshot, delete
  stale `.apm/apm.db-wal`/`apm.db-shm`, start again. Claim tokens in
  `.apm/sessions/` belong to live leases; after a restore they are stale and will
  be rejected.
* **Schema migration**: opening the database with any newer `apm`/`apm-serve`
  runs the transactional migration on first open (`db.rs`). There is no separate
  migrate command, and a failed migration rolls back — the file stays usable by
  the previous binary.
* **Moving machines**: take an `apm backup`, copy that one file (not `.apm/` with
  `sessions/` and `rest-token`), and let the new machine mint its own credentials.
  For live cross-machine access use `apm-serve` over the network boundary instead
  of copying the database.
* **Export is redacted, not a secret scanner**: known session/run/token and
  idempotency fields are removed recursively and token-shaped strings replaced,
  but review the NDJSON before sharing it. There is no importer.
* **What not to put in git**: the active `.apm/` directory. Version the workflow
  documents and fixtures instead; there is no supported git sync of a live
  ledger.

## 7. Troubleshooting

| symptom | cause | fix |
|---|---|---|
| `cannot bind 127.0.0.1:7717` at startup | port in use by another instance | `--listen 127.0.0.1:0` (or another port); check with `ss -ltnp \| grep 7717` |
| `refusing to bind 0.0.0.0:...` (exit 2) | loopback-only default | pass `--allow-remote` **and** read §1 before doing it |
| `401 unauthorized` | missing/wrong `Authorization` header | `curl -H "Authorization: Bearer $(cat .apm/rest-token)" …` |
| `403 forbidden_origin` | the client is a browser (or sends `Origin`) | use a non-browser client; do not add CORS |
| `403 forbidden_host` | reverse proxy passes a public `Host` | proxy must rewrite `Host` to `127.0.0.1…` (and terminate TLS itself) |
| `409 claim_required` | the session has no claim (or the wrong `X-APM-Session`) | `POST /items/{id}/claim` first, or send the session that owns it |
| `409 claim_invalid` | lease expired | re-claim with `{"takeover": true, "reason": …}` |
| `409 claim_conflict` | another session holds a live lease | stop; do not force-takeover, report who holds it (`/items/{id}`) |
| `409 version_conflict` | someone else wrote first | re-read `/items/{id}`, retry once with the new `version` |
| `422 workflow_violation` | target state not in the item's workflow, or a disallowed transition | `apm workflow show --type <type>` and use a legal edge |
| `422 completion_evidence_required` | closing with open acceptance criteria | attach evidence and mark acceptance `met` first |
| `400 invalid_input` mentioning an unknown field | strict body validation | send only the documented fields (a typo must not silently drop `expect_version`) |
| `413` / `431` | body > 64 KiB / headers > 8 KiB | shrink the payload; progress summaries are not blobs |
| request hangs ~10 s then connection closes | the client sent no/partial body | send `Content-Length` and a complete body |
| `500 database_error` | disk full, locked beyond `busy_timeout`, corrupt file | check `apm doctor --json`; do not run it on a network filesystem |

Diagnostics that do not need the server:

```bash
apm doctor --json                 # db path, open items, active claims, diagnostics
apm claims --session <id> --json  # what a session still holds (no tokens printed)
apm events --limit 20 --json      # audit trail, including claim/version fights
```

## 8. When the server is down

There is nothing to fail over:

* CLI: unchanged (`apm progress …` works, the lease is still enforced).
* MCP: unchanged (`apm-mcp` is a separate process with its own session).
* REST clients: connection refused. Retry with backoff; because writes are
  idempotent per key and version-checked, a retry is always safe:

```text
POST /items/{id}/progress
Idempotency-Key: <stable key per logical write>
{"summary": "…", "expect_version": <last read>}
```

A retry after a crash either replays (same key) or fails with
`version_conflict` (somebody else won) — never a silent double write.

## 9. Tests behind these claims

```bash
cargo test --test rest               # 7 API contract tests
cargo test --test rest_concurrency   # 9 race tests: HTTP claim race, version race,
                                     # mixed HTTP+CLI writers, token bootstrap race,
                                     # session isolation, strict fields, per-session
                                     # tokens, expired lease + recovery, server-gone
                                     # degradation (CLI keeps working)
cargo test rest::                    # 10 unit tests: bind rules, host check, token
                                     # bootstrap atomicity, strict fields, status map
bash scripts/stress-rest.sh 10       # 10 servers + 10 HTTP racers + mixed writers
bash scripts/stress.sh 10            # CLI-only baseline
```
