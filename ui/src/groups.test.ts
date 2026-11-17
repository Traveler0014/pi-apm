/**
 * Rules for the mutually exclusive dashboard groups (APM-32).
 *
 * The regression that motivated these tests: a `doing` item without blockers
 * appeared under "Ready to start" because readiness looked at the open/blocked
 * flags only. Now status, claims, attention, health and dependencies are folded
 * into one group with an explanation; every item lands in exactly one group.
 */

import { describe, expect, it } from "vitest";
import {
	GROUPS,
	classifyItem,
	effectiveClaim,
	expiredClaim,
	groupItems,
	groupOrder,
	indexItems,
	openBlockers,
} from "./groups";
import type { ClaimRef, ItemRef, WorkItem } from "./types";

const NOW = Date.parse("2026-11-17T02:00:00.000Z");

interface Spec {
	id?: string;
	status?: string;
	category?: string;
	blockedBy?: Array<{ id: string; status: string }>;
	waitingHuman?: boolean;
	waitingReason?: string;
	health?: string;
	claim?: Partial<ClaimRef> | null;
}

function claim(overrides: Partial<ClaimRef> = {}): ClaimRef {
	return {
		id: "claim-1",
		holder: "agent",
		holder_kind: "agent",
		status: "active",
		expires_at: new Date(NOW + 60_000).toISOString(),
		...overrides,
	};
}

function make(specs: Spec[]): WorkItem[] {
	return specs.map((spec, index) => {
		const id = spec.id ?? `item-${index + 1}`;
		const status = spec.status ?? "ready";
		return {
			id,
			project: "p",
			project_id: "proj",
			number: index + 1,
			identifier: id.toUpperCase(),
			title: `item ${index + 1}`,
			item_type: "task",
			status,
			status_category: spec.category ?? "open",
			priority: "normal",
			version: 1,
			context_version: 1,
			waiting_on_human: spec.waitingHuman ?? false,
			waiting_reason: spec.waitingReason ?? null,
			created_by: "agent",
			created_at: "2026-11-17T01:00:00.000Z",
			updated_at: "2026-11-17T01:00:00.000Z",
			health: spec.health ?? "unclaimed",
			claim: spec.claim === null ? null : spec.claim ? claim(spec.claim) : null,
			labels: [],
			blocked_by: (spec.blockedBy ?? []) as ItemRef[],
			blocks: [],
			attention: [],
		} satisfies WorkItem;
	});
}

function groupOf(item: WorkItem, all: WorkItem[] = [item]) {
	return classifyItem(item, indexItems(all), NOW);
}

describe("group precedence", () => {
	it("keeps a doing item out of Startable even when it has no blockers", () => {
		const item = make([{ status: "doing", category: "active" }])[0]!;
		const result = groupOf(item);
		expect(result.group).toBe("in_progress");
		expect(result.codes).toContain("active-status");
	});

	it("puts review and any valid claim in In progress", () => {
		const items = make([
			{ status: "review", category: "review" },
			{ status: "ready", claim: {} },
		]);
		const review = items[0]!;
		const claimed = items[1]!;
		expect(groupOf(review).group).toBe("in_progress");
		expect(groupOf(claimed).group).toBe("in_progress");
		expect(groupOf(claimed).codes).toContain("valid-claim");
	});

	it("only lets open, unclaimed, unblocked healthy items start", () => {
		const item = make([{ status: "ready", category: "open" }])[0]!;
		const result = groupOf(item);
		expect(result.group).toBe("startable");
		expect(result.codes).toEqual(expect.arrayContaining(["unclaimed", "unblocked"]));
	});

	it("does not treat an expired lease as ownership", () => {
		const item = make([
			{
				status: "ready",
				claim: { expires_at: new Date(NOW - 1_000).toISOString() },
			},
		])[0]!;
		expect(effectiveClaim(item, NOW)).toBeNull();
		expect(expiredClaim(item, NOW)).not.toBeNull();
		const result = groupOf(item);
		expect(result.group).toBe("at_risk");
		expect(result.codes).toContain("claim-expired");
	});

	it("prefers needs-human over blockers and risk", () => {
		const blocker = make([{ id: "b1", status: "doing", category: "active" }])[0]!;
		const item = make([
			{
				status: "ready",
				waitingHuman: true,
				waitingReason: "Which backend?",
				blockedBy: [{ id: blocker.id, status: blocker.status }],
				health: "stale",
			},
		])[0]!;
		const result = groupOf(item, [blocker, item]);
		expect(result.group).toBe("needs_human");
		expect(result.explanation).toBe("Which backend?");
	});

	it("treats closed blockers as satisfied, even custom closed states", () => {
		const shipped = make([{ id: "b1", status: "shipped", category: "closed" }])[0]!;
		const item = make([
			{ id: "i1", status: "ready", blockedBy: [{ id: shipped.id, status: "shipped" }] },
		])[0]!;
		const result = groupOf(item, [shipped, item]);
		expect(result.openBlockers).toHaveLength(0);
		expect(result.group).toBe("startable");
	});

	it("keeps waiting upstream when a blocker is still open", () => {
		const doing = make([{ id: "b1", status: "doing", category: "active" }])[0]!;
		const item = make([
			{ id: "i1", status: "ready", blockedBy: [{ id: doing.id, status: "doing" }] },
		])[0]!;
		const result = groupOf(item, [doing, item]);
		expect(result.group).toBe("waiting_upstream");
		expect(result.codes).toContain("open-blockers:1");
	});

	it("ignores blockers outside the loaded snapshot, like the topology graph", () => {
		const unknown = make([
			{ id: "i1", blockedBy: [{ id: "gone", status: "doing" }] },
		])[0]!;
		expect(groupOf(unknown).openBlockers).toEqual([]);
		expect(groupOf(unknown).group).toBe("startable");
	});

	it("ignores self-references", () => {
		const self = make([{ id: "i1", blockedBy: [{ id: "i1", status: "doing" }] }])[0]!;
		expect(groupOf(self).openBlockers).toEqual([]);
		expect(groupOf(self).group).toBe("startable");
	});

	it("keeps doing-with-blockers in progress and exposes the upstream badge", () => {
		const blocker = make([{ id: "b1", status: "doing", category: "active" }])[0]!;
		const item = make([
			{
				id: "i1",
				status: "doing",
				category: "active",
				blockedBy: [{ id: blocker.id, status: "doing" }],
			},
		])[0]!;
		const result = groupOf(item, [blocker, item]);
		expect(result.group).toBe("in_progress");
		expect(result.openBlockers).toHaveLength(1);
		expect(result.codes).toContain("open-blockers:1");
	});

	it("treats a waiting category without a reason as an anomaly", () => {
		const item = make([{ status: "blocked", category: "waiting" }])[0]!;
		const result = groupOf(item);
		expect(result.group).toBe("at_risk");
		expect(result.codes).toContain("waiting-without-reason");
	});

	it("flags stale execution health", () => {
		const item = make([{ status: "doing", category: "active", health: "stale" }])[0]!;
		const result = groupOf(item);
		// Active work stays In progress; the health problem is a badge/reason.
		expect(result.group).toBe("in_progress");
		expect(result.codes).not.toContain("valid-claim");
	});
});

describe("grouping is total and disjoint", () => {
	it("assigns every item to exactly one group in display order", () => {
		const doing = make([{ id: "d1", status: "doing", category: "active" }])[0]!;
		const claimed = make([{ id: "c1", status: "ready", claim: {} }])[0]!;
		const stale = make([{ id: "s1", status: "ready", health: "stale" }])[0]!;
		const blocked = make([
			{ id: "b1", status: "ready", blockedBy: [{ id: doing.id, status: "doing" }] },
		])[0]!;
		const human = make([{ id: "h1", status: "ready", waitingHuman: true }])[0]!;
		const ready = make([{ id: "r1", status: "ready" }])[0]!;
		const done = make([{ id: "z1", status: "done", category: "closed" }])[0]!;
		const items = [doing, claimed, stale, blocked, human, ready, done];

		const grouped = groupItems(items, NOW);
		expect(groupOrder()).toEqual([
			"needs_human",
			"at_risk",
			"in_progress",
			"waiting_upstream",
			"startable",
			"closed",
		]);
		const seen = [...grouped.byGroup.values()].flat();
		expect(seen).toHaveLength(items.length);
		expect(new Set(seen.map((item) => item.id)).size).toBe(items.length);

		expect(grouped.byGroup.get("in_progress")!.map((i) => i.id).sort()).toEqual(["c1", "d1"]);
		expect(grouped.byGroup.get("at_risk")!.map((i) => i.id)).toEqual(["s1"]);
		expect(grouped.byGroup.get("waiting_upstream")!.map((i) => i.id)).toEqual(["b1"]);
		expect(grouped.byGroup.get("needs_human")!.map((i) => i.id)).toEqual(["h1"]);
		expect(grouped.byGroup.get("startable")!.map((i) => i.id)).toEqual(["r1"]);
		expect(grouped.byGroup.get("closed")!.map((i) => i.id)).toEqual(["z1"]);
		for (const group of GROUPS) {
			expect(grouped.counts.get(group.key)).toBe(grouped.byGroup.get(group.key)!.length);
		}
	});

	it("resolves blocker categories from the shared index", () => {
		const closed = make([{ id: "b1", status: "shipped", category: "closed" }])[0]!;
		const item = make([
			{ id: "i1", blockedBy: [{ id: closed.id, status: "shipped" }] },
		])[0]!;
		expect(openBlockers(item, indexItems([closed, item]))).toHaveLength(0);
		expect(classifyItem(item, indexItems([closed, item]), NOW).group).toBe("startable");
	});
});
