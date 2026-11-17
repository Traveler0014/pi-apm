/**
 * Dashboard bootstrap: token → api → state → poller → render.
 *
 * The UI is read-only. Two feeds are refreshed together — the dashboard (buckets,
 * totals, generated_at) and the flat item list (every item, open or closed) — and
 * workflow definitions / acceptance counts are fetched lazily and cached, so a
 * 4-second refresh never re-requests them.
 *
 * User state that must survive a refresh lives here and nowhere else: the view
 * mode (list/topology), the filters, the search text, the done fold and the
 * selected item (dropped only when the item really disappears).
 */

import "./style.css";
import {
	ApiError,
	type Api,
	bootstrapTokenFromFragment,
	createApi,
	hasToken,
	ticketErrorMessage,
} from "./api";
import { createDrawer } from "./detail";
import { createMockFetcher } from "./fixtures";
import { createPoller } from "./poller";
import {
	createShell,
	updateBanner,
	updateFilters,
	updateHeader,
	updateMetrics,
	updateProject,
	updateDagPanel,
	updateQueue,
	updateSelectionSummary,
	updateTopology,
	updateViews,
	type RenderContext,
	type ViewMode,
} from "./render";
import { cacheIsFresh, findByReference, pruneCache, reconcileSelection } from "./selection";
import { type DagScope, buildGraph, topologySummary } from "./topology";
import {
	type ConnectionView,
	type ViewState,
	buildQueue,
	connectionView,
	defaultViewState,
	summarize,
} from "./transform";
import type { Dashboard, ItemDetail, WorkItem, WorkflowDef } from "./types";
import { acceptanceProgress, stageModel, workflowKey } from "./workflow";

interface Snapshot {
	dashboard: Dashboard;
	items: WorkItem[];
	fetchedAt: number;
}

const root = document.getElementById("app");
if (!root) throw new Error("#app is missing from index.html");

const params = new URLSearchParams(window.location.search);
const useMock = params.get("mock") === "1";
const api: Api = useMock ? createApi({ fetcher: createMockFetcher({ latencyMs: 90 }) }) : createApi();

const shell = createShell(root);
const state: ViewState = defaultViewState();
let view: ViewMode = "list";
let selectedId: string | null = null;
let dagScope: DagScope = "all";
let snapshot: Snapshot | null = null;
let connection: ConnectionView = connectionView({ lastSuccessAt: null, lastError: null, refreshing: false, now: Date.now() });

// --- caches --------------------------------------------------------------
/** workflow definition cache: `id@version` → definition, "missing", or pending. */
const workflows = new Map<string, WorkflowDef | "missing" | "pending">();
/** detail cache: item id → detail + the item version it was fetched at. */
const details = new Map<string, { detail: ItemDetail; version: number }>();
const detailRequests = new Set<string>();

function stageOf(item: WorkItem): ReturnType<typeof stageModel> {
	return stageModel(item, (id, version) => {
		const key = workflowKey(id, version);
		const cached = workflows.get(key);
		if (cached === "missing") return null;
		if (cached && cached !== "pending") return cached;
		if (cached === "pending") return undefined;
		workflows.set(key, "pending");
		api
			.getItemWorkflow(item.id)
			.then((response) => {
				workflows.set(key, response.workflow);
				// The response can carry a different key if the item moved; store both.
				workflows.set(workflowKey(response.workflow_id, response.workflow_version), response.workflow);
			})
			.catch(() => workflows.set(key, "missing"))
			.finally(() => render());
		return undefined;
	});
}

function acceptanceOf(item: WorkItem): ReturnType<typeof acceptanceProgress> {
	const cached = details.get(item.id);
	const fresh = cacheIsFresh(cached, item);
	// Only the selected item is loaded eagerly; everything else shows nothing
	// rather than a misleading "0/0". A stale entry is refetched and, until the
	// new data lands, not shown at all (an old ratio would be a lie).
	if (selectedId === item.id && !fresh && !detailRequests.has(item.id)) {
		detailRequests.add(item.id);
		api
			.getItem(item.id)
			.then((response) => {
				details.set(item.id, { detail: response.item, version: response.item.item.version });
			})
			.catch(() => undefined)
			.finally(() => {
				detailRequests.delete(item.id);
				render();
			});
	}
	return fresh ? acceptanceProgress(cached!.detail.acceptance) : undefined;
}

function context(): RenderContext {
	return { state, view, selectedId, stageOf, acceptanceOf, now: Date.now() };
}

// --- rendering -----------------------------------------------------------

function render(): void {
	const focus = captureFocus();
	const ctx = context();
	const dashboard = snapshot?.dashboard ?? null;
	updateProject(shell, dashboard?.project ?? (useMock ? "pi-apm (mock)" : "…"));
	updateMetrics(shell, dashboard ? summarize(dashboard) : []);
	updateHeader(shell, {
		connection,
		generatedAt: dashboard?.generated_at ?? null,
		now: ctx.now,
		refreshing: connection.state === "refreshing",
	});
	updateBanner(shell, connection);
	updateViews(shell, view, selectedId);
	updateFilters(shell, state, dashboard);

	const queue = buildQueue(dashboard, snapshot?.items ?? null, state);
	updateQueue(shell, queue, ctx);

	const graph = buildGraph(snapshot?.items ?? []);
	updateTopology(shell, topologySummary(graph), ctx);
	const selected = selectedId ? (snapshot?.items.find((item) => item.id === selectedId) ?? null) : null;
	updateSelectionSummary(shell, ctx, selected, selected ? details.get(selected.id)?.detail : undefined);
	updateDagPanel(shell, ctx, selected, graph, dagScope, (id) => {
		selectedId = id;
		updateHash();
		render();
	});

	restoreFocus(focus);
}

function renderClock(): void {
	if (!snapshot) return;
	connection = connectionView({
		lastSuccessAt: poller.lastSuccessAt(),
		lastError: poller.lastError(),
		refreshing: poller.isRefreshing(),
		now: Date.now(),
	});
	updateHeader(shell, { connection, generatedAt: snapshot.dashboard.generated_at, now: Date.now(), refreshing: poller.isRefreshing() });
	updateBanner(shell, connection);
}

/** Remember what had focus so a data refresh cannot steal it from the keyboard user. */
function captureFocus(): { itemId?: string; action?: string; bucket?: string; view?: string } | null {
	const active = document.activeElement as HTMLElement | null;
	if (!active) return null;
	const row = active.closest<HTMLElement>("[data-item-id]");
	return { itemId: row?.dataset.itemId, action: active.dataset.action, bucket: active.dataset.bucket, view: active.dataset.view };
}

function restoreFocus(saved: ReturnType<typeof captureFocus>): void {
	if (!saved) return;
	const selector = saved.bucket
		? `[data-bucket="${saved.bucket}"]`
		: saved.view
			? `[data-view="${saved.view}"]`
			: saved.itemId
				? `[data-item-id="${saved.itemId}"]${saved.action ? ` [data-action="${saved.action}"]` : ""}`
				: null;
	if (!selector) return;
	document.querySelector<HTMLElement>(selector)?.focus({ preventScroll: true });
}

// --- data ----------------------------------------------------------------

async function load(signal: AbortSignal): Promise<Snapshot> {
	const [dashboard, items] = await Promise.all([
		api.getDashboard(signal),
		api.getItems({ all: true, limit: 500 }, signal),
	]);
	return { dashboard: dashboard.dashboard, items: items.items, fetchedAt: Date.now() };
}

/** Drop a selection only when the item is really gone from the ledger. */
function reconcileSelectionWith(items: WorkItem[]): void {
	const next = reconcileSelection(selectedId, items);
	selectedId = next.selectedId;
	const pruned = pruneCache(details, items);
	for (const id of pruned) detailRequests.delete(id);
	if (next.dropped) notice("The selected item is no longer in the ledger.");
}

function notice(message: string): void {
	shell.banner.hidden = false;
	shell.banner.dataset.state = "stale";
	shell.banner.textContent = message;
}

const poller = createPoller<Snapshot>({
	load,
	intervalMs: 4_000,
	onData(next) {
		const first = snapshot === null;
		snapshot = next;
		reconcileSelectionWith(next.items);
		connection = connectionView({ lastSuccessAt: poller.lastSuccessAt(), lastError: null, refreshing: false, now: Date.now() });
		render();
		if (first) void openDeepLink();
	},
	onError(error) {
		if (error instanceof ApiError && error.status === 401) {
			poller.stop();
			renderSessionError("Dashboard session expired or was rejected (401). Run `apm ui` again to get a new link.");
			return;
		}
		connection = connectionView({
			lastSuccessAt: poller.lastSuccessAt(),
			lastError: friendlyError(error),
			refreshing: false,
			now: Date.now(),
		});
		render();
	},
});

function friendlyError(error: unknown): string {
	if (error instanceof ApiError) {
		if (error.status === 401) return "unauthorized (open the URL printed by `apm ui`, or refresh the token)";
		if (error.status === 0 || error.message === "Failed to fetch") return "server unreachable";
		return `${error.code}: ${error.message}`;
	}
	return error instanceof Error ? error.message : String(error);
}

// --- interaction ---------------------------------------------------------

shell.onRefresh(() => {
	connection = connectionView({
		lastSuccessAt: poller.lastSuccessAt(),
		lastError: poller.lastError(),
		refreshing: true,
		now: Date.now(),
	});
	render();
	poller.refreshNow();
});

shell.onStateChange((patch) => {
	Object.assign(state, patch);
	render();
});

shell.onDagScope((scope) => {
	dagScope = scope;
	updateHash();
	render();
});

shell.onViewChange((next) => {
	view = next;
	updateHash();
	render();
});

shell.onSelect((id) => {
	selectedId = id;
	updateHash();
	render();
});

// Filter chips are delegated: they are re-created on every render.
shell.filters.addEventListener("click", (event) => {
	const chip = (event.target as HTMLElement).closest<HTMLElement>("[data-bucket]");
	if (!chip?.dataset.bucket) return;
	const key = chip.dataset.bucket as ViewState["buckets"][number];
	const next = new Set(state.buckets);
	if (next.has(key)) next.delete(key);
	else next.add(key);
	state.buckets = [...next];
	render();
});

shell.doneFold.addEventListener("toggle", () => {
	state.showRecentlyDone = shell.doneFold.open;
	if (shell.doneFold.open) render();
});

const drawer = createDrawer({
	api,
	onOpenChange(identifier) {
		updateHash({ item: identifier });
	},
});

shell.onOpenItem((id) => {
	void drawer.open(id);
});

/** `#view=topology&item=APM-15` — shareable, and the ticket is never in the hash. */
function updateHash(extra: { item?: string | null } = {}): void {
	const hash = new URLSearchParams();
	if (view !== "list") hash.set("view", view);
	if (dagScope !== "all") hash.set("scope", dagScope);
	const item = extra.item !== undefined ? extra.item : selectedId ? identifierOf(selectedId) : null;
	if (item) hash.set("item", item);
	const suffix = hash.toString();
	window.history.replaceState(null, "", `${window.location.pathname}${window.location.search}${suffix ? `#${suffix}` : ""}`);
}

function identifierOf(id: string): string | null {
	return snapshot?.items.find((item) => item.id === id)?.identifier ?? null;
}

let lastDeepLink: string | null = null;

async function openDeepLink(): Promise<void> {
	const raw = window.location.hash.startsWith("#") ? window.location.hash.slice(1) : window.location.hash;
	if (!raw) {
		lastDeepLink = null;
		return;
	}
	if (raw === lastDeepLink) return;
	lastDeepLink = raw;
	const hash = new URLSearchParams(raw);
	const wantedView = hash.get("view");
	if (wantedView === "topology" || wantedView === "list") view = wantedView;
	const wantedScope = hash.get("scope");
	if (wantedScope === "all" || wantedScope === "blocking" || wantedScope === "upstream" || wantedScope === "downstream") dagScope = wantedScope;
	const wanted = hash.get("item");
	if (wanted) {
		const match = findByReference(snapshot?.items ?? [], wanted);
		if (match) {
			selectedId = match.id;
			updateViews(shell, view, selectedId);
			render();
			if (!drawer.isOpen() || drawer.currentIdentifier() !== match.identifier) await drawer.open(match.id);
			return;
		}
	}
	render();
}

/** Blocking "session expired" state: no polling, no 401 storm, clear instruction. */
function renderSessionError(message: string): void {
	shell.banner.hidden = false;
	shell.banner.dataset.state = "offline";
	shell.banner.textContent = message;
	shell.connection.textContent = "session expired";
	shell.connection.dataset.state = "offline";
	shell.refreshButton.disabled = true;
}

async function init(): Promise<void> {
	window.addEventListener("hashchange", () => {
		// In-session deep links (`#item=APM-15`, `#view=topology`) work without a
		// reload; the ticket never appears here because it is stripped on load.
		void openDeepLink();
	});
	if (!useMock) {
		try {
			await bootstrapTokenFromFragment(window.location, window.history);
		} catch (cause) {
			renderSessionError(ticketErrorMessage(cause));
			return;
		}
		if (!hasToken()) {
			renderSessionError(
				"No dashboard session: open the URL printed by `apm ui` (the link carries a one-time ticket), or run `apm ui` again.",
			);
			return;
		}
	}
	render();
	poller.start();
	setInterval(renderClock, 1_000);
}

void init();
