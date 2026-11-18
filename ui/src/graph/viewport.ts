/**
 * Zoomable graph viewport (APM-45).
 *
 * The viewport owns Fit / 100% / +/- / pan and the CSS transform only. Nodes
 * stay real `<button>` elements and the edge SVG stays `aria-hidden`, so the
 * existing keyboard and screen-reader behaviour is preserved when the graph is
 * embedded. The caller can read and pass back `ViewState` at any time; updating
 * the data (same selection) never resets the zoom or pan.
 */

import type { EdgeKind, GraphLayout, GraphRelation, LayoutNode, ViewState, ViewportSize } from "./types";
import { MAX_LINE_UNITS, wrapTitle } from "./text";

export const DEFAULT_PADDING = 24;
export const MIN_SCALE = 0.15;
export const MAX_SCALE = 3;

function clamp(value: number, min: number, max: number): number {
	return Math.min(max, Math.max(min, value));
}

function round(value: number): number {
	return Math.round(value * 1000) / 1000;
}

/**
 * Pure Fit calculation: the layout is centred and scaled down until it fits the
 * viewport minus padding (never below MIN_SCALE). A zero-sized viewport (server
 * rendering, happy-dom) falls back to the layout size so the result is stable.
 */
export function fitView(layout: GraphLayout, size: ViewportSize, padding = DEFAULT_PADDING): ViewState {
	const width = size.width > 0 ? size.width : layout.width;
	const height = size.height > 0 ? size.height : layout.height;
	const usableW = Math.max(1, width - padding * 2);
	const usableH = Math.max(1, height - padding * 2);
	const scale = clamp(Math.min(usableW / layout.width, usableH / layout.height, 1.5), MIN_SCALE, MAX_SCALE);
	return {
		scale: round(scale),
		x: round((width - layout.width * scale) / 2),
		y: round((height - layout.height * scale) / 2),
		mode: "fit",
	};
}

export interface GraphViewOptions {
	relation?: GraphRelation;
	expanded?: boolean;
	/** Initial view; when omitted the first layout is fitted. */
	view?: ViewState;
	onViewChange?: (view: ViewState) => void;
	onSelect?: (id: string) => void;
	padding?: number;
	label?: string;
}

export interface GraphViewHandle {
	element: HTMLElement;
	/** Rebuild the picture from new data; keeps the current zoom/pan. */
	update(layout: GraphLayout, selectedId?: string | null): void;
	getViewState(): ViewState;
	setViewState(view: ViewState): void;
	fit(): void;
	/** 100% (scale 1), centred. */
	resetZoom(): void;
	zoomBy(factor: number, origin?: { x: number; y: number }): void;
}

function button(className: string, text: string, ariaLabel: string): HTMLButtonElement {
	const node = document.createElement("button");
	node.type = "button";
	node.className = className;
	node.textContent = text;
	node.setAttribute("aria-label", ariaLabel);
	return node;
}

export function createGraphView(options: GraphViewOptions = {}): GraphViewHandle {
	const root = document.createElement("div");
	root.className = "dag dag-viewport";
	root.tabIndex = 0;
	root.setAttribute("role", "group");
	root.setAttribute("aria-label", options.label ?? "Local dependency graph");
	root.dataset.relation = options.relation ?? "combined";
	if (options.expanded) root.dataset.expanded = "true";

	const toolbar = document.createElement("div");
	toolbar.className = "dag-viewport-toolbar";
	const fitButton = button("dag-fit ghost", "Fit", "Fit graph to the viewport");
	const resetButton = button("dag-zoom-reset ghost", "100%", "Reset zoom to 100%");
	const zoomOut = button("dag-zoom-out ghost", "−", "Zoom out");
	const zoomIn = button("dag-zoom-in ghost", "+", "Zoom in");
	const level = document.createElement("span");
	level.className = "dag-zoom-level";
	level.setAttribute("aria-live", "polite");
	toolbar.append(fitButton, zoomOut, level, zoomIn, resetButton);

	const canvas = document.createElement("div");
	canvas.className = "dag-canvas";
	const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
	svg.setAttribute("class", "dag-edges");
	svg.setAttribute("aria-hidden", "true");
	svg.setAttribute("focusable", "false");
	const nodesLayer = document.createElement("div");
	nodesLayer.className = "dag-nodes";
	canvas.append(svg, nodesLayer);
	const empty = document.createElement("p");
	empty.className = "muted dag-empty";
	empty.textContent =
		(options.relation ?? "combined") === "hierarchy"
			? "No parent or child items for this selection."
			: "No upstream or downstream items in this view.";
	empty.hidden = true;
	root.append(toolbar, canvas, empty);

	let layout: GraphLayout | null = null;
	let view: ViewState = options.view ? { ...options.view } : { scale: 1, x: 0, y: 0, mode: "manual" };
	let hasView = Boolean(options.view);
	let selectedId: string | null = null;

	const emit = (): void => {
		options.onViewChange?.({ ...view });
	};

	const apply = (): void => {
		canvas.style.transform = `translate(${view.x}px, ${view.y}px) scale(${view.scale})`;
		level.textContent = `${Math.round(view.scale * 100)}%`;
		root.dataset.zoom = view.scale < 0.55 ? "far" : "normal";
		root.style.setProperty("--dag-scale", String(view.scale));
	};

	const setView = (next: ViewState, notify = true): void => {
		view = { ...next, scale: round(clamp(next.scale, MIN_SCALE, MAX_SCALE)) };
		hasView = true;
		apply();
		if (notify) emit();
	};

	const viewportSize = (): ViewportSize => ({
		width: root.clientWidth,
		height: root.clientHeight,
	});

	const fit = (): void => {
		if (!layout) return;
		setView(fitView(layout, viewportSize(), options.padding ?? DEFAULT_PADDING));
	};

	const resetZoom = (): void => {
		if (!layout) return;
		const size = viewportSize();
		const width = size.width > 0 ? size.width : layout.width;
		const height = size.height > 0 ? size.height : layout.height;
		setView({
			scale: 1,
			x: round((width - layout.width) / 2),
			y: round((height - layout.height) / 2),
			mode: "manual",
		});
	};

	const zoomAt = (factor: number, origin?: { x: number; y: number }): void => {
		const nextScale = round(clamp(view.scale * factor, MIN_SCALE, MAX_SCALE));
		if (nextScale === view.scale) return;
		const rect = root.getBoundingClientRect();
		const px = origin?.x ?? (rect.width > 0 ? rect.width / 2 : 0);
		const py = origin?.y ?? (rect.height > 0 ? rect.height / 2 : 0);
		const ratio = nextScale / view.scale;
		setView({
			scale: nextScale,
			x: round(px - (px - view.x) * ratio),
			y: round(py - (py - view.y) * ratio),
			mode: "manual",
		});
	};

	const renderNodes = (): void => {
		if (!layout) return;
		nodesLayer.replaceChildren();
		for (const node of layout.nodes) {
			nodesLayer.append(renderNode(node, node.id === selectedId, options.onSelect));
		}
	};

	const renderEdges = (): void => {
		if (!layout) return;
		svg.replaceChildren();
		svg.setAttribute("viewBox", `0 0 ${layout.width} ${layout.height}`);
		svg.setAttribute("width", String(layout.width));
		svg.setAttribute("height", String(layout.height));
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
	};

	const update = (next: GraphLayout, nextSelected: string | null = selectedId): void => {
		layout = next;
		selectedId = nextSelected;
		root.dataset.empty = String(next.empty);
		empty.hidden = !next.empty;
		canvas.hidden = next.empty;
		root.style.setProperty("--dag-width", `${next.width}px`);
		root.style.setProperty("--dag-height", `${next.height}px`);
		renderEdges();
		renderNodes();
		if (!hasView && !next.empty) {
			fit();
			hasView = true;
		} else {
			apply();
		}
	};

	const onKey = (event: KeyboardEvent): void => {
		if (event.metaKey || event.ctrlKey || event.altKey) return;
		const key = event.key;
		if (key === "+" || key === "=") zoomAt(1.2);
		else if (key === "-" || key === "_") zoomAt(1 / 1.2);
		else if (key === "0") resetZoom();
		else if (key === "f" || key === "F") fit();
		else if (key === "ArrowLeft") setView({ ...view, x: view.x + 32, mode: "manual" });
		else if (key === "ArrowRight") setView({ ...view, x: view.x - 32, mode: "manual" });
		else if (key === "ArrowUp") setView({ ...view, y: view.y + 32, mode: "manual" });
		else if (key === "ArrowDown") setView({ ...view, y: view.y - 32, mode: "manual" });
		else return;
		event.preventDefault();
	};

	let drag: { x: number; y: number } | null = null;
	const onPointerDown = (event: PointerEvent): void => {
		const target = event.target as HTMLElement;
		// The toolbar and any form control own the pointer: a Fit/100%/+/- click
		// must never start a pan drag.
		if (target.closest(".dag-node, .dag-viewport-toolbar, button, a, input, select, textarea")) return;
		drag = { x: event.clientX - view.x, y: event.clientY - view.y };
		root.setPointerCapture?.(event.pointerId);
	};
	const onPointerMove = (event: PointerEvent): void => {
		if (!drag) return;
		setView({ ...view, x: round(event.clientX - drag.x), y: round(event.clientY - drag.y), mode: "manual" });
	};
	const onPointerUp = (): void => {
		drag = null;
	};

	root.addEventListener("keydown", onKey);
	root.addEventListener("pointerdown", onPointerDown);
	root.addEventListener("pointermove", onPointerMove);
	root.addEventListener("pointerup", onPointerUp);
	root.addEventListener("pointercancel", onPointerUp);
	root.addEventListener(
		"wheel",
		(event: WheelEvent) => {
			if (layout?.empty) return;
			event.preventDefault();
			const rect = root.getBoundingClientRect();
			zoomAt(Math.exp(-event.deltaY * 0.0015), { x: event.clientX - rect.left, y: event.clientY - rect.top });
		},
		{ passive: false },
	);
	fitButton.addEventListener("click", fit);
	resetButton.addEventListener("click", resetZoom);
	zoomIn.addEventListener("click", () => zoomAt(1.2));
	zoomOut.addEventListener("click", () => zoomAt(1 / 1.2));

	return {
		element: root,
		update,
		getViewState: () => ({ ...view }),
		setViewState: (next) => setView(next),
		fit,
		resetZoom,
		zoomBy: (factor, origin) => zoomAt(factor, origin),
	};
}

function renderNode(node: LayoutNode, selected: boolean, onSelect?: (id: string) => void): HTMLButtonElement {
	const item = node.item;
	const element = document.createElement("button");
	element.type = "button";
	element.className = "dag-node";
	element.dataset.itemId = node.id;
	element.dataset.role = node.role;
	element.dataset.status = item.status_category;
	element.style.left = `${node.x}px`;
	element.style.top = `${node.y}px`;
	element.style.width = `${node.w}px`;
	element.style.height = `${node.h}px`;
	element.dataset.selected = String(selected);
	element.setAttribute(
		"aria-label",
		`${item.identifier} ${item.title} — ${item.status}, ${item.health}${selected ? " (selected)" : ""}`,
	);
	if (selected) element.setAttribute("aria-current", "true");

	const id = document.createElement("span");
	id.className = "dag-node-id";
	id.textContent = item.identifier;
	const title = document.createElement("span");
	title.className = "dag-node-title";
	title.title = item.title;
	for (const line of wrapTitle(item.title, MAX_LINE_UNITS)) {
		const span = document.createElement("span");
		span.className = "dag-node-title-line";
		span.textContent = line;
		title.append(span);
	}
	const meta = document.createElement("span");
	meta.className = "dag-node-meta";
	const glyph =
		node.role === "upstream" ? "← " : node.role === "downstream" ? "→ " : node.role === "parent" ? "↑ " : node.role === "child" ? "↓ " : "● ";
	meta.textContent = `${glyph}${item.status}`;
	element.append(id, title, meta);
	if (onSelect) element.addEventListener("click", () => onSelect(node.id));
	return element;
}

export type { EdgeKind };
