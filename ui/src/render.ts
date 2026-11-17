/**
 * DOM rendering for the dashboard shell.
 *
 * The shell is built once and only the dynamic regions are rewritten, because a
 * full re-render would steal focus and caret position from the search box on
 * every 4-second refresh (and would collapse the done fold). All text goes
 * through `textContent`, so ledger data can never be interpreted as markup.
 */

import {
	BUCKETS,
	type ConnectionView,
	type Metric,
	type QueuedItem,
	type QueueModel,
	type SortKey,
	type ViewState,
	bucketMeta,
	clockTime,
	dataAge,
	relativeTime,
} from "./transform";
import type { BucketKey, Dashboard } from "./types";

export interface Shell {
	root: HTMLElement;
	connection: HTMLElement;
	dataTime: HTMLElement;
	refreshButton: HTMLButtonElement;
	metrics: HTMLElement;
	banner: HTMLElement;
	filters: HTMLElement;
	search: HTMLInputElement;
	sort: HTMLSelectElement;
	queue: HTMLElement;
	queueCaption: HTMLElement;
	doneFold: HTMLDetailsElement;
	doneSummary: HTMLElement;
	doneList: HTMLElement;
	empty: HTMLElement;
	onRefresh: (handler: () => void) => void;
	onStateChange: (handler: (next: Partial<ViewState>) => void) => void;
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
	const refreshHint = el("kbd", { text: "R" });
	refresh.append(" ", refreshHint);
	meta.append(connection, dataTime, refresh);
	header.append(brand, meta);

	// --- metrics ----------------------------------------------------------
	const metrics = el("section", { className: "metrics", attrs: { "aria-label": "Summary" } });

	// --- banner (offline / stale) ----------------------------------------
	const banner = el("p", { className: "banner", attrs: { role: "status", "aria-live": "polite", hidden: "" } });

	// --- controls ---------------------------------------------------------
	const controls = el("section", { className: "controls", attrs: { "aria-label": "Filter and sort" } });
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
	controls.append(filters, searchWrap, sortWrap);

	// --- queue ------------------------------------------------------------
	const main = el("main", { className: "content" });
	const queueSection = el("section", { className: "queue-section", attrs: { "aria-label": "Main queue" } });
	const queueCaption = el("h2", { className: "section-title" });
	const queue = el("div", { className: "queue", attrs: { role: "list" } });
	const empty = el("p", { className: "empty", text: "Nothing matches the current filters.", attrs: { hidden: "" } });
	queueSection.append(queueCaption, queue, empty);

	const doneFold = el("details", { className: "done-fold" }) as HTMLDetailsElement;
	const doneSummary = el("summary", { className: "done-summary" });
	const doneList = el("div", { className: "queue done-list", attrs: { role: "list" } });
	doneFold.append(doneSummary, doneList);
	queueSection.append(doneFold);

	main.append(queueSection);
	shellRoot.append(header, metrics, banner, controls, main);
	root.append(shellRoot);

	const shell: Shell = {
		root: shellRoot,
		connection,
		dataTime,
		refreshButton: refresh,
		metrics,
		banner,
		filters,
		search,
		sort,
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
		onOpenItem(handler) {
			queue.addEventListener("click", (event) => {
				const row = (event.target as HTMLElement).closest<HTMLElement>("[data-item-id]");
				if (!row) return;
				handler(row.dataset.itemId ?? "");
			});
			queue.addEventListener("keydown", (event) => {
				if (event.key !== "Enter" && event.key !== " ") return;
				const row = (event.target as HTMLElement).closest<HTMLElement>("[data-item-id]");
				if (!row) return;
				event.preventDefault();
				handler(row.dataset.itemId ?? "");
			});
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
			? `Offline — showing ${connection.detail}. Filters and the done fold are kept; retrying every few seconds.`
			: `Showing cached data — ${connection.detail}.`;
}

export function updateFilters(shell: Shell, state: ViewState, dashboard: Dashboard | null): void {
	const active = new Set(state.buckets);
	const counts = new Map<BucketKey, number>();
	for (const bucket of BUCKETS) {
		counts.set(bucket.key, (dashboard?.buckets[bucket.key] ?? []).length);
	}
	shell.filters.replaceChildren(
		...BUCKETS.filter((bucket) => bucket.key !== "recently_done").flatMap((bucket) => {
			const count = counts.get(bucket.key) ?? 0;
			// Keep a chip for an active filter even when its bucket is empty, so the
			// user always sees why nothing is listed.
			if (count === 0 && !active.has(bucket.key)) return [];
			const button = el("button", {
				className: "chip",
				attrs: {
					type: "button",
					"aria-pressed": String(active.has(bucket.key)),
					title: `${bucket.label} (${count})`,
				},
			});
			button.dataset.bucket = bucket.key;
			button.dataset.tone = bucket.tone;
			button.append(el("span", { className: "chip-symbol", text: bucket.symbol }), el("span", { className: "chip-label", text: bucket.label }), el("span", { className: "chip-count", text: String(count) }));
			return [button];
		}),
	);
	if (shell.search.value !== state.search) shell.search.value = state.search;
	if (shell.sort.value !== state.sort) shell.sort.value = state.sort;
	shell.filters.dataset.active = state.buckets.join(",");
}

export function updateQueue(shell: Shell, model: QueueModel, state: ViewState, now: number): void {
	shell.queueCaption.textContent = `Main queue · ${model.items.length}${
		model.filtered > 0 ? ` of ${model.total} (+${model.filtered} filtered out)` : ""
	}`;
	shell.queue.replaceChildren(...model.items.map((row) => renderRow(row, now)));
	shell.empty.hidden = model.items.length > 0;

	shell.doneFold.hidden = model.recentlyDoneTotal === 0;
	shell.doneFold.open = state.showRecentlyDone;
	shell.doneSummary.textContent = `Recently done · ${model.recentlyDoneTotal}${
		model.recentlyDone.length !== model.recentlyDoneTotal ? ` (${model.recentlyDone.length} shown)` : ""
	}`;
	shell.doneList.replaceChildren(...(state.showRecentlyDone ? model.recentlyDone.map((row) => renderRow(row, now)) : []));
}

function renderRow(row: QueuedItem, now: number): HTMLElement {
	const item = row.item;
	const node = el("article", {
		className: "row",
		// Rows are an extra keyboard affordance (Enter/Space opens the dialog); the
		// Details button remains the primary, announced control.
		attrs: {
			role: "listitem",
			tabindex: "0",
			"aria-label": `${item.identifier}: ${item.title}`,
			"aria-haspopup": "dialog",
		},
	});
	node.dataset.itemId = item.id;
	node.dataset.priority = item.priority;
	node.dataset.health = item.health;

	const head = el("div", { className: "row-head" });
	head.append(
		el("span", { className: "identifier", text: item.identifier }),
		el("span", { className: "title", text: item.title }),
	);
	const badges = el("div", { className: "badges" });
	for (const badge of row.badges) {
		const chip = el("span", { className: "badge" });
		chip.dataset.tone = badge.tone;
		chip.dataset.bucket = badge.key;
		// Symbol + word: status is never colour-only.
		chip.textContent = `${badge.symbol} ${badge.label}`;
		badges.append(chip);
	}
	if (item.waiting_on_human) badges.append(badge("⏸ waiting human", "warn", "waiting_human"));
	if (item.claim && item.claim.status === "active") {
		const expired = item.claim.expired ? " (lease expired)" : "";
		badges.append(badge(`✋ ${item.claim.holder}${expired}`, item.claim.expired ? "danger" : "muted", "claim"));
	}
	head.append(badges);

	const details = el("div", { className: "row-detail" });
	if (item.progress_summary) details.append(el("p", { className: "progress", text: item.progress_summary }));
	if (item.next_action) details.append(el("p", { className: "next", text: `Next: ${item.next_action}` }));
	if (item.blocker) details.append(el("p", { className: "blocker", text: `Blocker: ${item.blocker}` }));
	if (item.waiting_reason && item.waiting_on_human) details.append(el("p", { className: "waiting", text: `Waiting: ${item.waiting_reason}` }));

	const foot = el("div", { className: "row-foot" });
	foot.append(
		el("span", { className: "priority", text: `priority ${item.priority}` }),
		el("span", { className: "health", text: `health ${item.health}` }),
		el("span", { className: "updated", text: `updated ${relativeTime(item.updated_at, now)}` }),
	);
	if (item.labels.length > 0) foot.append(el("span", { className: "labels", text: item.labels.map((label) => `#${label}`).join(" ") }));
	if (item.blocked_by.length > 0) {
		foot.append(el("span", { className: "blocked", text: `blocked by ${item.blocked_by.map((ref) => ref.identifier).join(", ")}` }));
	}
	const open = el("button", { className: "open-detail", text: "Details", attrs: { type: "button", "aria-label": `Open details for ${item.identifier}` } });
	open.dataset.action = "open-detail";
	foot.append(open);

	node.append(head, details, foot);
	return node;
}

function badge(text: string, tone: string, key: string): HTMLElement {
	const chip = el("span", { className: "badge", text });
	chip.dataset.tone = tone;
	chip.dataset.bucket = key;
	return chip;
}

export { bucketMeta };
