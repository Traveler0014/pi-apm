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
	type QueueSection,
	type QueuedItem,
	type SortKey,
	type StatusScope,
	type ViewState,
	bucketMeta,
	clockTime,
	dataAge,
	relativeTime,
} from "./transform";
import { graphViewFor, localDagFor, renderDagText, type DagRelationMode, type GraphViewOptions } from "./dag";
import { groupMeta } from "./groups";
import { type DagScope, type LocalDag, type TopologyGraph, type TopologySummary } from "./topology";
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
	statusScope: HTMLElement;
	filters: HTMLElement;
	search: HTMLInputElement;
	sort: HTMLSelectElement;
	listView: HTMLElement;
	topologyView: HTMLElement;
	/** Scrollable master column (left on desktop). */
	topoList: HTMLElement;
	topologyGroups: HTMLElement;
	/** Sticky detail column: graph on top, selection summary below. */
	topoAside: HTMLElement;
	dagPanel: HTMLElement;
	/** Modal overlay for the expanded local graph (APM-29). */
	dagDialog: HTMLDialogElement;
	dagDialogBody: HTMLElement;
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
	onStatusScopeChange: (handler: (scope: StatusScope) => void) => void;
	onSelect: (handler: (id: string | null) => void) => void;
	onDagScope: (handler: (scope: DagScope) => void) => void;
	onDagRelation: (handler: (relation: DagRelationMode) => void) => void;
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
	const statusScope = el("div", { className: "status-scope", attrs: { role: "group", "aria-label": "Status scope" } });
	for (const [scope, label] of [
		["open", "Open"],
		["closed", "Closed"],
		["all", "All"],
	] as Array<[StatusScope, string]>) {
		const button = el("button", { className: "scope", text: label, attrs: { type: "button", "aria-pressed": "false" } });
		// `data-scope` is the DAG scope; keep the status control on its own key so
		// generic scope selectors cannot grab the wrong button.
		button.dataset.statusScope = scope;
		statusScope.append(button);
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
	controls.append(viewToggle, statusScope, filters, searchWrap, sortWrap);

	// --- list view --------------------------------------------------------
	const main = el("main", { className: "content" });
	const listView = el("section", { className: "list-view", attrs: { "aria-label": "Main queue" } });
	const queueCaption = el("h2", { className: "section-title" });
	const queue = el("div", { className: "queue" });
	const empty = el("p", { className: "empty", text: "Nothing matches the current filters.", attrs: { hidden: "" } });
	const doneFold = el("details", { className: "done-fold" }) as HTMLDetailsElement;
	const doneSummary = el("summary", { className: "done-summary" });
	const doneList = el("div", { className: "queue done-list", attrs: { role: "list" } });
	doneFold.append(doneSummary, doneList);
	listView.append(queueCaption, queue, empty, doneFold);

	// --- topology view ----------------------------------------------------
	const topologyView = el("section", { className: "topology-view", attrs: { "aria-label": "Dependency topology" } });
	// Master/detail: the list owns its own scrollbar, the graph and summary stay
	// put beside it, so selecting an item near the end of a long list never needs
	// a page scroll to reach the picture (APM-46).
	const topoList = el("div", {
		className: "topo-list",
		attrs: { role: "region", "aria-label": "Work items", tabindex: "-1" },
	});
	const topologyGroups = el("div", { className: "topology-groups" });
	topoList.append(topologyGroups);
	const topoAside = el("aside", { className: "topo-aside", attrs: { "aria-label": "Graph and selected item" } });
	const dagPanel = el("section", { className: "dag-panel", attrs: { "aria-label": "Local dependency graph" } });
	const summary = el("section", { className: "selection-summary", attrs: { "aria-label": "Selected item" } });
	topoAside.append(dagPanel, summary);
	topologyView.append(topoList, topoAside);

	// Expanded graph overlay: native <dialog> gives focus trapping and Esc for
	// free; the same layout runs with larger boxes. Rendered on demand.
	const dagDialog = el("dialog", {
		className: "dag-dialog",
		attrs: { "aria-label": "Expanded local dependency graph" },
	}) as HTMLDialogElement;
	const dagDialogBody = el("div", { className: "dag-dialog-body" });
	dagDialog.append(dagDialogBody);
	// Inside the topology view so the delegated scope/selection handlers see it.
	topologyView.append(dagDialog);

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
		statusScope,
		filters,
		search,
		sort,
		listView,
		topologyView,
		topoList,
		topologyGroups,
		topoAside,
		dagPanel,
		dagDialog,
		dagDialogBody,
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
		onStatusScopeChange(handler) {
			statusScope.addEventListener("click", (event) => {
				const button = (event.target as HTMLElement).closest<HTMLElement>("[data-status-scope]");
				if (button?.dataset.statusScope) handler(button.dataset.statusScope as StatusScope);
			});
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
		onDagRelation(handler) {
			topologyView.addEventListener("click", (event) => {
				// `.dag` itself carries data-relation; only the buttons switch views.
				const button = (event.target as HTMLElement).closest<HTMLElement>("button[data-relation]");
				if (button?.dataset.relation) handler(button.dataset.relation as DagRelationMode);
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

/**
 * The workspace has to end at the viewport bottom, and how much chrome sits above
 * it changes with the banner, metrics and wrapped controls. Measure it instead of
 * guessing a constant, so a long queue scrolls inside its own column and the page
 * itself never moves (APM-46).
 */
export function applyWorkspaceHeight(shell: Shell): void {
	if (shell.topologyView.hidden) return;
	const top = shell.topologyView.getBoundingClientRect().top + window.scrollY;
	const parent = shell.root;
	// The shell's own bottom padding is part of the page height, so the columns
	// have to reserve it too or the page gains a few dozen pixels of scroll.
	const bottom = parent ? Number.parseFloat(getComputedStyle(parent).paddingBottom || "0") || 0 : 0;
	shell.topologyView.style.setProperty("--topo-top", `${Math.max(0, Math.round(top))}px`);
	shell.topologyView.style.setProperty("--topo-bottom", `${Math.round(bottom)}px`);
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

/** Reflect the open/closed/all scope in the segmented control. */
export function updateStatusScope(shell: Shell, state: ViewState): void {
	for (const button of shell.statusScope.querySelectorAll<HTMLButtonElement>("[data-status-scope]")) {
		button.setAttribute("aria-pressed", String(button.dataset.statusScope === state.statusScope));
	}
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
	shell.queue.replaceChildren(...model.groups.map((section) => renderGroup(section, ctx)));
	shell.empty.hidden = model.groups.some((section) => section.rows.length > 0);

	// When the status scope promotes closed work into a group, the fold would show
	// the same rows twice, so it is hidden instead.
	shell.doneFold.hidden = model.showClosedGroup || model.closedTotal === 0;
	shell.doneFold.open = ctx.state.showRecentlyDone;
	// Every closed category lives here (done/cancelled/aborted/custom), not just
	// the dashboard's "recently done" window.
	shell.doneSummary.textContent = `Closed items · ${model.closedTotal}${
		model.closed.length !== model.closedTotal ? ` (${model.closed.length} shown)` : ""
	}`;
	shell.doneList.replaceChildren(...(ctx.state.showRecentlyDone ? model.closed.map((row) => renderRow(row, ctx)) : []));
}

/**
 * One mutually exclusive group: a heading (symbol + label + count), the rule in
 * one sentence, then its rows. The heading is what makes "Startable" mean only
 * startable work — a `doing` item with blockers sits under "In progress" and
 * carries an upstream badge instead of leaking into the ready list.
 */
function renderGroup(section: QueueSection, ctx: RenderContext): HTMLElement {
	const group = el("section", {
		className: "queue-group",
		attrs: { "aria-label": `${section.meta.label} (${section.rows.length})` },
	});
	group.dataset.group = section.meta.key;
	group.dataset.tone = section.meta.tone;
	const title = el("h3", { className: "group-title" });
	title.append(
		el("span", { className: "group-symbol", text: section.meta.symbol }),
		el("span", { className: "group-label", text: `${section.meta.label} · ${section.rows.length}` }),
	);
	const description = el("p", { className: "group-description", text: section.meta.description });
	const rows = el("div", { className: "group-rows", attrs: { role: "list" } });
	rows.append(...section.rows.map((row) => renderRow(row, ctx)));
	group.append(title, description, rows);
	return group;
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
	node.dataset.group = row.group;
	// The group explanation is available on hover and to screen readers, so the
	// classification is never just a colour.
	if (row.explanation) node.title = row.explanation;

	const head = el("div", { className: "row-head" });
	head.append(el("span", { className: "identifier", text: item.identifier }), el("span", { className: "title", text: item.title }));

	const badges = el("div", { className: "badges" });
	for (const badge of row.badges) {
		const chip = el("span", { className: "badge", text: `${badge.symbol} ${badge.label}` });
		chip.dataset.tone = badge.tone;
		chip.dataset.bucket = badge.key;
		badges.append(chip);
	}
	if (row.openBlockerCount > 0 && !row.badges.some((badge) => badge.key === "waiting_dependencies")) {
		badges.append(tagged(`◆ blocked by ${row.openBlockerCount}`, "warn", "waiting_dependencies"));
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

/**
 * Decision groups in the same order and with the same names as the queue:
 * Needs human, At risk, In progress, Waiting upstream, Ready to start. Only
 * startable items may appear under "Ready to start".
 */
export function updateTopology(
	shell: Shell,
	summary: TopologySummary,
	ctx: RenderContext,
	closedRows?: QueuedItem[],
): void {
	const groups: HTMLElement[] = [];
	// Status scope wins: with `closed`/`all`, the open groups are still drawn (they
	// simply contain no closed items) and a real Closed group is appended.
	const scope = ctx.state.statusScope;
	const hideOpen = scope === "closed";

	const groupSection = (key: string, label: string, items: TopologySummary["ready"], empty: string): HTMLElement => {
		const section = el("section", { className: "topo-group", attrs: { "aria-label": label } });
		section.dataset.group = key;
		section.append(el("h3", { className: "topo-title", text: `${label} · ${items.length}` }));
		if (items.length === 0) section.append(el("p", { className: "muted", text: empty }));
		for (const item of items) section.append(renderCard(item, ctx, { unlocks: summary.unlocks.get(item.id) }));
		return section;
	};

	if (!hideOpen) {
		groups.push(groupSection("needs_human", "Needs human", summary.needsHuman, "Nobody is waiting on a human answer."));
		groups.push(groupSection("at_risk", "At risk", summary.atRisk, "No stale leases or unexplained waiting states."));
		groups.push(groupSection("in_progress", "In progress", summary.inProgress, "No active or claimed work."));
	}

	const ready = el("section", { className: "topo-group", attrs: { "aria-label": "Ready to start" } });
	if (hideOpen) ready.hidden = true;
	ready.dataset.group = "ready";
	ready.append(el("h3", { className: "topo-title", text: `Ready to start · ${summary.ready.length}` }));
	if (summary.ready.length === 0) ready.append(el("p", { className: "muted", text: "Nothing is unblocked right now." }));
	for (const item of summary.ready) ready.append(renderCard(item, ctx, { unlocks: summary.unlocks.get(item.id) }));
	groups.push(ready);

	const waiting = el("section", { className: "topo-group", attrs: { "aria-label": "Waiting on upstream" } });
	if (hideOpen) waiting.hidden = true;
	waiting.dataset.group = "waiting";
	waiting.append(el("h3", { className: "topo-title", text: `Waiting on upstream · ${summary.waiting.length}` }));
	if (summary.waiting.length === 0) waiting.append(el("p", { className: "muted", text: "Nothing is waiting on another item." }));
	for (const entry of summary.waiting) {
		waiting.append(renderCard(entry.item, ctx, { directBlockers: entry.directBlockers, transitiveBlockers: entry.transitiveBlockers, unlocks: entry.unlocks }));
	}
	groups.push(waiting);

	if (!hideOpen && summary.cycles.length > 0) {
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

	// Closed work becomes a first-class group when the status scope asks for it:
	// done, cancelled, aborted and custom closed states are searchable and open the
	// same detail drawer as open items. Rows are the already filtered/sorted ones
	// from `buildQueue`, so List and Topology never disagree.
	if (closedRows) {
		const meta = groupMeta("closed");
		const closed = el("section", { className: "topo-group", attrs: { "aria-label": `${meta.label} items` } });
		closed.dataset.group = "closed";
		closed.append(el("h3", { className: "topo-title", text: `${meta.symbol} ${meta.label} · ${closedRows.length}` }));
		closed.append(el("p", { className: "muted", text: meta.description }));
		if (closedRows.length === 0) closed.append(el("p", { className: "muted", text: "No closed item matches the current search." }));
		for (const row of closedRows) closed.append(renderCard(row.item, ctx));
		groups.push(closed);
	}

	// The list owns its own scrollbar (APM-46): a poll refresh must not throw the
	// user back to the top of a long queue.
	const scrollTop = shell.topoList.scrollTop;
	shell.topologyGroups.replaceChildren(...groups);
	shell.topoList.scrollTop = scrollTop;
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

/** Relation sub-view state, owned by the caller so it survives re-renders. */
export interface DagPanelState {
	relation: DagRelationMode;
	/** Last viewport the user left behind; reused while the picture is the same. */
	view: GraphViewOptions["view"] | null;
	/** `<selection>|<relation>|<scope>` of that view. */
	key: string | null;
}

const RELATIONS: Array<[DagRelationMode, string, string]> = [
	["dependencies", "Dependencies", "Work order: what blocks the selected item, and what it unblocks"],
	["hierarchy", "Hierarchy", "Parent and children of the selected item"],
];

function relationButtons(current: DagRelationMode): HTMLElement {
	const group = el("div", { className: "dag-relations", attrs: { role: "group", "aria-label": "Graph relation" } });
	for (const [value, label, hint] of RELATIONS) {
		const button = el("button", {
			className: "ghost dag-relation",
			text: label,
			attrs: { type: "button", title: hint, "aria-pressed": String(current === value) },
		});
		button.dataset.relation = value;
		group.append(button);
	}
	return group;
}

function scopeButtons(current: DagScope, relation: DagRelationMode): HTMLElement {
	const scopes = el("div", { className: "dag-scopes", attrs: { role: "group", "aria-label": "Graph scope" } });
	for (const [value, label] of [
		["all", "All"],
		["blocking", "Blocking chain"],
		["upstream", "Upstream"],
		["downstream", "Downstream"],
	] as Array<[DagScope, string]>) {
		const button = el("button", { className: "ghost dag-scope", text: label, attrs: { type: "button", "aria-pressed": String(current === value) } });
		button.dataset.scope = value;
		scopes.append(button);
	}
	// Scopes narrow a dependency chain; a hierarchy has nothing to narrow.
	scopes.hidden = relation === "hierarchy";
	return scopes;
}

/**
 * The local graph lives in the sticky detail column beside the list (APM-46).
 *
 * Two relation sub-views share one component: `dependencies` (work order, the
 * product default) and `hierarchy` (parent/children). The legacy combined
 * picture is deliberately not offered — mixing a parent and a dependent in one
 * rank produced reversed same-column edges.
 */
export function updateDagPanel(
	shell: Shell,
	ctx: RenderContext,
	item: WorkItem | null,
	graph: TopologyGraph | null,
	scope: DagScope,
	onSelect: (id: string) => void,
	panel?: DagPanelState,
): void {
	if (!ctx.selectedId || !item || !graph) {
		shell.dagPanel.replaceChildren(
			el("p", { className: "muted", text: "Select an item to see its upstream and downstream items." }),
		);
		if (shell.dagDialog.open) closeDagDialog(shell.dagDialog);
		return;
	}
	const relation: DagRelationMode = panel?.relation ?? "dependencies";
	// A hierarchy is never scoped: the scope buttons do not apply there.
	const effectiveScope: DagScope = relation === "hierarchy" ? "all" : scope;
	const dag = localDagFor(graph, item.id, 2, relation);
	const viewKey = `${item.id}|${relation}|${effectiveScope}`;
	// Fit only when the picture changes (other selection, relation or scope); a
	// poll refresh keeps the user's zoom and pan.
	const keepView = panel && panel.key === viewKey ? (panel.view ?? undefined) : undefined;

	const nodes: HTMLElement[] = [];
	const head = el("div", { className: "dag-panel-head" });
	head.append(el("h3", { className: "topo-title", text: `Local graph · ${item.identifier}` }));
	const expand = el("button", {
		className: "ghost dag-expand",
		text: "Expand graph",
		attrs: { type: "button", "aria-haspopup": "dialog", "aria-label": "Expand local dependency graph" },
	});
	expand.addEventListener("click", () => {
		renderDagDialog(shell, dag, item, effectiveScope, relation, onSelect);
		openDagDialog(shell.dagDialog);
		const close = shell.dagDialogBody.querySelector<HTMLButtonElement>("[data-action=close-graph]");
		close?.focus();
	});
	// Native dialog focus trap + Esc; returning focus to the trigger matters for
	// keyboard users and is asserted in the browser checks.
	shell.dagDialog.onclose = () => expand.focus();
	head.append(expand);
	nodes.push(head);

	const toolbar = el("div", { className: "dag-toolbar" });
	toolbar.append(relationButtons(relation), scopeButtons(effectiveScope, relation));
	nodes.push(toolbar);

	const handle = graphViewFor(dag, {
		scope: effectiveScope,
		relation,
		onSelect,
		view: keepView,
		onViewChange: (view) => {
			if (!panel) return;
			panel.view = view;
			panel.key = viewKey;
		},
	});
	nodes.push(handle.element);

	const legend = el("p", { className: "dag-legend" });
	if (relation === "hierarchy") {
		legend.append(el("span", { className: "legend dashed", text: "╌╌ parent / child" }));
	} else {
		legend.append(el("span", { className: "legend solid", text: "── dependency (blocks)" }));
	}
	nodes.push(legend);

	const details = el("details", { className: "dag-text-wrap" }) as HTMLDetailsElement;
	details.append(el("summary", { text: "Text view (same relations)" }));
	details.append(renderDagText(dag, { scope: effectiveScope, onSelect }));
	nodes.push(details);
	shell.dagPanel.replaceChildren(...nodes);
	// The viewport is measured only once it is in the document: a fit computed
	// while detached would shrink the picture to a thumbnail. Fit after mounting,
	// and only for a new picture (a poll refresh keeps the user's zoom/pan).
	if (!keepView) handle.fit();

	// Changing relation or scope inside the overlay goes through the normal
	// delegated handler, which re-renders the panel; keep the open overlay in sync.
	if (shell.dagDialog.open) renderDagDialog(shell, dag, item, effectiveScope, relation, onSelect);
}

function openDagDialog(dialog: HTMLDialogElement): void {
	if (typeof dialog.showModal === "function") {
		if (!dialog.open) dialog.showModal();
	} else {
		dialog.setAttribute("open", "");
	}
}

function closeDagDialog(dialog: HTMLDialogElement): void {
	if (typeof dialog.close === "function") dialog.close();
	else dialog.removeAttribute("open");
}

/** Expanded graph: same layout with larger boxes, plus scopes, legend and text view. */
function renderDagDialog(
	shell: Shell,
	dag: LocalDag,
	item: WorkItem,
	scope: DagScope,
	relation: DagRelationMode,
	onSelect: (id: string) => void,
): void {
	const body = shell.dagDialogBody;
	const head = el("div", { className: "dag-dialog-head" });
	head.append(el("h3", { className: "topo-title", text: `Local graph · ${item.identifier}` }));
	const close = el("button", {
		className: "ghost",
		text: "Close ✕",
		attrs: { type: "button", "aria-label": "Close expanded graph", "data-action": "close-graph" },
	});
	close.addEventListener("click", () => closeDagDialog(shell.dagDialog));
	head.append(close);
	const selectAndClose = (id: string) => {
		closeDagDialog(shell.dagDialog);
		onSelect(id);
	};
	const controls = el("div", { className: "dag-dialog-controls" });
	controls.append(relationButtons(relation), scopeButtons(scope, relation));
	const graphHandle = graphViewFor(dag, { scope, relation, onSelect: selectAndClose, expanded: true });
	const graph = graphHandle.element;
	const legend = el("p", { className: "dag-legend" });
	if (relation === "hierarchy") legend.append(el("span", { className: "legend dashed", text: "╌╌ parent / child" }));
	else legend.append(el("span", { className: "legend solid", text: "── dependency (blocks)" }));
	const text = renderDagText(dag, { scope, onSelect: selectAndClose });
	body.replaceChildren(head, controls, graph, legend, text);
	// Same reason as the inline panel: measure the viewport after it is mounted.
	graphHandle.fit();
	// Esc closes even where <dialog> semantics are emulated (tests, older engines).
	shell.dagDialog.onkeydown = (event: KeyboardEvent) => {
		if (event.key === "Escape") {
			event.preventDefault();
			closeDagDialog(shell.dagDialog);
		}
	};
}

export { bucketMeta };
