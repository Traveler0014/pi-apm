/** Shared geometry types for the graph layout and viewport (APM-45). */

export type EdgeKind = "dependency" | "parent";
export type GraphRelation = "dependencies" | "hierarchy" | "combined";

export interface Point {
	x: number;
	y: number;
}

export interface LayoutNode {
	id: string;
	item: import("../types").WorkItem;
	role: "upstream" | "selected" | "downstream" | "parent" | "child";
	x: number;
	y: number;
	w: number;
	h: number;
}

export interface LayoutEdge {
	from: string;
	to: string;
	kind: EdgeKind;
	/** Dagre's control points, clipped to the node borders. */
	points: Point[];
	/** SVG path data built from the points. */
	d: string;
}

export interface GraphLayout {
	relation: GraphRelation;
	width: number;
	height: number;
	nodes: LayoutNode[];
	edges: LayoutEdge[];
	empty: boolean;
}

export interface ViewState {
	scale: number;
	/** Translation in CSS pixels, applied before scaling. */
	x: number;
	y: number;
	/** "fit" means the last explicit action was Fit; used by the caller/tests. */
	mode: "fit" | "manual";
}

export interface ViewportSize {
	width: number;
	height: number;
}
