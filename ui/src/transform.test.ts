import { describe, expect, it } from "vitest";
import { MOCK_CLOSED_EXTRA, MOCK_DONE, MOCK_ITEMS, mockDashboard } from "./fixtures";
import {
	BUCKETS,
	ITEM_LIST_LIMIT,
	buildQueue,
	clockTime,
	connectionView,
	dataAge,
	defaultViewState,
	isListTruncated,
	matchesQuery,
	relativeTime,
	sortQueue,
	summarizeItems,
} from "./transform";
import type { BucketKey, Dashboard } from "./types";
import { groupItems } from "./groups";

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

describe("summarizeItems (header metrics from the full snapshot)", () => {
	const labels = (items: ReturnType<typeof allItems>) =>
		new Map(summarizeItems(items, NOW).map((metric) => [metric.label, metric.value]));

	it("counts the complete snapshot, not the dashboard's recent window", () => {
		const metrics = labels(allItems());
		const closed = allItems().filter((item) => item.status_category === "closed").length;
		expect(metrics.get("closed")).toBe(closed);
		expect(metrics.has("done")).toBe(false);
		expect(metrics.get("open")).toBe(allItems().length - closed);
	});

	it("agrees with the queue groups the list renders", () => {
		const { counts } = groupItems(allItems(), NOW);
		const metrics = labels(allItems());
		expect(metrics.get("in progress")).toBe(counts.get("in_progress"));
		expect(metrics.get("ready")).toBe(counts.get("startable"));
		expect(metrics.get("at risk")).toBe(counts.get("at_risk"));
		// Waiting human is deliberately *not* the needs_human bucket: an item that is
		// already in progress (or reviewing) but asked a human still waits for one.
		const waitingHuman = allItems().filter(
			(item) => item.status_category !== "closed" && (item.waiting_on_human || (item.attention ?? []).some((entry) => entry.status === "open")),
		).length;
		expect(metrics.get("waiting human")).toBe(waitingHuman);
		expect(waitingHuman).toBeGreaterThanOrEqual(counts.get("needs_human")!);
		expect(metrics.get("closed")).toBe(counts.get("closed"));
		// Open is exactly the sum of the open groups.
		const open = [...counts.entries()]
			.filter(([key]) => key !== "closed")
			.reduce((total, [, value]) => total + value, 0);
		expect(metrics.get("open")).toBe(open);
	});

	it("a fully closed ledger reads OPEN 0 / WAITING HUMAN 0 / CLOSED n", () => {
		// The reported screenshot: 48 closed items, two of them carrying a closed
		// attention. Nothing is open, and attention on closed work is not backlog.
		const closedItems = Array.from({ length: 48 }, (_, index) => ({
			...MOCK_DONE[0]!,
			id: `closed-${index}`,
			number: index + 1,
			identifier: `APM-${index + 1}`,
			status: index % 3 === 0 ? "cancelled" : index % 3 === 1 ? "done" : "aborted",
			status_category: "closed",
			waiting_on_human: index < 2,
			waiting_reason: index < 2 ? "closed item still carries an open attention" : null,
		}));
		const metrics = labels(closedItems as ReturnType<typeof allItems>);
		expect(metrics.get("open")).toBe(0);
		expect(metrics.get("in progress")).toBe(0);
		expect(metrics.get("ready")).toBe(0);
		expect(metrics.get("waiting human")).toBe(0);
		expect(metrics.get("at risk")).toBe(0);
		expect(metrics.get("closed")).toBe(48);
	});

	it("counts custom closed states and never let a doing item count as ready", () => {
		const custom = {
			...MOCK_ITEMS[0]!,
			id: "custom-closed",
			number: 99,
			identifier: "APM-99",
			status: "dropped",
			status_category: "closed",
		};
		const doing = { ...MOCK_ITEMS[1]!, id: "doing", number: 98, identifier: "APM-98", status: "doing", status_category: "active" };
		const metrics = labels([...allItems(), custom, doing] as ReturnType<typeof allItems>);
		expect(metrics.get("closed")).toBe(labels(allItems()).get("closed")! + 1);
		// A doing item belongs to in progress; ready stays the startable group only.
		expect(metrics.get("in progress")).toBe(groupItems([...allItems(), custom, doing], NOW).counts.get("in_progress"));
		expect(metrics.get("ready")).toBe(groupItems([...allItems(), custom, doing], NOW).counts.get("startable"));
	});

	it("counts an in-progress item that is waiting on a human (overlap is allowed)", () => {
		const active = {
			...MOCK_ITEMS[0]!,
			id: "active-asking",
			number: 97,
			identifier: "APM-97",
			status: "doing",
			status_category: "active",
			waiting_on_human: true,
			attention: [{ id: "a1", question: "which backend?", status: "open", blocking: true }],
		};
		const metrics = labels([...allItems(), active] as ReturnType<typeof allItems>);
		const before = labels(allItems());
		expect(metrics.get("waiting human")).toBe(before.get("waiting human")! + 1);
		// It is in progress too: the two numbers describe different questions.
		expect(metrics.get("in progress")).toBe(before.get("in progress")! + 1);
	});

	it("ignores an open attention on a closed item but counts one without the flag", () => {
		const closedAsking = {
			...MOCK_DONE[0]!,
			id: "closed-asking",
			number: 96,
			identifier: "APM-96",
			status: "cancelled",
			status_category: "closed",
			waiting_on_human: false,
			attention: [{ id: "a2", question: "still needed?", status: "open", blocking: true }],
		};
		const openAsking = {
			...MOCK_ITEMS[0]!,
			id: "open-asking",
			number: 95,
			identifier: "APM-95",
			waiting_on_human: false,
			attention: [{ id: "a3", question: "which cache?", status: "open", blocking: false }],
		};
		const before = labels(allItems()).get("waiting human")!;
		expect(labels([...allItems(), closedAsking] as ReturnType<typeof allItems>).get("waiting human")).toBe(before);
		expect(labels([...allItems(), openAsking] as ReturnType<typeof allItems>).get("waiting human")).toBe(before + 1);
	});

	it("is empty-safe", () => {
		const metrics = new Map(summarizeItems([], NOW).map((metric) => [metric.label, metric.value]));
		expect(metrics.get("open")).toBe(0);
		expect(metrics.get("closed")).toBe(0);
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

describe("list truncation", () => {
	it("treats a full page of results as possibly truncated", () => {
		// The server returns at most `limit` rows, so `fetched == limit` means
		// "there may be more" — the header metrics must not pretend otherwise.
		expect(isListTruncated(ITEM_LIST_LIMIT, ITEM_LIST_LIMIT)).toBe(true);
		expect(isListTruncated(ITEM_LIST_LIMIT + 1, ITEM_LIST_LIMIT)).toBe(true);
		expect(isListTruncated(ITEM_LIST_LIMIT - 1, ITEM_LIST_LIMIT)).toBe(false);
		expect(isListTruncated(0, ITEM_LIST_LIMIT)).toBe(false);
		// The default cap is the one the dashboard fetches with.
		expect(isListTruncated(ITEM_LIST_LIMIT)).toBe(true);
		expect(ITEM_LIST_LIMIT).toBeGreaterThanOrEqual(500);
	});
});
