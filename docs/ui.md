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

**Topology workspace (master/detail).** The topology view is a two-column
workspace (list column + ~55 % graph column, single column below 900 px): the
item groups scroll in their own left column, and the right column is pinned — the
graph sits on top (32 vh, so the summary stays visible), the selected-item
summary below. The columns
are sized from the measured chrome height (`--topo-top` / `--topo-bottom`), so
the page itself does not scroll in this view and picking an item near the end of
a long list never requires scrolling to find the picture. The list keeps its
scroll offset, the graph keeps its zoom/pan, and focus stays where it was across
the 4 s refresh.

**Local graph: two relation sub-views.** The product default is
**Dependencies** — work order, left-to-right, dependency edges solid + arrowed,
with scope buttons for **All**, **Blocking chain**, **Upstream** and
**Downstream** (reflected in `#scope=upstream`). **Hierarchy** is the other
sub-view: parent and children, top-down, dashed edges without arrows, and no
scope buttons (a tree has nothing to narrow). The relation lives in the URL
(`#relation=hierarchy`); the legacy *combined* picture is not offered, because a
parent and a dependent ranked together produced reversed same-column edges.
Layout comes from dagre (`ui/src/graph/layout.ts`): nodes are real `<button>`s
(`aria-label` carries identifier, title, status and health; `aria-current` marks
the selection) and the SVG edge layer is `aria-hidden`. A **Fit** control, 100 %,
zoom in/out, wheel zoom, drag-to-pan and `+ - 0 f` / arrow keys work without a
mouse. The viewport is fitted only when the picture changes (selection, relation
or scope); a refresh never re-fits under the user's hands. The **Text view**
disclosure below the graph lists the same relations in reading order
(`← blocks`, `↑ parent`, `● selected`, `↓ child`, `→ unblocks`) with *Select*
buttons that drive the same selection — that list is the keyboard and
screen-reader equivalent of the picture.

**Titles are measured in Unicode width units, not characters.** A CJK glyph is
worth two Latin letters, so node width comes from the widest *wrapped* line
(`titleUnits`, `MAX_LINE_UNITS = 26` ≈ 184 px of the 208 px minimum box).
Chinese/Japanese/Korean titles break between grapheme clusters (no spaces
needed), Latin titles still never split a word, and a truncated title ends with
`…` while the full text stays in `title=` and in the node's `aria-label`. Pasting
a long Chinese title therefore produces two readable lines instead of one
overflowing line.

**Status scope (Open / Closed / All).** The segmented control next to the view
switch decides which lifecycle half the queue shows, and is kept in the URL
(`#status=closed`) like the other view state. `Open` keeps the historical layout
(open groups plus the folded `Closed items` list); `Closed` replaces the open
groups with one accessible **`✓ Closed`** group; `All` shows both. The closed
group uses the same rows as the list — same search, priority filter and sort — so
the two views cannot drift, and each row opens the full detail drawer. Search
covers closed work directly: type `APM-19` (or a title fragment) while a closed
scope is active to reach done/cancelled/aborted/custom-closed items without
expanding anything. A refresh or a deep link keeps the chosen scope.

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
                   # workflow) + workspace geometry (own list scroll, pinned
                   # graph, no page scroll) + relation sub-views + refresh
                   # state preservation + full-screen overlay at 390 px
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

## Groups and status semantics

Every item is in **exactly one** headline group; the other dimensions stay
visible as badges, so the list is unambiguous without hiding information.

| Group | Rule (first match wins) | What it means |
|---|---|---|
| Needs human | not active/claimed, `waiting_on_human` | A human owes an answer; agents cannot progress. |
| At risk | not active/claimed, unhealthy lease (`stale`, `lost_lease`, `recovery_failed`, expired claim) or a `waiting` category with no recorded dependency or human request | The execution record needs repair. |
| In progress | category `active`/`review`, or a **valid** claim | Work is owned right now. |
| Waiting upstream | not active/claimed, at least one open blocker | Cannot start until upstream closes. |
| Startable | open, **unclaimed**, no open blockers, healthy | The only group labelled Ready to start. |
| Closed | category `closed` | Done, cancelled, aborted or a custom terminal state. |

Rules that follow from this:

- A `doing`/`review` item is **never** Startable. If it also has open blockers it
  stays in In progress and shows a `◆ blocked by N` badge.
- A claim protects an item only while it is active and unexpired; an expired
  lease moves an otherwise-ready item to At risk instead of pretending it is
  owned.
- Blockers are resolved against the loaded item snapshot: a blocker whose
  `status_category` is `closed` (including custom closed states) no longer
  blocks. References outside the snapshot (other project, deleted row) are
  ignored, matching the topology graph.
- A `waiting` category must say why. With an open dependency it is Waiting
  upstream; with a human request it is Needs human; with neither it is At risk
  ("waiting without a recorded reason").
- Group membership uses text, symbols and section headings — never colour alone.
  The reason is also exposed as the row's tooltip/accessible description.

The queue and the topology view use the same order and names: Needs human, At
risk, In progress, Waiting upstream, Startable, then the Closed fold. Closed
items are first-class (every terminal state, not just the dashboard's recent
window) and are reachable from the fold, search or a `#item=` deep link.

The classification is implemented as pure functions in `ui/src/groups.ts` and
pinned by `groups.test.ts`; `transform.ts` (queue sections) and `topology.ts`
(decision lists) consume it, so the rules cannot drift between views.

## Visual and interaction spec

The dashboard has one visual language across list, topology, local graph and the
detail drawer. Consistency comes from tokens in `ui/src/style.css`, not from
per-view spacing:

| Token group | Used for |
|---|---|
| `--space-1…6` | paddings of rows, cards, drawer sections, group gaps |
| `--radius-sm/md/lg` | buttons and badges, cards/rows/drawer, the expanded graph |
| `--font-xs/sm/md/lg` + `--line-tight/normal` | badges/labels, section titles and metadata, row titles, drawer title |
| `--control-height` | every button, scope switch and view toggle |
| `--group-gap` | distance between mutually exclusive groups |

Rules that are exercised by the browser checks:

- **Text before colour.** Every group has a heading, symbol and count; every
  badge has a symbol and a word; health is written (`health stale`), not only
  tinted. Group order is Needs human → At risk → In progress → Waiting upstream
  → Startable → Closed.
- **Keyboard.** Rows and cards are reachable with Tab and open with
  Enter/Space. The drawer and the expanded graph are native dialogs with a
  focus trap; `Escape` closes them and returns focus to the trigger. Copy
  buttons announce through `aria-live`.
- **Expanded graph.** `Expand graph` renders the same relation and scope with
  larger boxes and keeps the text view; the overlay stays in sync with the
  panel. Below 900 px the inline picture collapses and the overlay becomes the
  graph experience: it fills the viewport, scrolls inside, closes with `Escape`,
  and returns focus to the *Expand* trigger. The page never scrolls sideways.
- **Workspace.** Topology is master/detail: list column (own scroll, offset
  preserved on refresh), pinned graph column with the summary underneath.
  Selecting the last item of a long list needs no page scroll.
- **Status scope.** `Open / Closed / All` narrows the queue; closed work is a
  real group with the same rows/cards (searchable, deep-linkable via `#item=`)
  and is reflected in `#status=`, so a reload keeps it. When closed is a group,
  the old fold is hidden.
- **Freshness.** Data refreshes every 4 s; manual refresh is `R`/button; an
  outage keeps the cached rows under `offline · cached <time>` and recovers
  automatically. Filters, sort, scope and the expanded selection survive a
  refresh.
- **Narrow screens.** One column below 900 px (the topology list stops being a
  nested scroller and the page scrolls again); grid/flex children set
  `min-width: 0` so long titles wrap instead of widening the page; the graph
  dialog is full-screen below 900 px.

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
