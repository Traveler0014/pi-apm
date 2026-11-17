/**
 * Local dependency graph for the selected item.
 *
 * Vanilla SVG for the edges, real `<button>` elements for the nodes: the picture
 * is decorative (`aria-hidden`) and every node is keyboard-reachable through the
 * text list beside it, so a screen reader gets the same information without
 * parsing coordinates.
 *
 * Layout is pure arithmetic (no DOM measurement), which keeps it testable:
 * upstream columns to the left, the selected item in the middle, dependents to
 * the right, parents above and children below. Dependency edges are solid with an
 * arrowhead; parent/child edges are dashed without one — the two relations are
 * never drawn the same way.
 */

import type { LocalDag, DagScope, TopologyGraph } from "./topology";
import type { WorkItem } from "./types";

export const NODE_W = 168;
export const NODE_H = 58;
export const GAP_X = 46;
export const GAP_Y = 18;
const PAD = 14;

export type DagRole = LocalDag["nodes"][number]["role"];

export interface LaidOutNode {
	id: string;
	item: WorkItem;
	role: DagRole;
	x: number;
	y: number;
	w: number;
	h: number;
}

export interface LaidOutEdge {
	from: string;
	to: string;
	kind: "dependency" | "parent";
	/** SVG path data. */
	d: string;
}

export interface DagLayout {
	width: number;
	height: number;
	nodes: LaidOutNode[];
	edges: LaidOutEdge[];
	/** True when anything (a node or an edge) would fall outside the canvas. */
	empty: boolean;
}

/**
 * Position the neighbourhood.
 *
 * `dag.nodes[].depth` is negative upstream, 0 for the selected/parent/child
 * column and positive downstream, so the column index is just the depth.
 */
export function layoutDag(dag: LocalDag): DagLayout {
	const columns = new Map<number, LocalDag["nodes"]>();
	for (const entry of dag.nodes) {
		const column = entry.role === "parent" || entry.role === "child" || entry.role === "selected" ? 0 : entry.depth;
		const bucket = columns.get(column) ?? [];
		bucket.push(entry);
		columns.set(column, bucket);
	}
	const order: Record<DagRole, number> = { parent: -2, child: 2, upstream: 0, selected: 0, downstream: 0 };
	const sortedColumns = [...columns.keys()].sort((a, b) => a - b);
	const minColumn = Math.min(0, ...sortedColumns);
	const maxColumn = Math.max(0, ...sortedColumns);

	const placed = new Map<string, LaidOutNode>();
	for (const column of sortedColumns) {
		const bucket = [...(columns.get(column) ?? [])].sort(
			(a, b) => (order[a.role] ?? 0) - (order[b.role] ?? 0) || a.item.number - b.item.number,
		);
		const totalHeight = bucket.length * NODE_H + Math.max(0, bucket.length - 1) * GAP_Y;
		bucket.forEach((entry, index) => {
			const x = PAD + (column - minColumn) * (NODE_W + GAP_X);
			// Rows are centred inside the tallest column so the selected node sits in
			// the middle of its neighbourhood.
			const y = PAD + index * (NODE_H + GAP_Y) + Math.max(0, (maxRows(columns) * (NODE_H + GAP_Y) - totalHeight) / 2);
			placed.set(entry.item.id, {
				id: entry.item.id,
				item: entry.item,
				role: entry.role,
				x,
				y,
				w: NODE_W,
				h: NODE_H,
			});
		});
	}

	const edges: LaidOutEdge[] = [];
	for (const edge of dag.edges) {
		const from = placed.get(edge.from);
		const to = placed.get(edge.to);
		if (!from || !to) continue;
		if (edge.kind === "dependency") {
			const startX = from.x + from.w;
			const startY = from.y + from.h / 2;
			const endX = to.x - 6; // leave room for the arrowhead marker
			const endY = to.y + to.h / 2;
			const midX = (startX + endX) / 2;
			edges.push({
				...edge,
				d: `M ${startX} ${startY} C ${midX} ${startY}, ${midX} ${endY}, ${endX} ${endY}`,
			});
		} else {
			// Hierarchy: a straight dashed line between the two boxes.
			const startY = from.y < to.y ? from.y + from.h : from.y;
			const endY = from.y < to.y ? to.y : to.y + to.h;
			edges.push({ ...edge, d: `M ${from.x + from.w / 2} ${startY} L ${to.x + to.w / 2} ${endY}` });
		}
	}

	const width = Math.max(1, PAD * 2 + (maxColumn - minColumn + 1) * NODE_W + (maxColumn - minColumn) * GAP_X);
	const height = Math.max(1, PAD * 2 + maxRows(columns) * NODE_H + (maxRows(columns) - 1) * GAP_Y);

	return { width, height, nodes: [...placed.values()], edges, empty: placed.size === 0 };
}

function maxRows(columns: Map<number, LocalDag["nodes"]>): number {
	let rows = 1;
	for (const bucket of columns.values()) rows = Math.max(rows, bucket.length);
	return rows;
}

/**
 * Apply a scope to an already-built neighbourhood:
 *   - `blocking`: dependency edges/nodes only (hide hierarchy);
 *   - `upstream`: the selected item plus what blocks it;
 *   - `downstream`: the selected item plus what it unblocks;
 *   - `all`: everything (default).
 */
export function scopeDag(dag: LocalDag, scope: DagScope): LocalDag {
	if (scope === "all") return dag;
	if (scope === "blocking") {
		const keep = new Set([dag.selectedId, ...dag.upstream.map((item) => item.id), ...dag.downstream.map((item) => item.id)]);
		return {
			...dag,
			nodes: dag.nodes.filter((entry) => keep.has(entry.item.id)),
			edges: dag.edges.filter((edge) => edge.kind === "dependency" && keep.has(edge.from) && keep.has(edge.to)),
			parents: [],
			children: [],
		};
	}
	const keepUpstream = scope === "upstream";
	const keep = new Set([
		dag.selectedId,
		...(keepUpstream ? dag.upstream.map((item) => item.id) : dag.downstream.map((item) => item.id)),
	]);
	return {
		...dag,
		nodes: dag.nodes.filter((entry) => keep.has(entry.item.id)),
		edges: dag.edges.filter((edge) => edge.kind === "dependency" && keep.has(edge.from) && keep.has(edge.to)),
		// The side lists drive the text view, so they must mirror the scope too.
		upstream: keepUpstream ? dag.upstream : [],
		downstream: keepUpstream ? [] : dag.downstream,
		parents: keepUpstream ? dag.parents : [],
		children: keepUpstream ? [] : dag.children,
	};
}

/** Text lines for the accessible list: same nodes, same relations, no geometry. */
export function dagTextLines(dag: LocalDag): string[] {
	const lines: string[] = [];
	const mark = (item: WorkItem): string =>
		`${item.identifier} · ${item.status} · ${item.health}${item.claim ? ` · held by ${item.claim.holder}` : ""}`;
	for (const item of dag.upstream) lines.push(`← blocks ${mark(item)}`);
	for (const item of dag.parents) lines.push(`↑ parent ${mark(item)}`);
	lines.push(`● selected ${lines.length === 0 ? "" : ""}${mark(dag.nodes.find((node) => node.role === "selected")?.item ?? ({ identifier: "?" } as WorkItem))}`);
	for (const item of dag.children) lines.push(`↓ child ${mark(item)}`);
	for (const item of dag.downstream) lines.push(`→ unblocks ${mark(item)}`);
	return lines;
}

export interface DagRenderOptions {
	scope: DagScope;
	onSelect: (id: string) => void;
}

/**
 * Render the graph: an SVG edge layer plus absolutely positioned buttons. Using
 * buttons (not SVG text) is what makes keyboard focus, screen readers and hit
 * targets work without extra code.
 */
export function renderDag(dag: LocalDag, options: DagRenderOptions): HTMLElement {
	const scoped = scopeDag(dag, options.scope);
	const layout = layoutDag(scoped);
	const wrapper = document.createElement("div");
	wrapper.className = "dag";
	wrapper.dataset.scope = options.scope;
	wrapper.dataset.empty = String(layout.empty);
	wrapper.style.setProperty("--dag-width", `${layout.width}px`);
	wrapper.style.setProperty("--dag-height", `${layout.height}px`);

	if (layout.empty) {
		const empty = document.createElement("p");
		empty.className = "muted";
		empty.textContent = "No upstream or downstream items in this view.";
		wrapper.append(empty);
		return wrapper;
	}

	const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
	svg.setAttribute("class", "dag-edges");
	svg.setAttribute("viewBox", `0 0 ${layout.width} ${layout.height}`);
	svg.setAttribute("width", String(layout.width));
	svg.setAttribute("height", String(layout.height));
	svg.setAttribute("aria-hidden", "true");
	svg.setAttribute("focusable", "false");
	const defs = document.createElementNS("http://www.w3.org/2000/svg", "defs");
	defs.innerHTML =
		'<marker id="dag-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">' +
		'<path d="M 0 0 L 10 5 L 0 10 z" class="dag-arrow-head"/></marker>';
	svg.append(defs);
	for (const edge of layout.edges) {
		const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
		path.setAttribute("d", edge.d);
		path.setAttribute("class", `dag-edge dag-edge-${edge.kind}`);
		path.dataset.kind = edge.kind;
		path.dataset.from = edge.from;
		path.dataset.to = edge.to;
		if (edge.kind === "dependency") path.setAttribute("marker-end", "url(#dag-arrow)");
		svg.append(path);
	}
	wrapper.append(svg);

	for (const node of layout.nodes) {
		const button = document.createElement("button");
		button.type = "button";
		button.className = "dag-node";
		button.dataset.itemId = node.id;
		button.dataset.role = node.role;
		button.dataset.status = node.item.status_category;
		button.style.left = `${node.x}px`;
		button.style.top = `${node.y}px`;
		button.style.width = `${node.w}px`;
		button.style.height = `${node.h}px`;
		const selected = node.role === "selected";
		button.dataset.selected = String(selected);
		button.setAttribute(
			"aria-label",
			`${node.item.identifier} ${node.item.title} — ${node.item.status}, ${node.item.health}${selected ? " (selected)" : ""}`,
		);
		if (selected) button.setAttribute("aria-current", "true");

		const id = document.createElement("span");
		id.className = "dag-node-id";
		id.textContent = node.item.identifier;
		const title = document.createElement("span");
		title.className = "dag-node-title";
		title.textContent = node.item.title;
		const meta = document.createElement("span");
		meta.className = "dag-node-meta";
		// Symbol + word, never colour alone.
		meta.textContent = `${node.role === "upstream" ? "← " : node.role === "downstream" ? "→ " : node.role === "parent" ? "↑ " : node.role === "child" ? "↓ " : "● "}${node.item.status}`;
		button.append(id, title, meta);
		button.addEventListener("click", () => options.onSelect(node.id));
		wrapper.append(button);
	}
	return wrapper;
}

/** Accessible text view; the buttons inside stay in sync with the graph. */
export function renderDagText(dag: LocalDag, options: DagRenderOptions): HTMLElement {
	const scoped = scopeDag(dag, options.scope);
	const list = document.createElement("ul");
	list.className = "dag-text";
	list.setAttribute("aria-label", "Local dependency text view");
	for (const line of dagTextLines(scoped)) {
		const li = document.createElement("li");
		li.textContent = line;
		list.append(li);
	}
	// The selected/related nodes are also reachable as buttons here, so keyboard
	// users never have to tab through absolute-positioned elements.
	const related = [...scoped.upstream, ...scoped.downstream, ...scoped.parents, ...scoped.children];
	for (const item of related) {
		const li = document.createElement("li");
		const button = document.createElement("button");
		button.type = "button";
		button.className = "ghost dag-text-select";
		button.dataset.itemId = item.id;
		button.textContent = `Select ${item.identifier}`;
		button.addEventListener("click", () => options.onSelect(item.id));
		li.append(button);
		list.append(li);
	}
	return list;
}

export type { TopologyGraph };
