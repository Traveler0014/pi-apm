/**
 * DOM rendering for the dashboard shell.
 *
 * The shell is built once and only the dynamic regions are rewritten, because a
 * full re-render would steal focus and caret position from the search box on
 * every 4-second refresh (and would collapse the done fold). All text goes
 * through `textContent`, so ledger data can never be interpreted as markup.
 *
 * Two views share one data model: a flat list and a dependency topology
 * ("ready now" vs "waiting upstream"). Both show the item's own workflow stage
 * and acceptance progress, and both keep the user's selection across refreshes.
 */

import {
	BUCKETS,
	type ConnectionView,
	type Metric,
	type QueueModel,
	type QueuedItem,
	type SortKey,
	type ViewState,
	bucketMeta,
	clockTime,
	dataAge,
	relativeTime,
} from "./transform";
import { renderDag, renderDagText } from "./dag";
import { type DagScope, localDag, type TopologyGraph, type TopologySummary } from "./topology";
import type { BucketKey, Dashboard, WorkItem } from "./types";
import { acceptanceProgress, categoryMark, stageBadge, type StageModel } from "./workflow";

export type ViewMode = "list" | "topology";

export interface RenderContext {
	state: ViewState;
	view: ViewMode;
	selectedId: string | null;
	stageOf: (item: WorkItem) => StageModel;
	/** Acceptance progress, available once the detail panel has loaded. */
	acceptanceOf: (item: WorkItem) => ReturnType<typeof acceptanceProgress>;
	now: number;
}

export interface Shell {
	root: HTMLElement;
	connection: HTMLElement;
	dataTime: HTMLElement;
	refreshButton: HTMLButtonElement;
	metrics: HTMLElement;
	banner: HTMLElement;
	viewToggle: HTMLElement;
	filters: HTMLElement;
	search: HTMLInputElement;
	sort: HTMLSelectElement;
	listView: HTMLElement;
	topologyView: HTMLElement;
	topologyGroups: HTMLElement;
	dagPanel: HTMLElement;
	summary: HTMLElement;
	queue: HTMLElement;
	queueCaption: HTMLElement;
	doneFold: HTMLDetailsElement;
	doneSummary: HTMLElement;
	doneList: HTMLElement;
	empty: HTMLElement;
	onRefresh: (handler: () => void) => void;
	onStateChange: (handler: (next: Partial<ViewState>) => void) => void;
	onViewChange: (handler: (view: ViewMode) => void) => void;
	onSelect: (handler: (id: string | null) => void) => void;
	onDagScope: (handler: (scope: DagScope) => void) => void;
	onOpenItem: (handler: (id: string) => void) => void;
}

function el<K extends keyof HTMLElementTagNameMap>(
	tag: K,
	props: { className?: string; text?: string; attrs?: Record<string, string> } = {},
): HTMLElementTagNameMap[K] {
	const node = document.createElement(tag);
	if (props.className) node.className = props.className;
	if (props.text !== undefined) node.textContent = props.text;
	for (const [key, value] of Object.entries(props.attrs ?? {})) node.setAttribute(key, value);
	return node;
}

const SORT_LABELS: Array<[SortKey, string]> = [
	["queue", "Queue order (most urgent first)"],
	["priority", "Priority"],
	["updated", "Recently updated"],
	["progress", "Longest without progress"],
	["identifier", "Identifier"],
];

/** Build the static shell once; returns handles for incremental updates. */
export function createShell(root: HTMLElement): Shell {
	root.replaceChildren();
	const shellRoot = el("div", { className: "dashboard" });

	// --- header -----------------------------------------------------------
	const header = el("header", { className: "topbar", attrs: { role: "banner" } });
	const brand = el("div", { className: "brand" });
	brand.append(el("span", { className: "brand-name", text: "AgentPM" }));
	const project = el("span", { className: "project", text: "…" });
	project.setAttribute("data-role", "project");
	brand.append(project);

	const meta = el("div", { className: "meta" });
	const connection = el("span", { className: "connection", attrs: { role: "status", "aria-live": "polite" } });
	const dataTime = el("span", { className: "data-time" });
	const refresh = el("button", { className: "refresh", text: "Refresh", attrs: { type: "button", title: "Refresh now (R)" } });
	refresh.append(" ", el("kbd", { text: "R" }));
	meta.append(connection, dataTime, refresh);
	header.append(brand, meta);

	// --- metrics ----------------------------------------------------------
	const metrics = el("section", { className: "metrics", attrs: { "aria-label": "Summary" } });

	// --- banner (offline / stale) ----------------------------------------
	const banner = el("p", { className: "banner", attrs: { role: "status", "aria-live": "polite", hidden: "" } });

	// --- controls ---------------------------------------------------------
	const controls = el("section", { className: "controls", attrs: { "aria-label": "View, filter and sort" } });
	const viewToggle = el("div", { className: "views", attrs: { role: "group", "aria-label": "View mode" } });
	for (const [mode, label] of [
		["list", "List"],
		["topology", "Topology"],
	] as Array<[ViewMode, string]>) {
		const button = el("button", { className: "view", text: label, attrs: { type: "button", "aria-pressed": "false" } });
		button.dataset.view = mode;
		viewToggle.append(button);
	}
	const filters = el("div", { className: "filters", attrs: { role: "group", "aria-label": "Filter by queue" } });

	const searchWrap = el("label", { className: "search" });
	searchWrap.append(el("span", { className: "label", text: "Search" }));
	const search = el("input", {
		attrs: {
			type: "search",
			placeholder: "id, title, label, holder…",
			"aria-label": "Search work items",
			autocomplete: "off",
			spellcheck: "false",
		},
	}) as HTMLInputElement;
	searchWrap.append(search);

	const sortWrap = el("label", { className: "sort" });
	sortWrap.append(el("span", { className: "label", text: "Sort" }));
	const sort = el("select", { attrs: { "aria-label": "Sort work items" } }) as HTMLSelectElement;
	for (const [value, label] of SORT_LABELS) {
		const option = el("option", { text: label });
		option.value = value;
		sort.append(option);
	}
	sortWrap.append(sort);
	controls.append(viewToggle, filters, searchWrap, sortWrap);

	// --- list view --------------------------------------------------------
	const main = el("main", { className: "content" });
	const listView = el("section", { className: "list-view", attrs: { "aria-label": "Main queue" } });
	const queueCaption = el("h2", { className: "section-title" });
	const queue = el("div", { className: "queue", attrs: { role: "list" } });
	const empty = el("p", { className: "empty", text: "Nothing matches the current filters.", attrs: { hidden: "" } });
	const doneFold = el("details", { className: "done-fold" }) as HTMLDetailsElement;
	const doneSummary = el("summary", { className: "done-summary" });
	const doneList = el("div", { className: "queue done-list", attrs: { role: "list" } });
	doneFold.append(doneSummary, doneList);
	listView.append(queueCaption, queue, empty, doneFold);

	// --- topology view ----------------------------------------------------
	const topologyView = el("section", { className: "topology-view", attrs: { "aria-label": "Dependency topology" } });
	const topologyGroups = el("div", { className: "topology-groups" });
	const dagPanel = el("section", { className: "dag-panel", attrs: { "aria-label": "Local dependency graph" } });
	const summary = el("aside", { className: "selection-summary", attrs: { "aria-label": "Selected item" } });
	topologyView.append(topologyGroups, dagPanel, summary);

	main.append(listView, topologyView);
	shellRoot.append(header, metrics, banner, controls, main);
	root.append(shellRoot);

	let selectionHandler: ((id: string | null) => void) | null = null;

	const selectFromEvent = (event: Event): void => {
		const target = (event.target as HTMLElement).closest<HTMLElement>("[data-item-id]");
		if (!target?.dataset.itemId) return;
		selectionHandler?.(target.dataset.itemId);
	};

	const shell: Shell = {
		root: shellRoot,
		connection,
		dataTime,
		refreshButton: refresh,
		metrics,
		banner,
		viewToggle,
		filters,
		search,
		sort,
		listView,
		topologyView,
		topologyGroups,
		dagPanel,
		summary,
		queue,
		queueCaption,
		doneFold,
		doneSummary,
		doneList,
		empty,
		onRefresh(handler) {
			refresh.addEventListener("click", handler);
			document.addEventListener("keydown", (event) => {
				if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey) return;
				const target = event.target as HTMLElement | null;
				if (target && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName)) return;
				if (event.key === "r" || event.key === "R") {
					event.preventDefault();
					handler();
				}
			});
		},
		onStateChange(handler) {
			search.addEventListener("input", () => handler({ search: search.value }));
			sort.addEventListener("change", () => handler({ sort: sort.value as SortKey }));
		},
		onViewChange(handler) {
			viewToggle.addEventListener("click", (event) => {
				const button = (event.target as HTMLElement).closest<HTMLElement>("[data-view]");
				if (button?.dataset.view) handler(button.dataset.view as ViewMode);
			});
		},
		onSelect(handler) {
			selectionHandler = handler;
			// Clicking a row/card selects it; the summary panel is the anchor.
			queue.addEventListener("click", selectFromEvent);
			topologyGroups.addEventListener("click", selectFromEvent);
			queue.addEventListener("keydown", (event) => {
				if (event.key !== "Enter" && event.key !== " ") return;
				const row = (event.target as HTMLElement).closest<HTMLElement>("[data-item-id]");
				if (!row) return;
				event.preventDefault();
				handler(row.dataset.itemId ?? null);
			});
			topologyGroups.addEventListener("keydown", (event) => {
				if (event.key !== "Enter" && event.key !== " ") return;
				const card = (event.target as HTMLElement).closest<HTMLElement>("[data-item-id]");
				if (!card) return;
				event.preventDefault();
				handler(card.dataset.itemId ?? null);
			});
		},
		onDagScope(handler) {
			topologyView.addEventListener("click", (event) => {
				const button = (event.target as HTMLElement).closest<HTMLElement>("[data-scope]");
				if (button?.dataset.scope) handler(button.dataset.scope as DagScope);
			});
		},
		onOpenItem(handler) {
			const open = (event: Event): void => {
				const target = event.target as HTMLElement;
				const button = target.closest<HTMLElement>('[data-action="open-detail"]');
				if (!button) return;
				event.stopPropagation();
				const row = button.closest<HTMLElement>("[data-item-id]");
				if (row?.dataset.itemId) handler(row.dataset.itemId);
			};
			queue.addEventListener("click", open);
			// The closed fold holds done/cancelled/aborted rows: their Details buttons
			// must open the same full drawer as an open item's.
			doneList.addEventListener("click", open);
			topologyGroups.addEventListener("click", open);
			dagPanel.addEventListener("click", open);
		},
	};
	return shell;
}

export function updateProject(shell: Shell, project: string): void {
	const node = shell.root.querySelector('[data-role="project"]');
	if (node) node.textContent = project;
}

export function updateHeader(
	shell: Shell,
	input: { connection: ConnectionView; generatedAt: string | null; now: number; refreshing: boolean },
): void {
	shell.connection.textContent = `${input.connection.label} · ${input.connection.detail}`;
	shell.connection.dataset.state = input.connection.state;
	shell.connection.dataset.cached = String(input.connection.cached);
	shell.dataTime.textContent = input.generatedAt
		? `data ${clockTime(input.generatedAt)} (${dataAge(input.generatedAt, input.now).text})`
		: "no data yet";
	shell.dataTime.dataset.old = String(!input.generatedAt || dataAge(input.generatedAt, input.now).old);
	shell.refreshButton.disabled = input.refreshing;
	shell.refreshButton.setAttribute("aria-busy", String(input.refreshing));
}

export function updateMetrics(shell: Shell, metrics: Metric[]): void {
	shell.metrics.replaceChildren(
		...metrics.map((metric) => {
			const card = el("div", { className: "metric" });
			card.dataset.tone = metric.tone ?? "muted";
			card.append(
				el("span", { className: "metric-value", text: String(metric.value) }),
				el("span", { className: "metric-label", text: metric.label }),
			);
			if (metric.hint) card.title = metric.hint;
			return card;
		}),
	);
}

export function updateBanner(shell: Shell, connection: ConnectionView): void {
	if (!connection.cached && connection.state !== "offline") {
		shell.banner.hidden = true;
		shell.banner.textContent = "";
		return;
	}
	shell.banner.hidden = false;
	shell.banner.dataset.state = connection.state;
	shell.banner.textContent =
		connection.state === "offline"
			? `Offline — showing ${connection.detail}. Filters, the done fold and the selected item are kept; retrying every few seconds.`
			: `Showing cached data — ${connection.detail}.`;
}

export function updateFilters(shell: Shell, state: ViewState, dashboard: Dashboard | null): void {
	const active = new Set(state.buckets);
	const counts = new Map<BucketKey, number>();
	for (const bucket of BUCKETS) counts.set(bucket.key, (dashboard?.buckets[bucket.key] ?? []).length);
	shell.filters.replaceChildren(
		...BUCKETS.filter((bucket) => bucket.key !== "recently_done").flatMap((bucket) => {
			const count = counts.get(bucket.key) ?? 0;
			if (count === 0 && !active.has(bucket.key)) return [];
			const button = el("button", {
				className: "chip",
				attrs: { type: "button", "aria-pressed": String(active.has(bucket.key)), title: `${bucket.label} (${count})` },
			});
			button.dataset.bucket = bucket.key;
			button.dataset.tone = bucket.tone;
			button.append(
				el("span", { className: "chip-symbol", text: bucket.symbol }),
				el("span", { className: "chip-label", text: bucket.label }),
				el("span", { className: "chip-count", text: String(count) }),
			);
			return [button];
		}),
	);
	if (shell.search.value !== state.search) shell.search.value = state.search;
	if (shell.sort.value !== state.sort) shell.sort.value = state.sort;
	shell.filters.dataset.active = state.buckets.join(",");
}

export function updateViews(shell: Shell, view: ViewMode, selectedId: string | null): void {
	for (const button of shell.viewToggle.querySelectorAll<HTMLButtonElement>("[data-view]")) {
		const active = button.dataset.view === view;
		button.setAttribute("aria-pressed", String(active));
	}
	shell.listView.hidden = view !== "list";
	shell.topologyView.hidden = view !== "topology";
	if (selectedId) shell.topologyView.dataset.selected = selectedId;
}

// ---------------------------------------------------------------------------
// List view
// ---------------------------------------------------------------------------

export function updateQueue(shell: Shell, model: QueueModel, ctx: RenderContext): void {
	shell.queueCaption.textContent = `Main queue · ${model.items.length}${
		model.filtered > 0 ? ` of ${model.total} (+${model.filtered} filtered out)` : ""
	}`;
	shell.queue.replaceChildren(...model.items.map((row) => renderRow(row, ctx)));
	shell.empty.hidden = model.items.length > 0;

	shell.doneFold.hidden = model.closedTotal === 0;
	shell.doneFold.open = ctx.state.showRecentlyDone;
	// Every closed category lives here (done/cancelled/aborted/custom), not just
	// the dashboard's "recently done" window.
	shell.doneSummary.textContent = `Closed items · ${model.closedTotal}${
		model.closed.length !== model.closedTotal ? ` (${model.closed.length} shown)` : ""
	}`;
	shell.doneList.replaceChildren(...(ctx.state.showRecentlyDone ? model.closed.map((row) => renderRow(row, ctx)) : []));
}

function renderRow(row: QueuedItem, ctx: RenderContext): HTMLElement {
	const item = row.item;
	const stage = ctx.stageOf(item);
	const selected = ctx.selectedId === item.id;
	const node = el("article", {
		className: "row",
		attrs: {
			role: "listitem",
			tabindex: "0",
			"aria-label": `${item.identifier}: ${item.title}`,
			"aria-haspopup": "dialog",
			"aria-current": selected ? "true" : "false",
		},
	});
	node.dataset.itemId = item.id;
	node.dataset.priority = item.priority;
	node.dataset.health = item.health;
	node.dataset.selected = String(selected);

	const head = el("div", { className: "row-head" });
	head.append(el("span", { className: "identifier", text: item.identifier }), el("span", { className: "title", text: item.title }));

	const badges = el("div", { className: "badges" });
	for (const badge of row.badges) {
		const chip = el("span", { className: "badge", text: `${badge.symbol} ${badge.label}` });
		chip.dataset.tone = badge.tone;
		chip.dataset.bucket = badge.key;
		badges.append(chip);
	}
	if (item.waiting_on_human) badges.append(tagged("⏸ waiting human", "warn", "waiting_human"));
	if (item.claim && item.claim.status === "active") {
		const expired = item.claim.expired ? " (lease expired)" : "";
		badges.append(tagged(`✋ ${item.claim.holder}${expired}`, item.claim.expired ? "danger" : "muted", "claim"));
	}
	head.append(badges);

	const details = el("div", { className: "row-detail" });
	details.append(stageBar(stage));
	const progress = ctx.acceptanceOf(item);
	if (progress) details.append(acceptanceLine(progress));
	if (item.progress_summary) details.append(el("p", { className: "progress", text: item.progress_summary }));
	if (item.next_action) details.append(el("p", { className: "next", text: `Next: ${item.next_action}` }));
	if (item.blocker) details.append(el("p", { className: "blocker", text: `Blocker: ${item.blocker}` }));
	if (item.waiting_reason && item.waiting_on_human) details.append(el("p", { className: "waiting", text: `Waiting: ${item.waiting_reason}` }));

	const foot = el("div", { className: "row-foot" });
	foot.append(
		el("span", { className: "priority", text: `priority ${item.priority}` }),
		el("span", { className: "health", text: `health ${item.health}` }),
		el("span", { className: "updated", text: `updated ${relativeTime(item.updated_at, ctx.now)}` }),
	);
	if (item.labels.length > 0) foot.append(el("span", { className: "labels", text: item.labels.map((label) => `#${label}`).join(" ") }));
	if (item.blocked_by.length > 0) {
		foot.append(el("span", { className: "blocked", text: `blocked by ${item.blocked_by.map((ref) => ref.identifier).join(", ")}` }));
	}
	const open = el("button", {
		className: "open-detail",
		text: "Details",
		attrs: { type: "button", "aria-label": `Open details for ${item.identifier}` },
	});
	open.dataset.action = "open-detail";
	foot.append(open);

	node.append(head, details, foot);
	return node;
}

function tagged(text: string, tone: string, key: string): HTMLElement {
	const chip = el("span", { className: "badge", text });
	chip.dataset.tone = tone;
	chip.dataset.bucket = key;
	return chip;
}

/**
 * Compact stage bar from the item's own workflow definition. When the definition
 * is unavailable it says so — it never falls back to the six default states.
 */
export function stageBar(stage: StageModel): HTMLElement {
	const bar = el("div", { className: "stage-bar" });
	bar.dataset.workflow = `${stage.workflowId}@${stage.workflowVersion}`;
	if (stage.missing) {
		bar.dataset.missing = "true";
		bar.append(el("span", { className: "stage-missing", text: `◇ no workflow definition (${stage.workflowId || "unset"}@${stage.workflowVersion})` }));
		return bar;
	}
	if (stage.loading) {
		bar.append(el("span", { className: "stage-missing", text: "◇ loading stages…" }));
		return bar;
	}
	const summary = el("span", { className: "stage-summary", text: stageBadge(stage) });
	const list = el("ol", { className: "stages", attrs: { "aria-label": "Workflow stages" } });
	for (const entry of stage.stages) {
		const mark = categoryMark(entry.category);
		const li = el("li", { className: "stage" });
		li.dataset.state = entry.current ? "current" : entry.next ? "next" : entry.terminal ? "terminal" : "other";
		li.dataset.category = entry.category;
		li.append(el("span", { className: "stage-symbol", text: mark.symbol }));
		li.append(el("span", { className: "stage-label", text: entry.label }));
		if (entry.current) li.setAttribute("aria-current", "step");
		list.append(li);
	}
	bar.append(summary, list);
	if (stage.unknownState) bar.append(el("span", { className: "stage-warn", text: `⚠ state "${stage.current?.label ?? "?"}" is not declared by this workflow` }));
	return bar;
}

function acceptanceLine(progress: NonNullable<ReturnType<typeof acceptanceProgress>>): HTMLElement {
	const line = el("p", { className: "acceptance-line" });
	const bar = el("span", { className: "acceptance-bar", attrs: { role: "img", "aria-label": `acceptance ${progress.text}` } });
	const fill = el("span", { className: "acceptance-fill" });
	fill.style.width = `${Math.round(progress.ratio * 100)}%`;
	bar.append(fill);
	// Text, not just the bar width: the ratio must be readable without colour.
	line.append(el("span", { className: "acceptance-text", text: `acceptance ${progress.text}` }), bar);
	return line;
}

// ---------------------------------------------------------------------------
// Topology view
// ---------------------------------------------------------------------------

/** Cards for the two decision groups plus a delta for the selected item. */
export function updateTopology(
	shell: Shell,
	summary: TopologySummary,
	ctx: RenderContext,
): void {
	const groups: HTMLElement[] = [];

	const ready = el("section", { className: "topo-group", attrs: { "aria-label": "Ready to start" } });
	ready.dataset.group = "ready";
	ready.append(el("h3", { className: "topo-title", text: `Ready to start · ${summary.ready.length}` }));
	if (summary.ready.length === 0) ready.append(el("p", { className: "muted", text: "Nothing is unblocked right now." }));
	for (const item of summary.ready) ready.append(renderCard(item, ctx, { unlocks: summary.unlocks.get(item.id) }));
	groups.push(ready);

	const waiting = el("section", { className: "topo-group", attrs: { "aria-label": "Waiting on upstream" } });
	waiting.dataset.group = "waiting";
	waiting.append(el("h3", { className: "topo-title", text: `Waiting on upstream · ${summary.waiting.length}` }));
	if (summary.waiting.length === 0) waiting.append(el("p", { className: "muted", text: "Nothing is waiting on another item." }));
	for (const entry of summary.waiting) {
		waiting.append(renderCard(entry.item, ctx, { directBlockers: entry.directBlockers, transitiveBlockers: entry.transitiveBlockers, unlocks: entry.unlocks }));
	}
	groups.push(waiting);

	if (summary.cycles.length > 0) {
		const cycles = el("section", { className: "topo-group", attrs: { "aria-label": "Dependency cycles" } });
		cycles.dataset.group = "cycles";
		cycles.append(el("h3", { className: "topo-title", text: `Dependency cycles · ${summary.cycles.length}` }));
		cycles.append(
			el("p", {
				className: "muted",
				text: "These items block each other, so none of them can start. Break the loop with `apm dep remove`.",
			}),
		);
		for (const item of summary.cycles) cycles.append(renderCard(item, ctx, { cycle: true }));
		groups.push(cycles);
	}

	shell.topologyGroups.replaceChildren(...groups);
}

export interface CardExtra {
	directBlockers?: WorkItem[];
	transitiveBlockers?: number;
	unlocks?: number;
	cycle?: boolean;
}

function renderCard(item: WorkItem, ctx: RenderContext, extra: CardExtra = {}): HTMLElement {
	const stage = ctx.stageOf(item);
	const selected = ctx.selectedId === item.id;
	const card = el("article", {
		className: "topo-card",
		attrs: {
			role: "button",
			tabindex: "0",
			"aria-label": `${item.identifier}: ${item.title}${extra.cycle ? " (dependency cycle)" : ""}`,
			"aria-pressed": String(selected),
		},
	});
	card.dataset.itemId = item.id;
	card.dataset.selected = String(selected);
	card.dataset.health = item.health;

	const head = el("div", { className: "topo-card-head" });
	head.append(el("span", { className: "identifier", text: item.identifier }), el("span", { className: "title", text: item.title }));
	const badges = el("div", { className: "badges" });
	if (extra.cycle) badges.append(tagged("⟲ cycle", "danger", "cycle"));
	if (item.status_category !== "closed" && !extra.cycle) {
		badges.append(tagged(`${item.priority === "highest" ? "▲▲" : "·"} ${item.priority}`, "muted", "priority"));
	}
	head.append(badges);
	card.append(head);

	const body = el("div", { className: "topo-card-body" });
	body.append(stageBar(stage));

	if (extra.directBlockers && extra.directBlockers.length > 0) {
		const list = el("p", { className: "topo-blockers" });
		list.append(el("span", { className: "field-label", text: "waiting on " }));
		list.append(
			el("span", {
				text: extra.directBlockers.map((blocker) => `${blocker.identifier} (${blocker.status})`).join(", "),
			}),
		);
		body.append(list);
	}
	const facts = el("p", { className: "topo-facts" });
	const bits: string[] = [`health ${item.health}`, `updated ${relativeTime(item.updated_at, ctx.now)}`];
	if (extra.transitiveBlockers !== undefined) bits.push(`${extra.transitiveBlockers} upstream blocker(s)`);
	if (extra.unlocks !== undefined) bits.push(extra.unlocks > 0 ? `unlocks ${extra.unlocks}` : "unlocks nothing");
	const progress = ctx.acceptanceOf(item);
	if (progress) bits.push(`acceptance ${progress.text}`);
	facts.textContent = bits.join(" · ");
	body.append(facts);
	card.append(body);

	const foot = el("div", { className: "row-foot" });
	const open = el("button", {
		className: "open-detail",
		text: "Details",
		attrs: { type: "button", "aria-label": `Open details for ${item.identifier}` },
	});
	open.dataset.action = "open-detail";
	foot.append(open);
	card.append(foot);
	return card;
}

/**
 * Summary panel for the selected item. Kept separate from the detail drawer so a
 * selection (used by the topology and by APM-21's local graph) does not require
 * opening a dialog.
 */
export function updateSelectionSummary(
	shell: Shell,
	ctx: RenderContext,
	item: WorkItem | null,
	detail?: { acceptance?: Array<{ status: string }>; context?: { doc?: { goal?: string; current_state?: string } } },
): void {
	if (!ctx.selectedId || !item) {
		shell.summary.replaceChildren(el("p", { className: "muted", text: "Select an item to see its stage, branches and acceptance." }));
		return;
	}
	const stage = ctx.stageOf(item);

	const nodes: HTMLElement[] = [];
	const head = el("div", { className: "summary-head" });
	// No Details control here on purpose: the row/card that owns this item already
	// has the entry point, and a duplicate inside the summary panel was a dead
	// button (it lived outside the `[data-item-id]` container its handler needs).
	head.append(el("span", { className: "identifier", text: item.identifier }), el("span", { className: "title", text: item.title }));
	nodes.push(head);

	nodes.push(el("p", { className: "muted", text: `${item.status} · health ${item.health} · priority ${item.priority} · ${item.workflow_id ?? "no workflow"}@${item.workflow_version ?? 0}` }));
	nodes.push(stageBar(stage));
	if (stage.branches.length > 0) {
		const branches = el("div", { className: "branches" });
		branches.append(el("span", { className: "field-label", text: "Next branches" }));
		const list = el("ul", { className: "branch-list" });
		for (const branch of stage.branches) {
			const mark = categoryMark(branch.category);
			const li = el("li", { className: "branch" });
			li.append(el("span", { className: "branch-mark", text: `${mark.symbol} ${branch.label}` }));
			li.append(el("span", { className: "branch-guards", text: branch.requires.length > 0 ? branch.requires.join(", ") : "no guard" }));
			list.append(li);
		}
		branches.append(list);
		nodes.push(branches);
	}
	const progress = ctx.acceptanceOf(item);
	if (progress) nodes.push(acceptanceLine(progress));
	if (detail?.context?.doc?.goal) nodes.push(el("p", { className: "summary-goal", text: detail.context.doc.goal }));
	if (detail?.context?.doc?.current_state) nodes.push(el("p", { className: "muted", text: detail.context.doc.current_state }));

	shell.summary.replaceChildren(...nodes);
}

/**
 * The local dependency graph lives in its own panel under the topology groups:
 * it needs the full width, and it must be readable without opening the drawer.
 */
export function updateDagPanel(
	shell: Shell,
	ctx: RenderContext,
	item: WorkItem | null,
	graph: TopologyGraph | null,
	scope: DagScope,
	onSelect: (id: string) => void,
): void {
	if (!ctx.selectedId || !item || !graph) {
		shell.dagPanel.replaceChildren(
			el("p", { className: "muted", text: "Select an item to see its upstream and downstream items." }),
		);
		return;
	}
	const dag = localDag(graph, item.id, 2);
	const nodes: HTMLElement[] = [];
	nodes.push(el("h3", { className: "topo-title", text: `Local graph · ${item.identifier}` }));
	const scopes = el("div", { className: "dag-scopes", attrs: { role: "group", "aria-label": "Graph scope" } });
	for (const [value, label] of [
		["all", "All"],
		["blocking", "Blocking chain"],
		["upstream", "Upstream"],
		["downstream", "Downstream"],
	] as Array<[DagScope, string]>) {
		const button = el("button", { className: "ghost dag-scope", text: label, attrs: { type: "button", "aria-pressed": String(scope === value) } });
		button.dataset.scope = value;
		scopes.append(button);
	}
	nodes.push(scopes);
	nodes.push(renderDag(dag, { scope, onSelect }));
	const legend = el("p", { className: "dag-legend" });
	legend.append(el("span", { className: "legend solid", text: "── dependency (blocks)" }));
	legend.append(el("span", { className: "legend dashed", text: "╌╌ parent / child" }));
	nodes.push(legend);
	const details = el("details", { className: "dag-text-wrap" }) as HTMLDetailsElement;
	details.append(el("summary", { text: "Text view (same relations)" }));
	details.append(renderDagText(dag, { scope, onSelect }));
	nodes.push(details);
	shell.dagPanel.replaceChildren(...nodes);
}

export { bucketMeta };
