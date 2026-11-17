# `apm ui` — the embedded dashboard

`apm ui` serves a read-only dashboard from the same loopback process that hosts
the [REST adapter](rest.md). There is no second service, no daemon and no
database of its own: the page talks to the REST handlers, and every write still
goes through the CLI's rules (claims, versions, workflows).

```bash
apm ui                     # random loopback port, opens your browser
apm ui --port 7717         # fixed port
apm ui --no-open           # print the URL instead of opening a browser
apm ui --allow-remote      # NOT recommended, see docs/rest.md
```

Startup output is machine-friendly:

```text
apm ui listening on http://127.0.0.1:49152 (loopback: true, assets: dist)
ui: http://127.0.0.1:49152/ui/#ticket=apm_boot_…
listening on 127.0.0.1:49152
```

The `listening on` line never contains a secret; scripts can parse it. The `ui:`
line is the only delivery channel for the one-time ticket.

## Credentials: ticket → token

The dashboard needs the REST bearer token, but printing a long-lived token puts
it in shell history, CI logs and terminal scrollback. `apm ui` therefore mints
two process-local values:

| Value | Prefix | Lifetime | Where it goes |
|---|---|---|---|
| bootstrap ticket | `apm_boot_` | 60 s, single use | URL fragment, printed once |
| API token | `apm_ui_` | process lifetime | memory only |

The browser (or the fallback page) reads `#ticket=…`, immediately strips the
fragment with `history.replaceState`, and exchanges the ticket:

```http
POST /api/bootstrap
Content-Type: application/json

{"ticket": "apm_boot_…"}
```

```json
{"ok": true, "token": "apm_ui_…", "token_type": "Bearer"}
```

`Authorization: Bearer <ticket>` is accepted instead of the JSON body for
scripts. Failure modes are deliberately distinct so the UI can explain them:

| Status | `error.code` | Meaning |
|---|---|---|
| 401 | `invalid_ticket` | missing, unknown or mismatched ticket |
| 401 | `ticket_expired` | older than the TTL (default 60 s) |
| 409 | `ticket_consumed` | already exchanged (replay) |

After a successful exchange the ticket is burned: reloading an old URL cannot
mint another token. Restarting `apm ui` mints a new ticket *and* a new token, so
both old values stop working. Neither value is written to disk, the HTML, the
audit trail, a cookie, `localStorage` or `sessionStorage`, and the bearer token
is never printed.

The exchange endpoint only exists in `apm ui` mode (`apm-serve` returns 404) and
is subject to the same loopback `Host` and same-origin `Origin` checks as the
rest of the server. Responses are `Cache-Control: no-store`.

## Security headers

Every `/ui` response carries:

```text
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self';
  img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none';
  frame-ancestors 'none'; object-src 'none'
Cross-Origin-Resource-Policy: same-origin
Cross-Origin-Opener-Policy: same-origin
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
```

`index.html` is `no-store`; content-hashed `/ui/assets/*` files are immutable for
a year. The bundle has no inline scripts or styles and no remote resources, so
the CSP needs no `'unsafe-inline'` or `'unsafe-eval'`.

Requests with an `Origin` header must match this server exactly — scheme `http`
plus the same authority (host and port). `https://127.0.0.1:<port>` is a
different origin and is refused, as are other ports and hosts. Requests without
`Origin` (curl, scripts, same-origin `GET`) are allowed. `Host` must still name
loopback unless `--allow-remote` was passed.

Static `/ui/*` responses are unauthenticated — the page has to load before it
can exchange its ticket — but `/api/*` requires the bearer token, exactly like
the REST routes.

## Embedded assets

`build.rs` embeds `ui/dist/**` into the binary at compile time (`include_bytes!`
plus a compile-time lookup table), so running the dashboard never needs Node and
never reads the file system. `ui/dist` is committed; rebuild it with:

```bash
npm install            # once
just ui-build          # = npm run ui:build
cargo build --release  # picks up the new ui/dist automatically
```

When `ui/dist` is absent (a checkout without a frontend build), the binary falls
back to a small built-in page that still exercises the ticket → token → API
flow; the startup line reports `assets: fallback`.

Because assets are looked up by exact name in the embedded table, path
traversal (`/ui/../Cargo.toml`, `%2e%2e`) is rejected before any lookup and can
never reach the repository or the host file system.

## Refreshing, offline and stopping

The dashboard fetches the dashboard and item list every 4 seconds, which keeps
it inside a five-second freshness budget without hammering SQLite. `R` or the
**Refresh** button forces an immediate refresh; a manual refresh aborts an
in-flight automatic one so an older response can never overwrite a newer one.

When a poll fails the header shows `offline · cached <time> · last refresh
failed: <reason>` and the last good data stays on screen; filters, sort, search
and the **Recently done** fold are preserved. The next successful poll clears
the banner. Restarting `apm ui` mints a new token, so a page from the previous
process ends in a terminal "session expired — rerun `apm ui`" state and stops
polling (no 401 storm); reload the freshly printed URL to continue.

`Ctrl-C` stops the server. Nothing is left running: no pid file, no daemon, no
state outside the existing `.apm/apm.db`.

## Automated verification

```bash
just ui-check      # frontend typecheck + vitest (ui/**)
just ui-browser    # real Chromium via playwright-core, throwaway ledger:
                   # ticket exchange, first paint vs `apm dashboard --json`,
                   # dedup, filters/search/sort, drawer + events + focus + copy,
                   # 5 s auto refresh, manual refresh, offline cache + recovery,
                   # XSS escaping, 390 px overflow, storage/HTML/console hygiene
```

`just ui-browser` needs a Chromium binary; it looks at `APM_CHROME` /
`CHROME_PATH`, then Playwright's `~/.cache/ms-playwright`, then `PATH`, and
prints `SKIP` (exit 0) when none exists. The Rust side is covered by
`tests/ui.rs` (real process, raw TCP): assets, CSP, redirects, traversal,
ticket exchange/expiry/replay, restart invalidation, logs without the token,
Origin matrix and event routes.

## Routes

| Route | Auth | Notes |
|---|---|---|
| `GET /` | — | 302 to `/ui/` |
| `GET /ui` | — | 301 to `/ui/` |
| `GET /ui/` | — | `index.html`, `no-store` |
| `GET /ui/assets/<hashed>` | — | embedded, immutable cache |
| `GET /ui/<extensionless>` | — | SPA fallback to `index.html` |
| `POST /api/bootstrap` | ticket | one-time exchange |
| `GET /api/health` | — | liveness, same as `/health` |
| `GET /api/items/{id}/events?limit=20` | Bearer | recent audit entries for the detail drawer (max 200, newest window) |
| `/api/*` (rest) | Bearer | 1:1 alias of the REST routes |

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `the dashboard bootstrap endpoint requires apm ui` | hit `/api/bootstrap` on `apm-serve` |
| `unknown bootstrap ticket` | typo, or the URL came from another `apm ui` process |
| `ticket expired` | more than 60 s between start and browser load; re-run `apm ui` |
| `already exchanged` | reloaded an old URL; the ticket is single use |
| `invalid REST token` on `/api/*` | a stale tab kept an old token after a restart |
| blank page with CSP errors | the bundle gained an inline script/style or a remote URL |
| `403 forbidden_origin` | request came from another origin (including `https://`) |
| `assets: fallback` | `ui/dist` was missing at build time; run `just ui-build` |
