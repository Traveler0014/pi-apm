/**
 * Pure view-model logic for the dashboard.
 *
 * Everything here is a function of `(dashboard, ui state, clock)` — no DOM, no
 * network — which is what makes the dedupe/filter/sort/staleness behaviour
 * testable without a browser.
 */

import { GROUPS, groupItems, groupMeta, indexItems, type GroupKey, type GroupMeta } from "./groups";
import type { BucketKey, Dashboard, WorkItem } from "./types";

export interface BucketMeta {
	key: BucketKey;
	/** Text label, so status is never expressed by colour alone. */
	label: string;
	/** One-character marker for the same reason. */
	symbol: string;
	tone: "danger" | "warn" | "info" | "ok" | "muted";
}

/** Order defines both the badge order and the default queue ranking. */
export const BUCKETS: BucketMeta[] = [
	{ key: "lost_lease", label: "lost lease", symbol: "▲", tone: "danger" },
	{ key: "recovery_failed", label: "recovery failed", symbol: "✖", tone: "danger" },
	{ key: "waiting_human", label: "waiting human", symbol: "⏸", tone: "warn" },
	{ key: "stalled_no_progress", label: "no progress", symbol: "…", tone: "warn" },
	{ key: "waiting_dependencies", label: "blocked deps", symbol: "◆", tone: "warn" },
	{ key: "in_progress", label: "in progress", symbol: "●", tone: "info" },
	{ key: "ready_unclaimed", label: "ready", symbol: "○", tone: "ok" },
	{ key: "recently_done", label: "done", symbol: "✓", tone: "muted" },
];

const BUCKET_BY_KEY = new Map(BUCKETS.map((bucket) => [bucket.key, bucket]));
const ACTIVE_BUCKETS = BUCKETS.filter((bucket) => bucket.key !== "recently_done");

export function bucketMeta(key: BucketKey): BucketMeta {
	return BUCKET_BY_KEY.get(key) ?? { key, label: key, symbol: "?", tone: "muted" };
}

export type SortKey = "queue" | "priority" | "updated" | "identifier" | "progress";

/** Which lifecycle half the queue shows. `all` = open groups + the closed group. */
export type StatusScope = "open" | "closed" | "all";

export interface ViewState {
	search: string;
	/** Empty means "every bucket". */
	buckets: BucketKey[];
	priorities: string[];
	sort: SortKey;
	showRecentlyDone: boolean;
	/**
	 * Explicit lifecycle filter (APM-30). `open` keeps the historical behaviour
	 * (open groups + a folded closed list); `closed`/`all` promote the closed
	 * items into a real group so done/cancelled/aborted work is searchable and
	 * filterable exactly like open work.
	 */
	statusScope: StatusScope;
}

export function defaultViewState(): ViewState {
	return { search: "", buckets: [], priorities: [], sort: "queue", showRecentlyDone: false, statusScope: "open" };
}

const PRIORITY_RANK: Record<string, number> = {
	highest: 0,
	high: 1,
	normal: 2,
	low: 3,
	lowest: 4,
};

export function priorityRank(priority: string): number {
	return PRIORITY_RANK[priority] ?? 9;
}

export interface QueuedItem {
	item: WorkItem;
	badges: BucketMeta[];
	/** Mutually exclusive headline group (see groups.ts for the precedence). */
	group: GroupKey;
	/** Why this item landed in that group; used as the row's explanation. */
	explanation: string;
	/** Blockers that are still open, resolved against the loaded snapshot. */
	openBlockerCount: number;
}

export interface QueueSection {
	meta: GroupMeta;
	rows: QueuedItem[];
}

export interface QueueModel {
	/** Open items (any non-closed category), filtered and sorted. */
	items: QueuedItem[];
	/** The same rows partitioned into mutually exclusive display groups. */
	groups: QueueSection[];
	/** How many open items exist before filtering. */
	total: number;
	/** How many open items the current filters hide. */
	filtered: number;
	/**
	 * Every closed item — done, cancelled, aborted and any custom closed state —
	 * not just the dashboard's `recently_done` bucket (which deliberately excludes
	 * cancelled work). This is what makes every terminal item reachable.
	 */
	closed: QueuedItem[];
	closedTotal: number;
	/**
	 * True when closed items are rendered as a real group (status scope includes
	 * them) instead of the folded list.
	 */
	showClosedGroup: boolean;
}

function isClosed(item: Pick<WorkItem, "status_category">): boolean {
	return item.status_category === "closed";
}

/**
 * Bucket membership per item id (an item can be in several), so a row keeps the
 * dashboard's health semantics (lost lease, stalled, waiting human, blocked…).
 */
export function bucketMembership(dashboard: Dashboard | null): Map<string, BucketKey[]> {
	const membership = new Map<string, BucketKey[]>();
	if (!dashboard) return membership;
	for (const bucket of BUCKETS) {
		for (const item of dashboard.buckets[bucket.key] ?? []) {
			const keys = membership.get(item.id) ?? [];
			if (!keys.includes(bucket.key)) keys.push(bucket.key);
			membership.set(item.id, keys);
		}
	}
	return membership;
}

/** Fallback badge when the dashboard has no bucket for an item (custom states). */
function derivedBadge(item: WorkItem): BucketMeta {
	const byCategory: Record<string, BucketKey> = {
		open: "ready_unclaimed",
		active: "in_progress",
		waiting: "waiting_dependencies",
		review: "in_progress",
	};
	return bucketMeta(byCategory[item.status_category] ?? "ready_unclaimed");
}

function badgesFor(item: WorkItem, keys: BucketKey[] | undefined): BucketMeta[] {
	const ordered = BUCKETS.filter((bucket) => keys?.includes(bucket.key));
	if (ordered.length > 0) return ordered;
	return [derivedBadge(item)];
}

/**
 * Flatten the current snapshot into the dashboard queue.
 *
 * The item list (`/api/items?all=true`) is the source of truth — the dashboard
 * buckets only annotate it — because buckets are *views*: an item in a custom
 * state may appear in none, and `recently_done` excludes cancelled work by
 * design. Building from the snapshot guarantees every item is reachable.
 */
export function buildQueue(
	dashboard: Dashboard | null,
	items: WorkItem[] | null,
	state: ViewState,
	now: number = Date.now(),
): QueueModel {
	const all = items ?? [];
	const openItems = all.filter((item) => !isClosed(item));
	const closedItems = all.filter(isClosed);
	const membership = bucketMembership(dashboard);
	const grouped = groupItems(all, now, indexItems(all));

	const rowFor = (item: WorkItem, badges: BucketMeta[]): QueuedItem => {
		const classification = grouped.classification.get(item.id);
		return {
			item,
			badges,
			group: classification?.group ?? (isClosed(item) ? "closed" : "startable"),
			explanation: classification?.explanation ?? "",
			openBlockerCount: classification?.openBlockers.length ?? 0,
		};
	};

	const scope = state.statusScope;
	const openRows: QueuedItem[] = openItems.map((item) =>
		rowFor(item, badgesFor(item, membership.get(item.id))),
	);
	const queued = applyFilters(openRows, state);
	const showOpen = scope !== "closed";
	const sections: QueueSection[] = showOpen
		? GROUPS.filter((group) => group.key !== "closed")
				.map((meta) => ({
					meta,
					rows: sortQueue(
						queued.filter((row) => row.group === meta.key),
						state.sort,
					),
				}))
				.filter((section) => section.rows.length > 0)
		: [];

	// Closed rows share the search box and the priority filter (bucket chips
	// describe the open queue, so they are ignored here). They become a real group
	// whenever the status scope includes closed work, which keeps List and Topology
	// reading the same rows.
	const closedRows: QueuedItem[] = closedItems.map((item) =>
		rowFor(item, [{ key: "recently_done", label: item.status, symbol: "✓", tone: "muted" }]),
	);
	const closedFiltered = sortQueue(
		applyFilters(closedRows, { ...state, buckets: [] }),
		state.sort === "queue" ? "updated" : state.sort,
	);
	const showClosed = scope !== "open";
	if (showClosed && closedFiltered.length > 0) {
		sections.push({ meta: groupMeta("closed"), rows: closedFiltered });
	}

	return {
		items: showOpen ? sortQueue(queued, state.sort) : [],
		groups: sections,
		total: showOpen ? openItems.length : 0,
		filtered: showOpen ? openItems.length - queued.length : 0,
		closed: closedFiltered,
		closedTotal: closedItems.length,
		showClosedGroup: showClosed,
	};
}

/** Filter + search, shared by the open queue and the closed fold. */
export function applyFilters(rows: QueuedItem[], state: ViewState): QueuedItem[] {
	const wanted = new Set(state.buckets);
	const priorities = new Set(state.priorities);
	const query = state.search.trim().toLowerCase();
	return rows.filter((row) => {
		if (wanted.size > 0 && !row.badges.some((badge) => wanted.has(badge.key))) return false;
		if (priorities.size > 0 && !priorities.has(row.item.priority)) return false;
		if (query && !matchesQuery(row.item, query)) return false;
		return true;
	});
}

/** Search covers what a user would type: id, title, labels, status, holder. */
export function matchesQuery(item: WorkItem, query: string): boolean {
	const haystack = [
		item.identifier,
		item.title,
		item.status,
		item.item_type,
		item.priority,
		item.health,
		item.progress_summary ?? "",
		item.next_action ?? "",
		item.blocker ?? "",
		item.waiting_reason ?? "",
		item.claim?.holder ?? "",
		item.labels.join(" "),
	]
		.join("\n")
		.toLowerCase();
	return query
		.split(/\s+/)
		.filter(Boolean)
		.every((term) => haystack.includes(term));
}

export function sortQueue(rows: QueuedItem[], sort: SortKey): QueuedItem[] {
	const copy = [...rows];
	switch (sort) {
		case "priority":
			copy.sort((a, b) => {
				const byPriority = priorityRank(a.item.priority) - priorityRank(b.item.priority);
				return byPriority !== 0 ? byPriority : byIdentifier(a.item, b.item);
			});
			return copy;
		case "updated":
			copy.sort((a, b) => {
				const byTime = Date.parse(b.item.updated_at) - Date.parse(a.item.updated_at);
				return Number.isNaN(byTime) || byTime === 0 ? byIdentifier(a.item, b.item) : byTime;
			});
			return copy;
		case "identifier":
			copy.sort((a, b) => byIdentifier(a.item, b.item));
			return copy;
		case "progress":
			// Longest without progress first: what is quietly rotting.
			copy.sort((a, b) => {
				const byAge = (b.item.seconds_since_progress ?? 0) - (a.item.seconds_since_progress ?? 0);
				return byAge !== 0 ? byAge : byIdentifier(a.item, b.item);
			});
			return copy;
		case "queue":
		default:
			copy.sort((a, b) => {
				const byBadge = badgeRank(a) - badgeRank(b);
				if (byBadge !== 0) return byBadge;
				const byPriority = priorityRank(a.item.priority) - priorityRank(b.item.priority);
				if (byPriority !== 0) return byPriority;
				return byIdentifier(a.item, b.item);
			});
			return copy;
	}
}

function badgeRank(row: QueuedItem): number {
	const order = new Map(ACTIVE_BUCKETS.map((bucket, index) => [bucket.key, index]));
	return row.badges.reduce((best, badge) => Math.min(best, order.get(badge.key) ?? 99), 99);
}

function byIdentifier(a: WorkItem, b: WorkItem): number {
	if (a.number !== b.number) return a.number - b.number;
	return a.identifier.localeCompare(b.identifier);
}

export interface Metric {
	label: string;
	value: number;
	hint?: string;
	tone?: "danger" | "warn" | "info" | "ok" | "muted";
}

/**
 * Header summary, computed from the *complete* item snapshot (APM-49).
 *
 * The dashboard's own totals are a recent window: `status:done` only counts items
 * finished inside `done_recent_seconds`, and `waiting_human` counts attention on
 * items that may already be closed. Reading those made the header disagree with
 * the list right below it (e.g. "done 15" next to a Closed group of 48), so the
 * numbers come from the same `groupItems` classification the queue uses:
 *
 *   open           — every item that is not closed (any open category);
 *   in progress    — the `in_progress` group (active category or a live claim);
 *   ready          — the `startable` group (open, unclaimed, unblocked);
 *   waiting human  — non-closed items holding an open attention (may overlap
 *                    with in progress: an active item can still be waiting on a
 *                    human), so it is counted directly instead of via the group;
 *   at risk        — the `at_risk` group;
 *   closed         — the `closed` group: done, cancelled, aborted and any custom
 *                    closed state, not just the recent-completion window.
 */
export function summarizeItems(items: readonly WorkItem[], now: number = Date.now()): Metric[] {
	const { counts } = groupItems(items, now);
	const count = (key: GroupKey): number => counts.get(key) ?? 0;
	const closed = count("closed");
	const open = items.length - closed;
	// Waiting human is *not* the needs_human group: that group is a bucket for the
	// queue (in progress beats waiting human), while the header counts the question
	// itself. An active or reviewing item that asked a human still waits for one, so
	// the two numbers may legitimately overlap; closed items never count (APM-49).
	const waitingHuman = items.filter(
		(item) => !isClosed(item) && (item.waiting_on_human === true || (item.attention ?? []).some((entry) => entry.status === "open")),
	).length;
	const atRisk = count("at_risk");
	return [
		{
			label: "open",
			value: open,
			hint: "every item in an open state (all groups except closed)",
			tone: open > 0 ? "info" : "muted",
		},
		{ label: "in progress", value: count("in_progress"), hint: "active status or a live claim", tone: "info" },
		{ label: "ready", value: count("startable"), hint: "open, unclaimed and unblocked", tone: "ok" },
		{
			label: "waiting human",
			value: waitingHuman,
			hint: "non-closed items with an open question, including ones already in progress",
			tone: waitingHuman > 0 ? "warn" : "muted",
		},
		{ label: "at risk", value: atRisk, hint: "stale claims, bad health or unexplained waiting", tone: atRisk > 0 ? "danger" : "muted" },
		{
			label: "closed",
			value: closed,
			hint: "every terminal state: done, cancelled, aborted and custom closed",
			tone: "muted",
		},
	];
}

// ---------------------------------------------------------------------------
// Time / freshness
// ---------------------------------------------------------------------------

/** `12s ago`, `4m ago`, `2h ago`, `3d ago` — relative, stable and timezone-free. */
export function relativeTime(iso: string | null | undefined, now: number): string {
	if (!iso) return "—";
	const at = Date.parse(iso);
	if (Number.isNaN(at)) return "—";
	const seconds = Math.round((now - at) / 1000);
	if (seconds < 0) return "just now";
	if (seconds < 60) return `${seconds}s ago`;
	const minutes = Math.round(seconds / 60);
	if (minutes < 60) return `${minutes}m ago`;
	const hours = Math.round(minutes / 60);
	if (hours < 24) return `${hours}h ago`;
	const days = Math.round(hours / 24);
	return `${days}d ago`;
}

/** Absolute clock time for "data as of" labels (local timezone, second precision). */
export function clockTime(iso: string | null | undefined): string {
	if (!iso) return "—";
	const at = Date.parse(iso);
	if (Number.isNaN(at)) return "—";
	const date = new Date(at);
	const pad = (value: number): string => String(value).padStart(2, "0");
	return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

export type ConnectionState = "live" | "refreshing" | "stale" | "offline" | "connecting";

export interface ConnectionInput {
	lastSuccessAt: number | null;
	lastError: string | null;
	refreshing: boolean;
	now: number;
	/** How old data may be before it is labelled stale (default: 3 refresh cycles). */
	staleAfterMs?: number;
}

export interface ConnectionView {
	state: ConnectionState;
	label: string;
	detail: string;
	/** True when the user is looking at cached data. */
	cached: boolean;
}

/**
 * Connection and cache labelling. A failed fetch never clears the view: the user
 * keeps reading the last good snapshot and is told exactly how old it is.
 */
export function connectionView(input: ConnectionInput): ConnectionView {
	const staleAfter = input.staleAfterMs ?? 15_000;
	const age = input.lastSuccessAt === null ? null : input.now - input.lastSuccessAt;
	const cachedLabel = input.lastSuccessAt === null ? "no data yet" : `cached ${clockTime(new Date(input.lastSuccessAt).toISOString())}`;

	if (input.lastError !== null && input.lastSuccessAt !== null) {
		return {
			state: "offline",
			label: "offline",
			detail: `${cachedLabel} · last refresh failed: ${input.lastError}`,
			cached: true,
		};
	}
	if (input.lastError !== null) {
		return { state: "offline", label: "offline", detail: `last refresh failed: ${input.lastError}`, cached: false };
	}
	if (input.lastSuccessAt === null) {
		return { state: "connecting", label: "connecting", detail: "waiting for the first response", cached: false };
	}
	if (age !== null && age > staleAfter) {
		return { state: "stale", label: "stale", detail: `${cachedLabel} · refresh is overdue`, cached: true };
	}
	if (input.refreshing) {
		return { state: "refreshing", label: "refreshing", detail: cachedLabel, cached: false };
	}
	return { state: "live", label: "live", detail: `updated ${relativeTime(new Date(input.lastSuccessAt!).toISOString(), input.now)}`, cached: false };
}

/** Data-age warning for the header, independent of the connection state. */
export function dataAge(iso: string | null | undefined, now: number): { text: string; old: boolean } {
	if (!iso) return { text: "no data", old: true };
	const at = Date.parse(iso);
	if (Number.isNaN(at)) return { text: "unknown", old: true };
	const ageMs = now - at;
	return { text: relativeTime(iso, now), old: ageMs > 10_000 };
}
