/**
 * Dagre adapter (APM-45).
 *
 * Dagre owns the layered layout and the crossing minimisation; this module only
 * translates the local neighbourhood into a dagre graph and back into absolute
 * node rectangles plus clipped edge polylines. Rendering stays in our own DOM /
 * SVG so the nodes remain real buttons and the text equivalent stays authoritative.
 *
 * Two relation modes are laid out separately on purpose: dependencies are a
 * left-to-right work order, hierarchy is a top-to-bottom tree. Mixing them in one
 * graph (the legacy `combined` mode) can put a parent and a dependent in the same
 * rank, which is what produced reversed same-column edges.
 */

import dagre from "@dagrejs/dagre";
import type { LocalDag } from "../topology";
import type { WorkItem } from "../types";
import { MAX_LINE_UNITS, NODE_PADDING_X, UNIT_PX, titleUnits, wrapTitle } from "./text";
import type {
	EdgeKind,
	GraphLayout,
	GraphRelation,
	LayoutEdge,
	LayoutNode,
	Point,
} from "./types";

export const NODE_W = 208;
export { MAX_LINE_UNITS } from "./text";
/**
 * Node height must fit id + two title lines + meta inside a 6px/8px padded box.
 * At 11px/1.25 that is ~72px of content; 76 left no slack and the flex column
 * shrank the title block, clipping glyph descenders. 88 gives one line of slack
 * and keeps the box tight (APM-48).
 */
export const NODE_H = 88;
export const NODE_W_EXPANDED = 264;
export const NODE_H_EXPANDED = 104;
const PAD = 24;

export interface LayoutOptions {
	expanded?: boolean;
	/** Override for tests: keyword nodes get the same width. */
	fixedSize?: boolean;
	relation?: GraphRelation;
}

const ROLE_ORDER: Record<LayoutNode["role"], number> = {
	parent: -2,
	upstream: -1,
	selected: 0,
	downstream: 1,
	child: 2,
};

/**
 * Width grows with the title so two wrapped lines fit without mid-word cuts.
 *
 * The estimate is in Unicode width units (CJK ≈ 2 Latin letters), because a
 * Chinese title of the same character count is about twice as wide — sizing it
 * with `title.length` left the text overflowing its box.
 */
export function nodeSize(item: WorkItem, expanded = false, fixed = false): { w: number; h: number } {
	const minW = expanded ? NODE_W_EXPANDED : NODE_W;
	const h = expanded ? NODE_H_EXPANDED : NODE_H;
	if (fixed) return { w: minW, h };
	// Size to the *widest wrapped line*, not the raw length: two short Chinese
	// lines should not produce a box as wide as the whole title, and a title that
	// wraps into two full lines must not overflow the box either.
	const widest = Math.max(
		0,
		...wrapTitle(item.title, expanded ? MAX_LINE_UNITS + 4 : MAX_LINE_UNITS, 2).map((line) => titleUnits(line)),
	);
	const estimated = NODE_PADDING_X + widest * UNIT_PX;
	return { w: Math.min(expanded ? 360 : 320, Math.max(minW, Math.ceil(estimated))), h };
}

/** The nodes/edges a relation mode actually shows; combined is the legacy path. */
export function relationView(dag: LocalDag, relation: GraphRelation): LocalDag {
	if (relation === "combined") return dag;
	const roles = new Set(
		relation === "dependencies" ? ["selected", "upstream", "downstream"] : ["selected", "parent", "child"],
	);
	const kind = relation === "dependencies" ? "dependency" : "parent";
	const nodes = dag.nodes.filter((node) => roles.has(node.role));
	const included = new Set(nodes.map((node) => node.item.id));
	const edges = dag.edges.filter((edge) => edge.kind === kind && included.has(edge.from) && included.has(edge.to));
	return {
		...dag,
		nodes,
		edges,
		upstream: relation === "dependencies" ? dag.upstream : [],
		downstream: relation === "dependencies" ? dag.downstream : [],
		parents: relation === "hierarchy" ? dag.parents : [],
		children: relation === "hierarchy" ? dag.children : [],
	};
}

/** Clip a segment at the border of the source/target rectangles. */
function clipToBox(center: Point, toward: Point, w: number, h: number): Point {
	const dx = toward.x - center.x;
	const dy = toward.y - center.y;
	if (dx === 0 && dy === 0) return center;
	const scaleX = dx === 0 ? Number.POSITIVE_INFINITY : (w / 2) / Math.abs(dx);
	const scaleY = dy === 0 ? Number.POSITIVE_INFINITY : (h / 2) / Math.abs(dy);
	const t = Math.min(scaleX, scaleY, 1);
	return { x: center.x + dx * t, y: center.y + dy * t };
}

function polyline(points: Point[]): string {
	if (points.length === 0) return "";
	const [first, ...rest] = points;
	return `M ${first!.x.toFixed(2)} ${first!.y.toFixed(2)} ${rest
		.map((point) => `L ${point.x.toFixed(2)} ${point.y.toFixed(2)}`)
		.join(" ")}`;
}

/**
 * Lay out one relation view with dagre. Coordinates are top-left based and
 * deterministic for the same input (nodes and edges are sorted before insertion).
 */
export function layoutGraph(
	dag: LocalDag,
	options: LayoutOptions = {},
): GraphLayout {
	const relation = options.relation ?? "combined";
	const view = relationView(dag, relation);
	const expanded = options.expanded === true;
	const rankdir = relation === "hierarchy" ? "TB" : "LR";

	const graph = new dagre.graphlib.Graph({ multigraph: true });
	graph.setGraph({
		rankdir,
		nodesep: expanded ? 40 : 26,
		ranksep: expanded ? 90 : 64,
		marginx: expanded ? 28 : PAD,
		marginy: expanded ? 28 : PAD,
		ranker: "network-simplex",
	});
	graph.setDefaultEdgeLabel(() => ({}));

	const ordered = [...view.nodes].sort(
		(a, b) =>
			ROLE_ORDER[a.role] - ROLE_ORDER[b.role] ||
			a.item.number - b.item.number ||
			a.item.identifier.localeCompare(b.item.identifier),
	);
	const sizes = new Map<string, { w: number; h: number }>();
	for (const node of ordered) {
		const size = nodeSize(node.item, expanded, options.fixedSize);
		sizes.set(node.item.id, size);
		graph.setNode(node.item.id, { width: size.w, height: size.h, role: node.role });
	}

	const edges = [...view.edges].sort(
		(a, b) =>
			a.kind.localeCompare(b.kind) ||
			a.from.localeCompare(b.from) ||
			a.to.localeCompare(b.to),
	);
	for (const edge of edges) {
		if (!sizes.has(edge.from) || !sizes.has(edge.to)) continue;
		graph.setEdge(edge.from, edge.to, { kind: edge.kind }, `${edge.kind}:${edge.from}->${edge.to}`);
	}

	dagre.layout(graph);

	const nodes: LayoutNode[] = ordered.map((node) => {
		const laid = graph.node(node.item.id) as { x: number; y: number; width: number; height: number };
		const size = sizes.get(node.item.id)!;
		return {
			id: node.item.id,
			item: node.item,
			role: node.role,
			x: laid.x - size.w / 2,
			y: laid.y - size.h / 2,
			w: laid.width ?? size.w,
			h: laid.height ?? size.h,
		};
	});
	const byId = new Map(nodes.map((node) => [node.id, node]));

	const laidEdges: LayoutEdge[] = [];
	for (const edge of edges) {
		const from = byId.get(edge.from);
		const to = byId.get(edge.to);
		if (!from || !to) continue;
		const laid = graph.edge({ v: edge.from, w: edge.to, name: `${edge.kind}:${edge.from}->${edge.to}` }) as
			| { points?: Point[] }
			| undefined;
		const raw = laid?.points ?? [];
		const fromCenter = { x: from.x + from.w / 2, y: from.y + from.h / 2 };
		const toCenter = { x: to.x + to.w / 2, y: to.y + to.h / 2 };
		const start = clipToBox(fromCenter, raw[0] ?? toCenter, from.w, from.h);
		const end = clipToBox(toCenter, raw[raw.length - 1] ?? fromCenter, to.w, to.h);
		const points = [start, ...raw, end];
		laidEdges.push({ from: edge.from, to: edge.to, kind: edge.kind as EdgeKind, points, d: polyline(points) });
	}

	const graphLabel = graph.graph() as { width?: number; height?: number };
	const maxX = Math.max(0, ...nodes.map((node) => node.x + node.w));
	const maxY = Math.max(0, ...nodes.map((node) => node.y + node.h));
	const pad = expanded ? 28 : PAD;
	const width = Math.max(1, Math.ceil(graphLabel.width ?? maxX + pad));
	const height = Math.max(1, Math.ceil(graphLabel.height ?? maxY + pad));

	return { relation, width, height, nodes, edges: laidEdges, empty: nodes.length === 0 };
}

/** Convenience wrappers for the two product modes. */
export function layoutDependencies(dag: LocalDag, options: Omit<LayoutOptions, "relation"> = {}): GraphLayout {
	return layoutGraph(dag, { ...options, relation: "dependencies" });
}

export function layoutHierarchy(dag: LocalDag, options: Omit<LayoutOptions, "relation"> = {}): GraphLayout {
	return layoutGraph(dag, { ...options, relation: "hierarchy" });
}
