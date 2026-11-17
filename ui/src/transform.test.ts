import { describe, expect, it } from "vitest";
import { MOCK_CLOSED_EXTRA, MOCK_DONE, MOCK_ITEMS, mockDashboard } from "./fixtures";
import {
	BUCKETS,
	buildQueue,
	clockTime,
	connectionView,
	dataAge,
	defaultViewState,
	matchesQuery,
	relativeTime,
	sortQueue,
	summarize,
} from "./transform";
import type { BucketKey, Dashboard } from "./types";

const NOW = Date.parse("2026-11-17T02:10:00.000Z");

function state(patch: Partial<ReturnType<typeof defaultViewState>> = {}) {
	return { ...defaultViewState(), ...patch };
}

function dashboard(): Dashboard {
	return mockDashboard(new Date(NOW).toISOString());
}

/** Everything the server would return for `?all=true`, open first. */
function allItems(): typeof MOCK_ITEMS {
	return [...MOCK_ITEMS, ...MOCK_DONE, ...MOCK_CLOSED_EXTRA];
}

function queue(patch: Partial<ReturnType<typeof defaultViewState>> = {}, items: typeof MOCK_ITEMS | null = allItems()) {
	return buildQueue(dashboard(), items, state(patch));
}

describe("bucket metadata", () => {
	it("names every bucket with a label and a non-colour symbol", () => {
		expect(BUCKETS).toHaveLength(8);
		for (const bucket of BUCKETS) {
			expect(bucket.label.length).toBeGreaterThan(2);
			expect(bucket.symbol.length).toBeGreaterThan(0);
			expect(["danger", "warn", "info", "ok", "muted"]).toContain(bucket.tone);
		}
	});
});

describe("buildQueue: dedupe and multi-badges", () => {
	it("collapses an item that appears in several buckets into one row with every badge", () => {
		const model = queue();
		const apm14 = model.items.filter((row) => row.item.identifier === "APM-14");
		expect(apm14).toHaveLength(1);
		const keys = apm14[0]!.badges.map((badge) => badge.key);
		expect(keys).toEqual(["waiting_human", "in_progress"]);
		// Badge order follows the catalogue order, not the bucket iteration order.
		expect(keys).toEqual([...keys].sort((a, b) => orderOf(a) - orderOf(b)));
	});

	it("keeps the recently done bucket out of the main queue and counts it separately", () => {
		const model = queue();
		expect(model.items.some((row) => row.item.identifier === "APM-11")).toBe(false);
		expect(model.closedTotal).toBe(MOCK_DONE.length + MOCK_CLOSED_EXTRA.length);
		// Every closed category is counted, not just the dashboard's recently-done window.
		expect(model.closed.map((row) => row.item.identifier).sort()).toEqual(["APM-11", "APM-12", "APM-18", "APM-19"]);
		expect(model.total).toBe(MOCK_ITEMS.length);
	});

	it("does not mutate the dashboard payload", () => {
		const input = dashboard();
		const before = JSON.stringify(input);
		buildQueue(input, allItems(), state({ search: "apm", buckets: ["in_progress"] }));
		expect(JSON.stringify(input)).toBe(before);
	});
});

describe("buildQueue: filters and search", () => {
	it("filters by bucket chip", () => {
		const model = queue({ buckets: ["waiting_dependencies"] });
		expect(model.items.map((row) => row.item.identifier)).toEqual(["APM-17"]);
		expect(model.filtered).toBeGreaterThan(0);
	});

	it("filters by priority", () => {
		const model = queue({ priorities: ["highest"] });
		expect(model.items.map((row) => row.item.identifier)).toEqual(["APM-13"]);
	});

	it("searches identifier, title, label, progress and holder", () => {
		for (const [query, expected] of [
			["apm-16", ["APM-16"]],
			["detail drawer", ["APM-16"]],
				["rust", ["APM-14"]],
			["poller", ["APM-15"]],
			["apm-worker", ["APM-14"]],
			["apm-1", ["APM-15", "APM-16", "APM-14", "APM-17", "APM-13"]],
		] as Array<[string, string[]]>) {
			const model = queue({ search: query });
			expect(model.items.map((row) => row.item.identifier).sort()).toEqual([...expected].sort());
		}
	});

	it("requires every search term", () => {
		expect(matchesQuery(MOCK_ITEMS[0]!, "apm-15 frontend")).toBe(true);
		expect(matchesQuery(MOCK_ITEMS[0]!, "apm-15 rust")).toBe(false);
	});

	it("combines bucket, priority and search filters", () => {
		const model = queue({ buckets: ["in_progress"], priorities: ["high"], search: "ui" });
		expect(model.items.map((row) => row.item.identifier)).toEqual(["APM-14", "APM-15"]);
	});
});

describe("sortQueue", () => {
	const rows = queue().items;

	it("queue order puts the most urgent badge first", () => {
		const ranked = sortQueue(
			[
				...rows,
				{
					item: { ...MOCK_ITEMS[0]!, id: "i-lease", identifier: "APM-99", title: "lost lease" },
					badges: [{ key: "lost_lease", label: "lost lease", symbol: "▲", tone: "danger" }],
					group: "at_risk",
					explanation: "health lost_lease",
					openBlockerCount: 0,
				},
			],
			"queue",
		);
		expect(ranked[0]!.item.identifier).toBe("APM-99");
	});

	it("priority puts highest first, then identifier", () => {
		const ranked = sortQueue(rows, "priority");
		expect(ranked[0]!.item.priority).toBe("highest");
	});

	it("updated sorts newest first", () => {
		const ranked = sortQueue(rows, "updated");
		for (let i = 1; i < ranked.length; i += 1) {
			expect(Date.parse(ranked[i - 1]!.item.updated_at)).toBeGreaterThanOrEqual(Date.parse(ranked[i]!.item.updated_at));
		}
	});

	it("progress puts the item that has waited longest first", () => {
		const ranked = sortQueue(rows, "progress");
		const first = ranked[0]!.item.seconds_since_progress ?? 0;
		for (const row of ranked.slice(1)) expect(row.item.seconds_since_progress ?? 0).toBeLessThanOrEqual(first);
	});

	it("identifier sorts by number, not string", () => {
		const ranked = sortQueue(rows, "identifier");
		expect(ranked.map((row) => row.item.identifier)).toEqual(["APM-13", "APM-14", "APM-15", "APM-16", "APM-17"]);
	});

	it("does not mutate the incoming array", () => {
		const copy = [...rows];
		sortQueue(rows, "identifier");
		expect(rows).toEqual(copy);
	});
});

describe("summarize", () => {
	it("reads the totals the backend already computed", () => {
		const metrics = summarize(dashboard());
		const byLabel = new Map(metrics.map((metric) => [metric.label, metric.value]));
		expect(byLabel.get("open")).toBe(5);
		expect(byLabel.get("in progress")).toBe(3);
		expect(byLabel.get("ready")).toBe(1);
		expect(byLabel.get("waiting human")).toBe(1);
		expect(byLabel.get("at risk")).toBe(1);
		expect(byLabel.get("done")).toBe(MOCK_DONE.length);
	});

	it("tolerates a dashboard without totals (bucket-derived metrics still work)", () => {
		const empty = { ...dashboard(), totals: {} as Record<string, number> };
		const byLabel = new Map(summarize(empty).map((metric) => [metric.label, metric.value]));
		expect(byLabel.get("open")).toBe(0);
		expect(byLabel.get("waiting human")).toBe(0);
		expect(byLabel.get("done")).toBe(0);
		// These come from the buckets, not from totals.
		expect(byLabel.get("in progress")).toBe(3);
		expect(byLabel.get("ready")).toBe(1);
	});
});

describe("time formatting", () => {
	it("renders relative ages", () => {
		expect(relativeTime(new Date(NOW - 12_000).toISOString(), NOW)).toBe("12s ago");
		expect(relativeTime(new Date(NOW - 5 * 60_000).toISOString(), NOW)).toBe("5m ago");
		expect(relativeTime(new Date(NOW - 3 * 3_600_000).toISOString(), NOW)).toBe("3h ago");
		expect(relativeTime(null, NOW)).toBe("—");
		expect(relativeTime("nonsense", NOW)).toBe("—");
	});

	it("renders a clock time and flags old data", () => {
		const iso = new Date(NOW).toISOString();
		expect(clockTime(iso)).toMatch(/^\d{2}:\d{2}:\d{2}$/);
		expect(dataAge(iso, NOW + 1_000).old).toBe(false);
		expect(dataAge(iso, NOW + 60_000).old).toBe(true);
		expect(dataAge(null, NOW)).toEqual({ text: "no data", old: true });
	});
});

describe("connectionView", () => {
	it("starts as connecting", () => {
		const view = connectionView({ lastSuccessAt: null, lastError: null, refreshing: false, now: NOW });
		expect(view.state).toBe("connecting");
		expect(view.cached).toBe(false);
	});

	it("is live for recent data and stale after the freshness window", () => {
		expect(connectionView({ lastSuccessAt: NOW - 2_000, lastError: null, refreshing: false, now: NOW }).state).toBe("live");
		const stale = connectionView({ lastSuccessAt: NOW - 20_000, lastError: null, refreshing: false, now: NOW });
		expect(stale.state).toBe("stale");
		expect(stale.cached).toBe(true);
	});

	it("reports a refresh in progress without claiming failure", () => {
		const view = connectionView({ lastSuccessAt: NOW - 1_000, lastError: null, refreshing: true, now: NOW });
		expect(view.state).toBe("refreshing");
	});

	it("keeps the cache timestamp on failure", () => {
		const view = connectionView({ lastSuccessAt: NOW - 30_000, lastError: "server unreachable", refreshing: false, now: NOW });
		expect(view.state).toBe("offline");
		expect(view.cached).toBe(true);
		expect(view.detail).toMatch(/cached \d{2}:\d{2}:\d{2}/);
		expect(view.detail).toContain("server unreachable");
	});

	it("distinguishes 'offline with no data' from 'offline with a cache'", () => {
		const view = connectionView({ lastSuccessAt: null, lastError: "boom", refreshing: false, now: NOW });
		expect(view.state).toBe("offline");
		expect(view.cached).toBe(false);
		expect(view.detail).toContain("boom");
	});
});

function orderOf(key: BucketKey): number {
	return BUCKETS.findIndex((bucket) => bucket.key === key);
}

describe("status scope (APM-30)", () => {
	it("open scope keeps the historical behaviour (open groups, closed only in the fold)", () => {
		const model = queue({ statusScope: "open" });
		expect(model.showClosedGroup).toBe(false);
		expect(model.groups.every((section) => section.meta.key !== "closed")).toBe(true);
		expect(model.items.length).toBeGreaterThan(0);
		expect(model.closedTotal).toBeGreaterThan(0);
	});

	it("closed scope promotes every closed category into one group and empties the open list", () => {
		const model = queue({ statusScope: "closed" });
		expect(model.showClosedGroup).toBe(true);
		expect(model.items).toEqual([]);
		expect(model.total).toBe(0);
		const closed = model.groups.find((section) => section.meta.key === "closed")!;
		expect(closed.meta.label).toBe("Closed");
		const ids = closed.rows.map((row) => row.item.identifier);
		expect(ids).toContain("APM-12"); // done
		expect(ids).toContain("APM-19"); // cancelled
		expect(ids).toContain("APM-18"); // aborted (custom closed state)
		expect(closed.rows.every((row) => row.group === "closed")).toBe(true);
	});

	it("all scope shows open groups plus the closed group", () => {
		const model = queue({ statusScope: "all" });
		expect(model.showClosedGroup).toBe(true);
		expect(model.groups.some((section) => section.meta.key === "closed")).toBe(true);
		expect(model.groups.some((section) => section.meta.key !== "closed")).toBe(true);
		expect(model.items.length).toBeGreaterThan(0);
	});

	it("search reaches closed work directly in every scope that shows it", () => {
		const closedOnly = queue({ statusScope: "closed", search: "APM-19" });
		expect(closedOnly.groups[0]!.rows.map((row) => row.item.identifier)).toEqual(["APM-19"]);
		const all = queue({ statusScope: "all", search: "APM-18" });
		const closed = all.groups.find((section) => section.meta.key === "closed")!;
		expect(closed.rows.map((row) => row.item.identifier)).toEqual(["APM-18"]);
		// A closed item does not leak into the open part of `all`.
		expect(all.items.some((row) => row.item.identifier === "APM-18")).toBe(false);
	});

	it("keeps total/filtered/closedTotal semantics for the open queue", () => {
		const all = queue({ statusScope: "open" });
		const filtered = queue({ statusScope: "open", search: "no-such-item" });
		expect(filtered.total).toBe(all.total);
		expect(filtered.filtered).toBe(all.total);
		expect(filtered.items).toEqual([]);
		expect(filtered.closedTotal).toBe(all.closedTotal);
	});
});
