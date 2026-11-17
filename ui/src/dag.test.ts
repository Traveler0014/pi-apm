import { describe, expect, it } from "vitest";
import { GAP_X, NODE_H, NODE_W, dagTextLines, layoutDag, scopeDag, wrapTitle } from "./dag";
import { buildGraph, localDag } from "./topology";
import type { WorkItem } from "./types";

const T0 = "2026-11-17T02:00:00.000Z";

function make(specs: Array<{ id: string; blockedBy?: string[]; parent?: string; status?: string; category?: string }>): WorkItem[] {
	return specs.map((spec, index) => ({
		id: spec.id,
		project: "p",
		project_id: "p1",
		number: index + 1,
		identifier: `APM-${200 + index}`,
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
		created_by: "t",
		created_at: T0,
		updated_at: T0,
		last_heartbeat_at: null,
		last_progress_at: null,
		last_run_sync_at: null,
		done_at: null,
		cancelled_at: null,
		health: "unclaimed",
		claim: null,
		labels: [],
		blocked_by: (spec.blockedBy ?? []).map((id) => ({ id, identifier: id, title: id, status: "open" })),
		blocks: [],
		attention: [],
		seconds_since_progress: null,
	}));
}

const items = make([
	{ id: "up", category: "active" },
	{ id: "mid", blockedBy: ["up"] },
	{ id: "down", blockedBy: ["mid"] },
	{ id: "epic", category: "active" },
	{ id: "sibling", parent: "epic" },
]);
items[1]!.parent_id = "epic";
const graph = buildGraph(items);

describe("layoutDag", () => {
	it("places upstream left, the selection centre and dependents right", () => {
		const layout = layoutDag(localDag(graph, "mid", 2));
		const by = new Map(layout.nodes.map((node) => [node.item.id, node]));
		expect(by.get("up")!.x).toBeLessThan(by.get("mid")!.x);
		expect(by.get("down")!.x).toBeGreaterThan(by.get("mid")!.x);
		expect(by.get("mid")!.role).toBe("selected");
		// Columns are one node wide apart.
		expect(by.get("down")!.x - by.get("mid")!.x).toBe(NODE_W + GAP_X);
	});

	it("keeps parents above and children below the selected row", () => {
		const layout = layoutDag(localDag(graph, "mid", 1));
		const by = new Map(layout.nodes.map((node) => [node.item.id, node]));
		expect(by.get("epic")!.role).toBe("parent");
		expect(by.get("epic")!.y).toBeLessThan(by.get("mid")!.y);
		expect(by.get("epic")!.x).toBe(by.get("mid")!.x);
	});

	it("draws dependency edges with an arrow curve and hierarchy edges dashed", () => {
		const layout = layoutDag(localDag(graph, "mid", 1));
		const dependency = layout.edges.find((edge) => edge.kind === "dependency")!;
		const parent = layout.edges.find((edge) => edge.kind === "parent")!;
		expect(dependency.d).toMatch(/^M .* C /);
		expect(parent.d).toMatch(/^M .* L /);
		expect(new Set(layout.edges.map((edge) => edge.kind))).toEqual(new Set(["dependency", "parent"]));
	});

	it("sizes the canvas to fit every column and row", () => {
		const layout = layoutDag(localDag(graph, "mid", 2));
		for (const node of layout.nodes) {
			expect(node.x + node.w).toBeLessThanOrEqual(layout.width);
			expect(node.y + node.h).toBeLessThanOrEqual(layout.height);
		}
		expect(layout.height).toBeGreaterThanOrEqual(NODE_H);
	});

	it("reports an empty neighbourhood instead of drawing nothing", () => {
		const alone = buildGraph(make([{ id: "solo" }]));
		const layout = layoutDag(localDag(alone, "solo"));
		expect(layout.empty).toBe(false); // the selected node itself is still drawn
		const missing = layoutDag(localDag(alone, "nope"));
		expect(missing.empty).toBe(true);
	});
});

describe("scopeDag", () => {
	const dag = localDag(graph, "mid", 2);

	it("blocking keeps dependency edges only and drops hierarchy nodes", () => {
		const scoped = scopeDag(dag, "blocking");
		expect(scoped.nodes.every((node) => node.role !== "parent" && node.role !== "child")).toBe(true);
		expect(scoped.edges.every((edge) => edge.kind === "dependency")).toBe(true);
		expect(scoped.parents).toEqual([]);
		expect(scoped.children).toEqual([]);
	});

	it("upstream keeps the selected item and what blocks it", () => {
		const scoped = scopeDag(dag, "upstream");
		const ids = scoped.nodes.map((node) => node.item.id).sort();
		expect(ids).toEqual(["mid", "up"]);
		expect(scoped.downstream).toEqual([]);
	});

	it("downstream keeps the selected item and what it unblocks", () => {
		const scoped = scopeDag(dag, "downstream");
		expect(scoped.nodes.map((node) => node.item.id).sort()).toEqual(["down", "mid"]);
		expect(scoped.upstream).toEqual([]);
	});
});

describe("dagTextLines", () => {
	it("names relations in reading order with status and holder", () => {
		const lines = dagTextLines(localDag(graph, "mid", 2));
		expect(lines[0]).toMatch(/^← blocks APM-200 · ready · unclaimed/);
		expect(lines.join("\n")).toContain("● selected APM-201 · ready");
		expect(lines.join("\n")).toContain("→ unblocks APM-202 · ready");
	});

	it("mirrors the scoped view exactly", () => {
		const scoped = dagTextLines(scopeDag(localDag(graph, "mid", 2), "upstream"));
		expect(scoped.join("\n")).not.toContain("unblocks");
		expect(scoped.join("\n")).toContain("blocks");
	});
});

describe("readable node geometry (APM-29)", () => {
	it("uses boxes tall enough for a two-line title", () => {
		expect(NODE_W).toBeGreaterThanOrEqual(200);
		expect(NODE_H).toBeGreaterThanOrEqual(72);
	});

	it("wraps titles into at most two lines without cutting words", () => {
		const lines = wrapTitle("Parser crashes on an empty page in production");
		expect(lines.length).toBeLessThanOrEqual(2);
		expect(lines.join(" ").length).toBeLessThanOrEqual(
			"Parser crashes on an empty page in production".length + 1,
		);
		for (const line of lines) expect(line.length).toBeLessThanOrEqual(28);
		expect(lines[0]!.startsWith("Parser")).toBe(true);
	});

	it("keeps short titles on one line and never emits empty lines", () => {
		expect(wrapTitle("Short title")).toEqual(["Short title"]);
		expect(wrapTitle("", 10)).toEqual([""]);
	});

	it("the expanded layout is larger than the inline one", () => {
		const dag = localDag(graph, "mid", 2);
		const inline = layoutDag(dag);
		const expanded = layoutDag(dag, { nodeW: NODE_W + 56, nodeH: NODE_H + 26 });
		expect(expanded.width).toBeGreaterThan(inline.width);
		expect(expanded.height).toBeGreaterThan(inline.height);
	});
});
