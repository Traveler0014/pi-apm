/**
 * Local dependency graph adapter (APM-45).
 *
 * Layout is delegated to dagre (see `graph/layout.ts`); this module keeps the
 * public surface used by the topology view while adding two relation modes:
 *
 *   - `dependencies` — left-to-right work order (dagre layered, crossing
 *     minimisation);
 *   - `hierarchy`    — top-to-bottom parent/child tree;
 *   - `combined`     — the legacy single picture (kept for existing callers).
 *
 * Rendering uses a zoomable viewport (`graph/viewport.ts`): SVG edges that are
 * `aria-hidden` and real `<button>` nodes, so keyboard and screen readers keep
 * working. `renderDag` remains a thin compatibility wrapper; new integrations
 * should prefer `createGraphView` + `layoutDependencies`/`layoutHierarchy`.
 */

import { createGraphView, type GraphViewHandle, type GraphViewOptions } from "./graph/viewport";
import { layoutGraph, NODE_H, NODE_W, nodeSize } from "./graph/layout";
import { MAX_LINE_UNITS, titleUnits, wrapTitle } from "./graph/text";
import type { GraphLayout, GraphRelation, LayoutEdge, LayoutNode } from "./graph/types";
import type { LocalDag } from "./topology";
import type { WorkItem } from "./types";

export { NODE_H, NODE_W };
export const GAP_X = 56;
export const GAP_Y = 22;
/** @deprecated kept for callers that imported the old layout constants. */
export const NODE_W_EXPANDED = 264;
export const NODE_H_EXPANDED = 96;

export type DagRole = LayoutNode["role"];
export type LaidOutNode = LayoutNode;
export type LaidOutEdge = LayoutEdge;
export type DagLayout = GraphLayout;
export type DagRelation = GraphRelation;

export { MAX_LINE_UNITS, titleUnits, wrapTitle };
export { createGraphView, fitView, layoutDependencies, layoutHierarchy, relationView } from "./graph";
export { localDagFor, type DagRelation as DagRelationMode } from "./topology";
export type { GraphViewHandle, GraphViewOptions };

export type DagScope = "all" | "blocking" | "upstream" | "downstream";

export interface LayoutDagOptions {
	expanded?: boolean;
	relation?: GraphRelation;
	/** Tests can pin every node to NODE_W for geometry assertions. */
	fixedSize?: boolean;
}

/** Lay out the current neighbourhood (legacy signature: options are optional). */
export function layoutDag(dag: LocalDag, options: LayoutDagOptions = {}): DagLayout {
	return layoutGraph(dag, {
		expanded: options.expanded === true,
		relation: options.relation ?? "combined",
		fixedSize: options.fixedSize,
	});
}

/**
 * Apply a scope to an already-built neighbourhood:
 *   - `blocking`: dependency edges/nodes only (hide hierarchy);
 *   - `upstream`: the selected item plus what blocks it;
 *   - `downstream`: the selected item plus what it unblocks;
 *   - `all`: everything (default).
 *
 * A hierarchy-only neighbourhood ignores the dependency scopes: it has no
 * blocking chain to narrow, and hiding the tree would leave a lone node.
 */
export function scopeDag(dag: LocalDag, scope: DagScope): LocalDag {
	if (scope === "all") return dag;
	if (dag.upstream.length === 0 && dag.downstream.length === 0 && (dag.parents.length > 0 || dag.children.length > 0)) {
		return dag;
	}
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

/**
 * Text lines for the accessible list: same nodes, same relations, no geometry.
 * Each item appears once even when it is both a parent and an upstream blocker.
 */
export function dagTextLines(dag: LocalDag): string[] {
	const lines: string[] = [];
	const seen = new Set<string>();
	const mark = (item: WorkItem): string =>
		`${item.identifier} · ${item.status} · ${item.health}${item.claim ? ` · held by ${item.claim.holder}` : ""}`;
	const add = (line: string, item: WorkItem): void => {
		if (seen.has(item.id)) return;
		seen.add(item.id);
		lines.push(line);
	};
	for (const item of dag.upstream) add(`← blocks ${mark(item)}`, item);
	for (const item of dag.parents) add(`↑ parent ${mark(item)}`, item);
	const selected = dag.nodes.find((node) => node.role === "selected")?.item;
	if (selected) add(`● selected ${mark(selected)}`, selected);
	for (const item of dag.children) add(`↓ child ${mark(item)}`, item);
	for (const item of dag.downstream) add(`→ unblocks ${mark(item)}`, item);
	return lines;
}

export interface DagRenderOptions {
	scope: DagScope;
	onSelect: (id: string) => void;
	/** Overlay view: larger boxes and gaps, same layout algorithm. */
	expanded?: boolean;
	relation?: GraphRelation;
	/** Optional externally held viewport state (Fit/100%/pan survive re-renders). */
	view?: GraphViewOptions["view"];
	onViewChange?: GraphViewOptions["onViewChange"];
}

/**
 * Render the graph: an SVG edge layer plus absolutely positioned buttons.
 * Using buttons (not SVG text) is what makes keyboard focus, screen readers and
 * hit targets work without extra code. The returned element is a viewport; keep
 * the handle when the caller wants the Fit/zoom API and persistent view state.
 */
export function renderDag(dag: LocalDag, options: DagRenderOptions): HTMLElement {
	return graphViewFor(dag, options).element;
}

/** Same as `renderDag`, but exposes the viewport handle for Fit/zoom state. */
export function graphViewFor(dag: LocalDag, options: DagRenderOptions): GraphViewHandle {
	const scoped = scopeDag(dag, options.scope);
	const relation = options.relation ?? "combined";
	const layout = layoutGraph(scoped, { expanded: options.expanded, relation });
	const handle = createGraphView({
		relation,
		expanded: options.expanded,
		view: options.view,
		onViewChange: options.onViewChange,
		onSelect: options.onSelect,
	});
	handle.element.dataset.scope = options.scope;
	const selectedId = scoped.nodes.find((node) => node.role === "selected")?.item.id ?? null;
	handle.update(layout, selectedId);
	return handle;
}

/** Accessible text view; the buttons inside stay in sync with the graph. */
export function renderDagText(dag: LocalDag, options: { scope: DagScope; onSelect: (id: string) => void }): HTMLElement {
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
	// users never have to tab through absolute-positioned elements. Deduplicate:
	// an item can be both a parent and an upstream blocker.
	const related = [...scoped.upstream, ...scoped.downstream, ...scoped.parents, ...scoped.children];
	const seen = new Set<string>();
	for (const item of related) {
		if (seen.has(item.id)) continue;
		seen.add(item.id);
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

/** Node size helper re-exported for the integration tests (title-aware width). */
export { nodeSize };
