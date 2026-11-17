/**
 * Mutually exclusive work groups for the dashboard (APM-32).
 *
 * A ledger item carries several independent dimensions — business status
 * (workflow category), execution health (claim/lease), dependencies and human
 * attention. The queue used to infer "ready" from a single bucket, so a `doing`
 * item without blockers could appear under "Ready to start". This module turns
 * those dimensions into **one** group per item and keeps the others as badges.
 *
 * Precedence (first match wins), with the reason recorded for the UI:
 *
 *   1. closed         — terminal item; history is not queue work.
 *   2. in_progress    — workflow category `active`/`review`, or a *valid* claim.
 *   3. needs_human    — `waiting_on_human` (a human owes an answer).
 *   4. at_risk        — unhealthy execution record (stale/lost lease, recovery)
 *                       or a `waiting` category with no recorded reason.
 *   5. waiting_upstream — open (non-closed) blockers; work cannot start yet.
 *   6. startable      — open, unclaimed, unblocked, healthy: the only group
 *                       the UI may call "Startable".
 *
 * Grouping is total and disjoint: every item is in exactly one group. Badges
 * such as "blocked by 2" or "waiting human" still render for items in other
 * groups (a `doing` item with open blockers stays in progress and shows the
 * upstream badge), so nothing is hidden — only the headline is unambiguous.
 *
 * Everything here is pure; the tests in `groups.test.ts` pin the rules.
 */

import type { ClaimRef, ItemRef, WorkItem } from "./types";

export type GroupKey =
	| "needs_human"
	| "at_risk"
	| "in_progress"
	| "waiting_upstream"
	| "startable"
	| "closed";

export type GroupTone = "danger" | "warn" | "info" | "ok" | "muted";

export interface GroupMeta {
	key: GroupKey;
	/** Human label; status must never be conveyed by colour alone. */
	label: string;
	/** One-character marker used next to the label. */
	symbol: string;
	tone: GroupTone;
	/** Display order for sections and the default queue sort. */
	order: number;
	/** Shown as the section's explanation. */
	description: string;
}

/** Display order: action for the human first, then execution, then backlog. */
export const GROUPS: GroupMeta[] = [
	{
		key: "needs_human",
		label: "Needs human",
		symbol: "⏸",
		tone: "warn",
		order: 0,
		description: "Waiting for an answer; agents cannot make progress.",
	},
	{
		key: "at_risk",
		label: "At risk",
		symbol: "▲",
		tone: "danger",
		order: 1,
		description: "Stale lease, failed recovery or an unexplained waiting state.",
	},
	{
		key: "in_progress",
		label: "In progress",
		symbol: "●",
		tone: "info",
		order: 2,
		description: "Active or in review, or held by a valid claim.",
	},
	{
		key: "waiting_upstream",
		label: "Waiting upstream",
		symbol: "◆",
		tone: "warn",
		order: 3,
		description: "Open blockers; cannot start until upstream work closes.",
	},
	{
		key: "startable",
		label: "Startable",
		symbol: "○",
		tone: "ok",
		order: 4,
		description: "Open, unclaimed, unblocked and healthy.",
	},
	{
		key: "closed",
		label: "Closed",
		symbol: "✓",
		tone: "muted",
		order: 5,
		description: "Terminal: done, cancelled, aborted or a custom closed state.",
	},
];

const GROUP_BY_KEY = new Map(GROUPS.map((group) => [group.key, group]));

export function groupMeta(key: GroupKey): GroupMeta {
	const meta = GROUP_BY_KEY.get(key);
	if (meta) return meta;
	// Every GroupKey has metadata; this keeps the return type honest for custom
	// callers without adding a non-null assertion in the hot path.
	return GROUPS[GROUPS.length - 1]!;
}

const AT_RISK_HEALTH = new Set(["stale", "lost_lease", "recovery_failed"]);
const ACTIVE_CATEGORIES = new Set(["active", "review"]);

export type ItemLookup = ReadonlyMap<string, WorkItem>;

/** Index the full item list so blocker categories can be resolved. */
export function indexItems(items: readonly WorkItem[]): ItemLookup {
	return new Map(items.map((item) => [item.id, item]));
}

function isClosedItem(item: Pick<WorkItem, "status_category">): boolean {
	return item.status_category === "closed";
}

/**
 * A claim that still protects the item at `now`. Released/expired claims do not
 * count: an expired lease is an execution problem, not ownership.
 */
export function effectiveClaim(item: WorkItem, now: number): ClaimRef | null {
	const claim = item.claim;
	if (!claim) return null;
	if (claim.status !== "active") return null;
	if (claim.expired === true) return null;
	const expires = Date.parse(claim.expires_at);
	if (Number.isNaN(expires)) return claim;
	return expires > now ? claim : null;
}

/** The claim that exists but no longer protects the item (for the At risk group). */
export function expiredClaim(item: WorkItem, now: number): ClaimRef | null {
	const claim = item.claim;
	if (!claim) return null;
	if (claim.status !== "active") return null;
	return effectiveClaim(item, now) === null ? claim : null;
}

/** Blockers that are not closed, using the loaded item list when possible. */
export function openBlockers(item: WorkItem, lookup: ItemLookup): ItemRef[] {
	return item.blocked_by.filter((blocker) => {
		// Self-references are ledger noise (the API rejects new ones) and unknown
		// ids are outside this snapshot; neither can be worked on from here, so
		// they never block. This matches the topology graph's behaviour.
		if (blocker.id === item.id) return false;
		const loaded = lookup.get(blocker.id);
		if (!loaded) return false;
		return !isClosedItem(loaded);
	});
}

export interface Classification {
	group: GroupKey;
	/** Short, stable reason codes (tested and used for tooltips). */
	codes: string[];
	/** Human-readable explanation shown next to the group. */
	explanation: string;
	openBlockers: ItemRef[];
	effectiveClaim: ClaimRef | null;
	expiredClaim: ClaimRef | null;
}

function classify(item: WorkItem, lookup: ItemLookup, now: number): Classification {
	const activeClaim = effectiveClaim(item, now);
	const staleClaim = expiredClaim(item, now);
	const blockers = openBlockers(item, lookup);
	const codes: string[] = [];
	let group: GroupKey;
	let explanation: string;

	if (isClosedItem(item)) {
		group = "closed";
		codes.push("closed");
		explanation = `closed (${item.status})`;
	} else if (ACTIVE_CATEGORIES.has(item.status_category)) {
		group = "in_progress";
		codes.push(`${item.status_category}-status`);
		if (activeClaim) codes.push("valid-claim");
		// Blockers do not move an active item out of "in progress" — they become
		// an upstream badge so the work stays visible where it actually is.
		if (blockers.length > 0) codes.push(`open-blockers:${blockers.length}`);
		explanation = `${item.status} (${item.status_category})`;
	} else if (activeClaim) {
		group = "in_progress";
		codes.push("valid-claim");
		explanation = `held by ${activeClaim.holder}`;
	} else if (item.waiting_on_human) {
		group = "needs_human";
		codes.push("waiting-human");
		explanation = item.waiting_reason?.trim() || "waiting for a human answer";
	} else if (AT_RISK_HEALTH.has(item.health) || staleClaim) {
		group = "at_risk";
		if (AT_RISK_HEALTH.has(item.health)) codes.push(`health:${item.health}`);
		if (staleClaim) codes.push("claim-expired");
		if (blockers.length > 0) codes.push(`open-blockers:${blockers.length}`);
		const parts = [];
		if (staleClaim) parts.push("lease expired");
		if (AT_RISK_HEALTH.has(item.health)) parts.push(`health ${item.health}`);
		explanation = parts.join(" · ") || "execution state needs attention";
	} else if (blockers.length > 0) {
		group = "waiting_upstream";
		codes.push(`open-blockers:${blockers.length}`);
		explanation = `blocked by ${blockers.map((blocker) => blocker.identifier).join(", ")}`;
	} else if (item.status_category === "waiting") {
		// A waiting category must say *why*. A dependency or a human request is a
		// reason; without one it is an anomaly, not a normal backlog item.
		group = "at_risk";
		codes.push("waiting-without-reason");
		explanation = "waiting status without a recorded dependency or human request";
	} else {
		group = "startable";
		codes.push("unclaimed", "unblocked");
		explanation = "open, unclaimed, unblocked";
	}

	return {
		group,
		codes,
		explanation,
		openBlockers: blockers,
		effectiveClaim: activeClaim,
		expiredClaim: staleClaim,
	};
}

/** Classify one item; used by the queue, topology and the detail drawer. */
export function classifyItem(
	item: WorkItem,
	lookup: ItemLookup,
	now: number,
): Classification {
	return classify(item, lookup, now);
}

export interface GroupedItems {
	/** Items per group, preserving the input order within each group. */
	byGroup: Map<GroupKey, WorkItem[]>;
	/** Classification per item id (same object used for badges/explanations). */
	classification: Map<string, Classification>;
	/** Group sizes, including empty groups, in display order. */
	counts: Map<GroupKey, number>;
}

/**
 * Partition items into the mutually exclusive groups. The lookup may be omitted
 * when `items` is the complete ledger view; otherwise pass all loaded items so
 * blocker categories can be resolved.
 */
export function groupItems(
	items: readonly WorkItem[],
	now: number,
	lookup: ItemLookup = indexItems(items),
): GroupedItems {
	const byGroup = new Map<GroupKey, WorkItem[]>(GROUPS.map((group) => [group.key, []]));
	const classification = new Map<string, Classification>();
	const counts = new Map<GroupKey, number>(GROUPS.map((group) => [group.key, 0]));
	for (const item of items) {
		const result = classify(item, lookup, now);
		classification.set(item.id, result);
		byGroup.get(result.group)!.push(item);
		counts.set(result.group, (counts.get(result.group) ?? 0) + 1);
	}
	return { byGroup, classification, counts };
}

/** Group keys in display order. */
export function groupOrder(): GroupKey[] {
	return GROUPS.map((group) => group.key);
}
