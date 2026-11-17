import { describe, expect, it } from "vitest";
import {
	buildGraph,
	findCycles,
	isOpenItem,
	localDag,
	localDagText,
	openBlockers,
	openDependents,
	topologySummary,
	unlockCount,
	upstreamChain,
} from "./topology";
import type { WorkItem } from "./types";

const T0 = "2026-11-17T02:00:00.000Z";

interface Spec {
	id: string;
	status?: string;
	category?: string;
	blockedBy?: string[];
	parent?: string;
	health?: string;
}

function make(specs: Spec[]): WorkItem[] {
	return specs.map((spec, index) => ({
		id: spec.id,
		project: "p",
		project_id: "p1",
		number: index + 1,
		identifier: `APM-${100 + index}`,
		title: `item ${spec.id}`,
		item_type: "task",
		status: spec.status ?? "ready",
		status_category: spec.category ?? "open",
		workflow_id: "apm-default",
		workflow_version: 1,
		priority: "normal",
		parent_id: spec.parent ?? null,
		parent_identifier: null,
		version: 1,
		context_version: 1,
		progress_summary: null,
		next_action: null,
		blocker: null,
		waiting_on_human: false,
		waiting_reason: null,
		completion_note: null,
		created_by: "test",
		created_at: T0,
		updated_at: T0,
		last_heartbeat_at: null,
		last_progress_at: null,
		last_run_sync_at: null,
		done_at: null,
		cancelled_at: null,
		health: spec.health ?? "unclaimed",
		claim: null,
		labels: [],
		blocked_by: (spec.blockedBy ?? []).map((id) => ({ id, identifier: `APM-${id}`, title: id, status: "open" })),
		blocks: [],
		attention: [],
		seconds_since_progress: null,
	}));
}

const byId = (items: WorkItem[], id: string): WorkItem => items.find((item) => item.id === id)!;

describe("buildGraph", () => {
	it("keeps dependency and parent edges apart", () => {
		const items = make([
			{ id: "a" },
			{ id: "b", blockedBy: ["a"], parent: "a" },
		]);
		const graph = buildGraph(items);
		expect(graph.edges).toEqual(
			expect.arrayContaining([
				{ from: "a", to: "b", kind: "dependency" },
				{ from: "a", to: "b", kind: "parent" },
			]),
		);
		expect(graph.nodes.get("b")!.dependsOn).toEqual(["a"]);
		expect(graph.nodes.get("b")!.parentId).toBe("a");
		expect(graph.nodes.get("a")!.childIds).toEqual(["b"]);
		expect(graph.nodes.get("a")!.blockedDependents).toEqual(["b"]);
	});

	it("drops references to items outside the snapshot and self-loops", () => {
		const items = make([{ id: "a", blockedBy: ["ghost", "a"] }]);
		const graph = buildGraph(items);
		expect(graph.nodes.get("a")!.dependsOn).toEqual([]);
		expect(graph.edges).toEqual([]);
		expect(graph.readyIds).toEqual(["a"]);
	});

	it("classifies open vs closed by category, not by state name", () => {
		expect(isOpenItem({ status_category: "open" })).toBe(true);
		expect(isOpenItem({ status_category: "review" })).toBe(true);
		expect(isOpenItem({ status_category: "closed" })).toBe(false);
		const graph = buildGraph(make([{ id: "a", status: "aborted", category: "closed" }]));
		expect(graph.readyIds).toEqual([]);
		expect(graph.waitingIds).toEqual([]);
	});
});

describe("ready / waiting", () => {
	it("only counts open blockers, so closed work stops blocking", () => {
		const items = make([
			{ id: "done", category: "closed", status: "done" },
			{ id: "open", category: "active", status: "doing" },
			{ id: "waiting_done", blockedBy: ["done"], category: "waiting" },
			{ id: "waiting_open", blockedBy: ["open"], category: "waiting" },
			{ id: "waiting_chain", blockedBy: ["waiting_open"], category: "waiting" },
		]);
		const graph = buildGraph(items);
		// `open` has no blockers itself; `waiting_done` is unblocked because `done` closed.
		expect(graph.readyIds).toEqual(["open", "waiting_done"]);
		expect(new Set(graph.waitingIds)).toEqual(new Set(["waiting_open", "waiting_chain"]));
		expect(openBlockers(graph, "waiting_open").map((item) => item.id)).toEqual(["open"]);
		expect(openBlockers(graph, "waiting_done")).toEqual([]);
		expect(openDependents(graph, "open").map((item) => item.id)).toEqual(["waiting_open"]);
	});

	it("reports direct and transitive blockers and how much a node unlocks", () => {
		const items = make([
			{ id: "a", category: "active" },
			{ id: "b", blockedBy: ["a"], category: "waiting" },
			{ id: "c", blockedBy: ["b"], category: "waiting" },
			{ id: "d", blockedBy: [], category: "open" },
		]);
		const graph = buildGraph(items);
		expect(upstreamChain(graph, "c").open.map((item) => item.id)).toEqual(["a", "b"]);
		expect(unlockCount(graph, "a")).toBe(2); // b and c
		expect(unlockCount(graph, "c")).toBe(0);
		const summary = topologySummary(graph);
		expect(summary.ready.map((item) => item.id)).toEqual(["a", "d"]);
		const waiting = summary.waiting.find((entry) => entry.item.id === "c")!;
		expect(waiting.directBlockers.map((item) => item.id)).toEqual(["b"]);
		expect(waiting.transitiveBlockers).toBe(2);
		// Ordered by how much they would unlock.
		expect(summary.waiting[0]!.item.id).toBe("b");
	});

	it("shows closed items separately, newest first", () => {
		const items = make([
			{ id: "done", category: "closed", status: "done" },
			{ id: "aborted", category: "closed", status: "aborted" },
			{ id: "cancelled", category: "closed", status: "cancelled" },
			{ id: "open" },
		]);
		items[0]!.updated_at = "2026-11-17T01:00:00.000Z";
		items[1]!.updated_at = "2026-11-17T03:00:00.000Z";
		items[2]!.updated_at = "2026-11-17T02:00:00.000Z";
		const summary = topologySummary(buildGraph(items));
		expect(summary.closed.map((item) => item.id)).toEqual(["aborted", "cancelled", "done"]);
	});
});

describe("cycle defence", () => {
	it("detects a two-node loop and never treats it as ready", () => {
		const items = make([
			{ id: "a", blockedBy: ["b"], category: "waiting" },
			{ id: "b", blockedBy: ["a"], category: "waiting" },
		]);
		const graph = buildGraph(items);
		expect(graph.cycles).toEqual(["a", "b"]);
		expect(graph.readyIds).toEqual([]);
		expect(new Set(graph.waitingIds)).toEqual(new Set(["a", "b"]));
		const summary = topologySummary(graph);
		expect(summary.cycles.map((item) => item.id).sort()).toEqual(["a", "b"]);
	});

	it("terminates on a longer loop plus a healthy tail", () => {
		const items = make([
			{ id: "a", blockedBy: ["c"], category: "waiting" },
			{ id: "b", blockedBy: ["a"], category: "waiting" },
			{ id: "c", blockedBy: ["b"], category: "waiting" },
			{ id: "tail", blockedBy: ["c"], category: "waiting" },
		]);
		const graph = buildGraph(items);
		expect(graph.cycles.sort()).toEqual(["a", "b", "c"]);
		// Transitive walks still terminate and do not include the start node.
		// The loop (a/b/c) cannot be unlocked by closing one node; only `tail` can.
		expect(unlockCount(graph, "c")).toBe(1);
		expect(upstreamChain(graph, "tail").open.map((item) => item.id).sort()).toEqual(["a", "b", "c"]);
	});

	it("findCycles reports every node on the loop", () => {
		const graph = buildGraph(
			make([
				{ id: "a", blockedBy: ["b"] },
				{ id: "b", blockedBy: ["a"] },
				{ id: "solo" },
			]),
		);
		expect(findCycles(graph.nodes).sort()).toEqual(["a", "b"]);
	});
});

describe("localDag", () => {
	const items = make([
		{ id: "root", category: "active", parent: "epic" },
		{ id: "epic", category: "active" },
		{ id: "mid", blockedBy: ["root"], category: "waiting" },
		{ id: "leaf", blockedBy: ["mid"], category: "waiting" },
		{ id: "far", blockedBy: ["leaf"], category: "waiting" },
		{ id: "sibling", parent: "epic" },
	]);

	it("returns only the neighbourhood, split into upstream/downstream/parents/children", () => {
		const graph = buildGraph(items);
		const dag = localDag(graph, "mid", 1);
		expect(dag.upstream.map((item) => item.id)).toEqual(["root"]);
		expect(dag.downstream.map((item) => item.id)).toEqual(["leaf"]);
		expect(dag.parents).toEqual([]);
		// hops=1 stops before `far`
		expect(dag.nodes.map((entry) => entry.item.id).sort()).toEqual(["leaf", "mid", "root"]);
		expect(dag.edges.every((edge) => ["root", "mid", "leaf"].includes(edge.from))).toBe(true);
	});

	it("keeps parent edges visible but never confuses them with dependencies", () => {
		const graph = buildGraph(items);
		const dag = localDag(graph, "root", 1);
		expect(dag.parents.map((item) => item.id)).toEqual(["epic"]);
		const kinds = dag.edges.map((edge) => edge.kind);
		expect(kinds).toContain("parent");
		expect(kinds).toContain("dependency");
	});

	it("handles a wider radius and unknown ids", () => {
		const graph = buildGraph(items);
		const wide = localDag(graph, "mid", 2);
		expect(wide.downstream.map((item) => item.id).sort()).toEqual(["far", "leaf"]);
		const missing = localDag(graph, "nope");
		expect(missing.nodes).toEqual([]);
		expect(localDagText(missing)).toEqual([
			"Upstream: none (nothing blocks this item)",
			"Downstream: none (unlocking it unblocks nothing)",
		]);
	});

	it("has a text equivalent that names every relation", () => {
		const graph = buildGraph(items);
		const text = localDagText(localDag(graph, "mid", 1)).join("\n");
		expect(text).toContain("Upstream: APM-100 (ready, unclaimed)");
		expect(text).toContain("Downstream: APM-103 (ready, unclaimed)");
		const rootText = localDagText(localDag(graph, "root", 1)).join("\n");
		expect(rootText).toContain("Parent: APM-101");
		// `root` has no children of its own; the epic does.
		expect(rootText).not.toContain("Children:");
		const epicText = localDagText(localDag(graph, "epic", 1)).join("\n");
		expect(epicText).toContain("Children: APM-100");
		expect(epicText).toContain("APM-105");
	});
});

describe("numbers stay stable", () => {
	it("uses item numbers for identifier ordering", () => {
		const items = make([{ id: "a" }, { id: "b" }]);
		const graph = buildGraph(items);
		expect(byId(items, "a").number).toBeLessThan(byId(items, "b").number);
		expect(topologySummary(graph).ready.map((item) => item.number)).toEqual([1, 2]);
	});
});
