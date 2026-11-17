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

## Credentials: ticket → HttpOnly session cookie

`apm ui` prints one URL and it is the only credential delivery channel:

```
ui: http://127.0.0.1:PORT/ui/#ticket=apm_boot_<uuid>    # single use, 60 s TTL
```

1. The page reads `#ticket=…`, removes it from the address bar immediately and
   posts it to `POST /api/bootstrap` (body `{"ticket": …}`; the bearer header form
   is accepted for curl).
2. On success the server answers `{"ok":true,"token_type":"cookie"}` **without a
   token** and sets:

   ```
   Set-Cookie: apm_ui_session=<process-token>; HttpOnly; SameSite=Strict; Path=/api
   ```

   No `Expires`/`Max-Age`, so it is a browser-session cookie; `Secure` is omitted
   because the dashboard is served over loopback **HTTP** (a `Secure` cookie would
   be dropped by the browser). If the UI is ever put behind TLS, add `Secure`.
3. The browser replays that cookie on every `/api/*` request, which is why a
   **page reload keeps working** without a new ticket. JavaScript never sees the
   token: it is not in the body, the URL, `localStorage`/`sessionStorage`, the DOM
   or any readable cookie.
4. The ticket stays single-use: replaying it returns `409 ticket_consumed`
   (`401 invalid_ticket` / `401 ticket_expired` for the other cases). Re-opening
   the original link while the cookie session is still live keeps the session and
   shows no error; a *reload* does not even re-post the ticket because it was
   stripped on first use.
5. The cookie value **is** the process token: it is generated per `apm ui`
   process and never persisted. Restarting `apm ui` therefore invalidates both the
   old cookie and the old ticket — a long-lived tab stops working and needs the
   freshly printed URL.
6. `apm-serve` (non-UI mode) never sets or accepts this cookie; it keeps the pure
   `Authorization: Bearer <token>` contract for CLI and scripts, which is also
   still accepted by `apm ui`.

There is no logout endpoint in the first version: stop the process (the cookie
dies with it) or clear cookies for the origin to end the session early.

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

## Visualisation: topology, stages and the local graph

The dashboard has two views, switched from the segmented control in the
controls bar; the choice is kept in the URL (`#view=topology`) exactly like the
filters and the selection.

**List view** — one row per item, with the item's own workflow stage bar, its
acceptance progress (`acceptance 1/2 done · 1 open`), queue badges (an item can
carry several: `▲ lost lease` + `● in progress` + `✋ holder`), priority, health
and last update. Rows are deduplicated by item id: an item that appears in
several dashboard buckets is one row with several badges, never two rows.

**Topology view** — the decision view:

| Group | Meaning |
|---|---|
| **Ready to start** | open items with no open blocker; each card shows `unlocks N` (how many transitive dependents it would free) |
| **Waiting on upstream** | open items with at least one open blocker; the card lists the direct blockers (`waiting on BRW-1 (doing)`) plus the transitive count and the unlock count |
| **Dependency cycles** | items that block each other; they are never "ready" and the card says to break the loop with `apm dep remove` |

Blockers are resolved from the item snapshot (`/api/items?all=true`), so closed
work stops blocking immediately, and an item inside a cycle is never counted as
"unlocked" by closing one node.

**Stage bars** come from the item's *workflow snapshot* (`workflow_id` +
`workflow_version`), fetched read-only from `GET /api/items/{id}/workflow`:

```bash
curl -H "Authorization: Bearer $(cat .apm/rest-token)" \
  http://127.0.0.1:7717/api/items/APM-15/workflow
```

* the current state is marked (`aria-current="step"`), one-step neighbours are
  dashed, and closed states are marked with ✓;
* the item's **branches** (allowed transitions) are listed with their guards
  (`needs evidence or acceptance`, `needs reason`) in the selection summary;
* a missing definition renders `◇ no workflow definition (<id>@<version>)`, and a
  state the workflow does not declare renders an explicit unknown-stage warning —
  the UI never guesses the default six states.

**Local dependency graph** (below the groups, for the selected item): only the
neighbourhood is drawn (two hops), with dependency edges solid + arrowed and
parent/child edges dashed without an arrow, plus a legend. Scope buttons switch
between **All**, **Blocking chain**, **Upstream** and **Downstream**; the scope
is reflected in the URL (`#scope=upstream`). Every node is a real `<button>`
(`aria-label` carries identifier, title, status and health; `aria-current` marks
the selection), and the **Text view** disclosure below the graph lists the same
relations in reading order (`← blocks`, `↑ parent`, `● selected`,
`↓ child`, `→ unblocks`) with *Select* buttons that drive the same selection —
that list is the keyboard and screen-reader equivalent of the picture.

**Closed items are first-class.** The `Closed items` fold contains every closed
category — `done`, `cancelled`, `aborted` and any custom closed state — not just
the dashboard's "recently done" window. Any of them opens the full detail drawer
from the fold, from a search hit, or from a deep link (`#item=APM-19`, also while
the page is already open). Deep links never carry the ticket: it is stripped on
load and the remaining fragment keys are kept.

## Automated verification

```bash
just ui-check      # frontend typecheck + vitest (ui/**)
just ui-browser    # real Chromium via playwright-core, throwaway ledger:
                   # ticket exchange, first paint vs `apm dashboard --json`,
                   # dedup, filters/search/sort, drawer + events + focus + copy,
                   # topology groups + blockers/unlocks + stage bars (custom
                   # workflow) + local graph (both edge kinds, scopes, text view)
                   # + closed-category drawer, 5 s auto refresh, manual refresh,
                   # offline cache + recovery, XSS escaping, 390 px overflow,
                   # storage/HTML/CSP/console hygiene
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
