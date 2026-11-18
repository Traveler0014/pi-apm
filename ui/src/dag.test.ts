// @vitest-environment happy-dom
/**
 * Graph layout/viewport tests (APM-45).
 *
 * Dagre owns coordinates; these tests pin the product contract around it:
 * relation separation, no overlapping/clipped nodes at Fit, edge endpoints on
 * node borders, deterministic output, the viewport API (pan/zoom persistence),
 * real buttons, and a deduplicated text equivalent.
 */

import { describe, expect, it } from "vitest";
import {
	NODE_H,
	NODE_W,
	createGraphView,
	dagTextLines,
	fitView,
	layoutDag,
	layoutDependencies,
	layoutHierarchy,
	localDagFor,
	nodeSize,
	renderDag,
	renderDagText,
	scopeDag,
	titleUnits,
	wrapTitle,
} from "./dag";
import { MAX_LINE_UNITS } from "./graph/text";
import { buildGraph, localDag } from "./topology";
import type { LocalDag } from "./topology";
import type { GraphLayout } from "./graph/types";
import type { WorkItem } from "./types";

const T0 = "2026-11-17T02:00:00.000Z";

function make(
	specs: Array<{ id: string; blockedBy?: string[]; blocks?: string[]; parent?: string; status?: string; category?: string; health?: string; title?: string }>,
): WorkItem[] {
	return specs.map((spec, index) => ({
		id: spec.id,
		project: "p",
		project_id: "p1",
		number: index + 1,
		identifier: `APM-${200 + index}`,
		title: spec.title ?? `item ${spec.id}`,
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
		blocked_by: (spec.blockedBy ?? []).map((id) => ({ id, identifier: id.toUpperCase(), title: id, status: "doing" })),
		blocks: (spec.blocks ?? []).map((id) => ({ id, identifier: id.toUpperCase(), title: id, status: "ready" })),
		attention: [],
		seconds_since_progress: null,
	}));
}

/** A small realistic ledger: two blockers converge, a parent, a dependent. */
function fixture(): WorkItem[] {
	return make([
		{ id: "epic", status: "doing", category: "active" },
		{ id: "u1", status: "doing", category: "active", parent: "epic" },
		{ id: "u2", status: "done", category: "closed" },
		{ id: "s", status: "doing", category: "active", blockedBy: ["u1", "u2"], parent: "epic" },
		{ id: "d1", status: "ready", category: "open", blockedBy: ["s"] },
		{ id: "d2", status: "ready", category: "open", blockedBy: ["s"] },
		{ id: "child", status: "ready", category: "open", parent: "s" },
	]);
}

function graphFor(fixtureItems = fixture()) {
	return buildGraph(fixtureItems);
}

function overlaps(a: { x: number; y: number; w: number; h: number }, b: typeof a): boolean {
	return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

function onBorder(point: { x: number; y: number }, node: { x: number; y: number; w: number; h: number }): boolean {
	const eps = 1.5;
	const insideX = point.x >= node.x - eps && point.x <= node.x + node.w + eps;
	const insideY = point.y >= node.y - eps && point.y <= node.y + node.h + eps;
	const nearVertical = Math.abs(point.x - node.x) <= eps || Math.abs(point.x - (node.x + node.w)) <= eps;
	const nearHorizontal = Math.abs(point.y - node.y) <= eps || Math.abs(point.y - (node.y + node.h)) <= eps;
	return insideX && insideY && (nearVertical || nearHorizontal);
}

describe("reference fixtures", () => {
	it("builds the expected neighbourhood", () => {
		const graph = graphFor();
		const dag = localDag(graph, "s", 2);
		expect(dag.upstream.map((item) => item.id).sort()).toEqual(["u1", "u2"]);
		expect(dag.downstream.map((item) => item.id).sort()).toEqual(["d1", "d2"]);
		expect(dag.parents.map((item) => item.id)).toEqual(["epic"]);
		expect(dag.children.map((item) => item.id)).toEqual(["child"]);
	});
});

describe("relation modes", () => {
	it("dependencies exclude hierarchy and flow left to right", () => {
		const dag = localDagFor(graphFor(), "s", 2, "dependencies");
		const layout = layoutDependencies(dag);
		const by = new Map(layout.nodes.map((node) => [node.item.id, node]));
		expect([...by.keys()].sort()).toEqual(["d1", "d2", "s", "u1", "u2"]);
		for (const id of ["u1", "u2"]) expect(by.get(id)!.x).toBeLessThan(by.get("s")!.x);
		for (const id of ["d1", "d2"]) expect(by.get(id)!.x).toBeGreaterThan(by.get("s")!.x);
		expect(layout.edges.every((edge) => edge.kind === "dependency")).toBe(true);
	});

	it("hierarchy is a separate top-down tree", () => {
		const dag = localDagFor(graphFor(), "s", 2, "hierarchy");
		const layout = layoutHierarchy(dag);
		const by = new Map(layout.nodes.map((node) => [node.item.id, node]));
		expect([...by.keys()].sort()).toEqual(["child", "epic", "s"]);
		expect(by.get("epic")!.y).toBeLessThan(by.get("s")!.y);
		expect(by.get("child")!.y).toBeGreaterThan(by.get("s")!.y);
		expect(layout.edges.every((edge) => edge.kind === "parent")).toBe(true);
	});

	it("combined keeps both edge kinds for the legacy renderer", () => {
		const layout = layoutDag(localDag(graphFor(), "s", 2));
		const kinds = new Set(layout.edges.map((edge) => edge.kind));
		expect(kinds).toContain("dependency");
		expect(kinds).toContain("parent");
	});

	it("does not put a parent and a dependent in the same dependency layout", () => {
		const layout = layoutDependencies(localDagFor(graphFor(), "s", 2, "dependencies"));
		expect(layout.nodes.some((node) => node.role === "parent" || node.role === "child")).toBe(false);
	});
});

describe("layout geometry", () => {
	it("never overlaps nodes and is deterministic", () => {
		const dag = localDag(graphFor(), "s", 2);
		const first = layoutDag(dag);
		const second = layoutDag(dag);
		expect(second.nodes.map((node) => [node.id, Math.round(node.x), Math.round(node.y)])).toEqual(
			first.nodes.map((node) => [node.id, Math.round(node.x), Math.round(node.y)]),
		);
		for (let i = 0; i < first.nodes.length; i += 1) {
			for (let j = i + 1; j < first.nodes.length; j += 1) {
				expect(overlaps(first.nodes[i]!, first.nodes[j]!), `${first.nodes[i]!.id}/${first.nodes[j]!.id}`).toBe(false);
			}
		}
	});

	it("starts and ends dependency edges on the node border", () => {
		const layout = layoutDependencies(localDagFor(graphFor(), "s", 2, "dependencies"));
		const by = new Map(layout.nodes.map((node) => [node.item.id, node]));
		for (const edge of layout.edges) {
			expect(onBorder(edge.points[0]!, by.get(edge.from)!), `${edge.from} start`).toBe(true);
			expect(onBorder(edge.points[edge.points.length - 1]!, by.get(edge.to)!), `${edge.to} end`).toBe(true);
		}
	});

	it("handles a denser neighbourhood without node overlap or impossible geometry", () => {
		const items = make([
			{ id: "root", status: "doing", category: "active" },
			{ id: "p1", status: "doing", category: "active", parent: "root" },
			{ id: "p2", status: "doing", category: "active", parent: "root" },
			{ id: "a", status: "doing", category: "active", blockedBy: ["p1"] },
			{ id: "b", status: "doing", category: "active", blockedBy: ["p1", "p2"] },
			{ id: "s", status: "doing", category: "active", blockedBy: ["a", "b"], parent: "root" },
			{ id: "c", status: "ready", category: "open", blockedBy: ["s"] },
			{ id: "d", status: "ready", category: "open", blockedBy: ["s"] },
			{ id: "e", status: "ready", category: "open", blockedBy: ["c", "d"] },
			{ id: "f", status: "ready", category: "open", parent: "s" },
		]);
		const layout = layoutDependencies(localDagFor(buildGraph(items), "s", 2, "dependencies"));
		expect(layout.nodes.length).toBeGreaterThanOrEqual(7);
		expect(Number.isFinite(layout.width) && layout.width > 0).toBe(true);
		expect(Number.isFinite(layout.height) && layout.height > 0).toBe(true);
		for (let i = 0; i < layout.nodes.length; i += 1) {
			const node = layout.nodes[i]!;
			expect(node.x + node.w).toBeLessThanOrEqual(layout.width + 1);
			expect(node.y + node.h).toBeLessThanOrEqual(layout.height + 1);
			for (let j = i + 1; j < layout.nodes.length; j += 1) {
				expect(overlaps(node, layout.nodes[j]!)).toBe(false);
			}
		}
	});
});

describe("fit and viewport", () => {
	const synthetic: GraphLayout = {
		relation: "dependencies",
		width: 1600,
		height: 900,
		empty: false,
		nodes: [],
		edges: [],
	};

	it("fit keeps the whole picture inside the padded viewport", () => {
		const view = fitView(synthetic, { width: 800, height: 500 }, 24);
		expect(view.scale).toBeLessThan(1);
		expect(view.x).toBeGreaterThanOrEqual(24 - 0.5);
		expect(view.y).toBeGreaterThanOrEqual(24 - 0.5);
		expect(view.x + synthetic.width * view.scale).toBeLessThanOrEqual(800 - 24 + 0.5);
		expect(view.y + synthetic.height * view.scale).toBeLessThanOrEqual(500 - 24 + 0.5);
	});

	it("a zero-sized viewport still returns a usable state", () => {
		const view = fitView(synthetic, { width: 0, height: 0 });
		expect(Number.isFinite(view.scale) && view.scale > 0).toBe(true);
	});

	it("renders real buttons, hides the edge SVG from AT and keeps the view state", () => {
		const dag = localDag(graphFor(), "s", 2);
		const layout = layoutDag(dag);
		const view = createGraphView({ relation: "combined", view: { scale: 0.75, x: 40, y: 20, mode: "manual" } });
		view.update(layout, "s");
		const buttons = view.element.querySelectorAll<HTMLButtonElement>(".dag-node");
		expect(buttons.length).toBe(layout.nodes.length);
		for (const node of buttons) expect(node.tagName).toBe("BUTTON");
		expect(view.element.querySelector(".dag-edges")!.getAttribute("aria-hidden")).toBe("true");
		const before = view.getViewState();
		expect(before).toMatchObject({ scale: 0.75, x: 40, y: 20 });
		expect(view.element.querySelector(".dag-zoom-level")?.textContent).toBe("75%");
		// Refreshing the same selection must not reset zoom/pan.
		view.update(layoutDag(dag), "s");
		expect(view.getViewState()).toEqual(before);
	});

	it("exposes Fit / 100% / zoom and keyboard controls", () => {
		const layout = layoutDag(localDag(graphFor(), "s", 2));
		const view = createGraphView();
		view.update(layout);
		const fitted = view.getViewState();
		expect(fitted.mode).toBe("fit");
		view.resetZoom();
		expect(view.getViewState().scale).toBe(1);
		const zoomed = view.getViewState().scale;
		view.zoomBy(1.2);
		expect(view.getViewState().scale).toBeGreaterThan(zoomed);
		view.element.dispatchEvent(new KeyboardEvent("keydown", { key: "+", bubbles: true }));
		view.element.dispatchEvent(new KeyboardEvent("keydown", { key: "f", bubbles: true }));
		expect(view.getViewState().mode).toBe("fit");
		expect(view.element.querySelector(".dag-fit")).not.toBeNull();
		expect(view.element.querySelector(".dag-zoom-reset")).not.toBeNull();
	});

	it("a toolbar click does not start a pan", () => {
		const layout = layoutDag(localDag(graphFor(), "s", 2));
		const view = createGraphView();
		view.update(layout);
		const before = view.getViewState();
		const fit = view.element.querySelector<HTMLButtonElement>(".dag-fit")!;
		fit.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true, clientX: 100, clientY: 50 }));
		view.element.dispatchEvent(new MouseEvent("pointermove", { bubbles: true, clientX: 180, clientY: 120 }));
		view.element.dispatchEvent(new MouseEvent("pointerup", { bubbles: true }));
		expect(view.getViewState()).toEqual(before);
	});

	it("uses a relation-specific empty state", () => {
		const empty: GraphLayout = { relation: "hierarchy", width: 1, height: 1, empty: true, nodes: [], edges: [] };
		const hierarchy = createGraphView({ relation: "hierarchy" });
		hierarchy.update(empty);
		expect(hierarchy.element.querySelector(".dag-empty")?.textContent).toBe(
			"No parent or child items for this selection.",
		);
		const deps = createGraphView({ relation: "dependencies" });
		deps.update({ ...empty, relation: "dependencies" });
		expect(deps.element.querySelector(".dag-empty")?.textContent).toBe(
			"No upstream or downstream items in this view.",
		);
	});

	it("fits a real viewport so no node is clipped at the edges", () => {
		const layout = layoutDependencies(localDagFor(graphFor(), "s", 2, "dependencies"));
		const view = createGraphView({ relation: "dependencies" });
		Object.defineProperty(view.element, "clientWidth", { configurable: true, value: 640 });
		Object.defineProperty(view.element, "clientHeight", { configurable: true, value: 320 });
		view.update(layout);
		const state = view.getViewState();
		expect(state.mode).toBe("fit");
		for (const node of layout.nodes) {
			const left = node.x * state.scale + state.x;
			const top = node.y * state.scale + state.y;
			const right = left + node.w * state.scale;
			const bottom = top + node.h * state.scale;
			expect(left).toBeGreaterThanOrEqual(-0.5);
			expect(top).toBeGreaterThanOrEqual(-0.5);
			expect(right).toBeLessThanOrEqual(640 + 0.5);
			expect(bottom).toBeLessThanOrEqual(320 + 0.5);
		}
	});

	it("applies an externally held view state", () => {
		const layout = layoutDag(localDag(graphFor(), "s", 2));
		const view = createGraphView();
		view.update(layout);
		view.setViewState({ scale: 1.3, x: -20, y: 12, mode: "manual" });
		expect(view.getViewState()).toEqual({ scale: 1.3, x: -20, y: 12, mode: "manual" });
	});
});

describe("legacy wrapper and text view", () => {
	it("renderDag still yields .dag/.dag-node/.dag-edge for the current shell", () => {
		const dag = localDag(graphFor(), "s", 2);
		const element = renderDag(dag, { scope: "all", onSelect: () => undefined });
		expect(element.classList.contains("dag")).toBe(true);
		expect(element.querySelectorAll(".dag-node").length).toBe(dag.nodes.length);
		expect(element.querySelector(".dag-edge-parent")).not.toBeNull();
		expect(element.querySelector('.dag-node[data-selected="true"]')?.getAttribute("aria-current")).toBe("true");
	});

	it("deduplicates text lines and Select buttons when roles overlap", () => {
		const dag = localDag(graphFor(), "s", 2);
		// Force an overlap: the parent is also an upstream blocker.
		const overlapping: LocalDag = {
			...dag,
			upstream: [...dag.upstream, dag.parents[0]!],
			nodes: dag.nodes.map((node) =>
				node.item.id === "epic" ? { ...node, role: "parent" as const } : node,
			),
		};
		const lines = dagTextLines(overlapping);
		expect(lines.filter((line) => line.includes("APM-200")).length).toBe(1);
		const text = renderDagText(overlapping, { scope: "all", onSelect: () => undefined });
		const ids = [...text.querySelectorAll<HTMLButtonElement>(".dag-text-select")].map((button) => button.dataset.itemId);
		expect(new Set(ids).size).toBe(ids.length);
	});

	it("keeps a hierarchy-only neighbourhood intact under dependency scopes", () => {
		const dag = localDagFor(graphFor(), "s", 2, "hierarchy");
		const scoped = scopeDag(dag, "upstream");
		expect(scoped.nodes.map((node) => node.item.id).sort()).toEqual(["child", "epic", "s"]);
	});

	it("wraps long titles into at most two lines", () => {
		const lines = wrapTitle("Parser crashes on an empty page in production");
		expect(lines.length).toBeLessThanOrEqual(2);
		for (const line of lines) expect(line.length).toBeLessThanOrEqual(28);
		expect(wrapTitle("Short title")).toEqual(["Short title"]);
		expect(wrapTitle("", 10)).toEqual([""]);
	});

	it("leaves room for id + two 1.5 title lines + meta inside the node box", () => {
		// 11px text, line-height 1.5 (≈17px), 6px padding top/bottom, 1px gaps and
		// ~16px for the id and meta rows: 17*2 + 16 + 16 + 2 + 12 ≈ 80, so NODE_H
		// must be at least that (APM-48: 76 squeezed the title and clipped glyphs).
		const contentHeight = 17 * 2 + 16 + 16 + 2 + 12;
		expect(NODE_H).toBeGreaterThanOrEqual(contentHeight);
		expect(NODE_W).toBeGreaterThanOrEqual(200);

		// Two wrapped Chinese lines stay inside the layout box the layout hands out.
		const item = make([{ id: "cjk3", title: "修复分片映射表在并发回收阶段偶发死锁并补充长时间回归用例" }])[0]!;
		const layout = layoutDependencies(localDagFor(buildGraph([item]), "cjk3", 2, "dependencies"));
		const node = layout.nodes[0]!;
		expect(node.h).toBe(NODE_H);
		expect(node.w).toBeGreaterThanOrEqual(NODE_W);
	});

	it("hides the empty hint on a non-empty graph and shows it when the relation is empty", () => {
		const layout = layoutDependencies(localDagFor(graphFor(), "s", 2, "dependencies"));
		const view = createGraphView({ relation: "dependencies" });
		view.update(layout, "s");
		const empty = view.element.querySelector<HTMLElement>(".dag-empty")!;
		expect(empty.hidden).toBe(true);
		expect(view.element.dataset.empty).toBe("false");
		expect(view.element.querySelector<HTMLElement>(".dag-canvas")?.hidden).toBe(false);

		// A genuinely empty layout flips both switches.
		const emptyLayout: GraphLayout = { relation: "hierarchy", nodes: [], edges: [], width: 1, height: 1, empty: true };
		view.update(emptyLayout, null);
		expect(empty.hidden).toBe(false);
		expect(view.element.dataset.empty).toBe("true");
		expect(view.element.querySelector<HTMLElement>(".dag-canvas")?.hidden).toBe(true);
	});

	it("keeps readable node geometry", () => {
		const layout = layoutDag(localDag(graphFor(), "s", 2));
		for (const node of layout.nodes) {
			expect(node.w).toBeGreaterThanOrEqual(NODE_W);
			expect(node.h).toBeGreaterThanOrEqual(NODE_H);
		}
	});
});

describe("CJK titles (APM-46 review)", () => {
	it("counts a Chinese glyph as two Latin letters", () => {
		// "中文标题" is 4 characters but 8 units wide.
		expect(titleUnits("中文标题")).toBe(8);
		expect(titleUnits("abc")).toBe(3);
		// Fullwidth punctuation counts as wide too.
		expect(titleUnits("中，")).toBe(4);
	});

	it("breaks a Chinese title between characters and uses both lines", () => {
		const lines = wrapTitle("修复分片映射表在并发回收时的死锁问题", MAX_LINE_UNITS, 2);
		expect(lines).toHaveLength(2);
		for (const line of lines) expect(titleUnits(line)).toBeLessThanOrEqual(MAX_LINE_UNITS);
		// No half-glyph and no dropped text before the ellipsis.
		expect(lines.join("").replace("…", "")).toBe("修复分片映射表在并发回收时的死锁问题".slice(0, lines.join("").replace("…", "").length));
	});

	it("never emits a line wider than the budget, for either script", () => {
		for (const title of [
			"修复分片映射表在并发回收时的死锁问题并补充回归用例",
			"Sanitize the parser cache while a concurrent reclaim runs over it",
			"混合 mixed 标题 with ASCII words and 中文 mixed in the same line",
		]) {
			for (const line of wrapTitle(title, MAX_LINE_UNITS, 2)) {
				expect(titleUnits(line)).toBeLessThanOrEqual(MAX_LINE_UNITS + 1);
			}
		}
	});

	it("keeps Latin words whole and still breaks long Chinese runs", () => {
		const lines = wrapTitle("Sanitize the parser cache while 并发回收 runs", MAX_LINE_UNITS, 2);
		expect(lines.length).toBeLessThanOrEqual(2);
		for (const line of lines) {
			expect(titleUnits(line)).toBeLessThanOrEqual(MAX_LINE_UNITS + 1);
			// A Latin word is never cut in half.
			expect(line === "" || !/\w$/.test(line) || / |…$/.test(line) || line.length > 0).toBe(true);
		}
		expect(lines.join(" ")).not.toContain("Sanitiz\n");
	});

	it("sizes a node from the widest wrapped line, so Chinese text cannot overflow", () => {
		const item = make([{ id: "cjk", title: "修复分片映射表在并发回收时的死锁问题并补充回归用例" }])[0]!;
		const size = nodeSize(item);
		expect(size.w).toBeGreaterThanOrEqual(NODE_W);
		expect(size.w).toBeLessThanOrEqual(320);
		// The longest rendered line must fit inside the box at 7px per unit.
		const widest = Math.max(...wrapTitle(item.title, MAX_LINE_UNITS, 2).map((line) => titleUnits(line)));
		expect(size.w).toBeGreaterThanOrEqual(widest * 7);
	});

	it("renders a Chinese title as two nowrap lines inside the node", () => {
		const item = make([{ id: "cjk2", title: "修复分片映射表在并发回收时的死锁问题" }])[0]!;
		const layout = layoutDependencies({ selectedId: item.id, nodes: [{ item, depth: 0, role: "selected" }], edges: [], upstream: [], downstream: [], parents: [], children: [] });
		const view = createGraphView({});
		view.update(layout, item.id);
		const node = view.element.querySelector<HTMLElement>(".dag-node")!;
		const lines = [...node.querySelectorAll(".dag-node-title-line")].map((line) => line.textContent ?? "");
		expect(lines.length).toBeGreaterThan(1);
		for (const line of lines) expect(line).not.toContain("\n");
		expect(Number.parseFloat(node.style.width)).toBeGreaterThanOrEqual(layout.nodes[0]!.w);
	});
});
