/**
 * Dashboard bootstrap: token → api → state → poller → render.
 *
 * The UI is read-only. Two independent feeds are refreshed together — the
 * dashboard (buckets, totals, generated_at) and the flat item list (for filters
 * the buckets do not expose) — and both must succeed for a render to count as
 * "live"; a failed refresh keeps the previous snapshot on screen.
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
	updateQueue,
} from "./render";
import {
	type ConnectionView,
	type ViewState,
	buildQueue,
	connectionView,
	defaultViewState,
	summarize,
} from "./transform";
import type { Dashboard, WorkItem } from "./types";

interface Snapshot {
	dashboard: Dashboard;
	items: WorkItem[];
	fetchedAt: number;
}

const root = document.getElementById("app");
if (!root) throw new Error("#app is missing from index.html");

const params = new URLSearchParams(window.location.search);
const useMock = params.get("mock") === "1";
const api: Api = useMock
	? createApi({ fetcher: createMockFetcher({ latencyMs: 90 }) })
	: createApi();

const shell = createShell(root);
const state: ViewState = defaultViewState();
let snapshot: Snapshot | null = null;
let connection: ConnectionView = connectionView({ lastSuccessAt: null, lastError: null, refreshing: false, now: Date.now() });

/**
 * `#item=APM-15` opens that item's drawer once the first snapshot arrives, so a
 * link from a terminal or a session log lands on the right detail view.
 */
async function openDeepLink(): Promise<void> {
	const raw = window.location.hash.startsWith("#") ? window.location.hash.slice(1) : window.location.hash;
	if (!raw) return;
	const wanted = new URLSearchParams(raw).get("item");
	if (!wanted) return;
	const match = snapshot?.items.find((row) => row.identifier === wanted || row.id === wanted);
	if (match) await drawer.open(match.id);
}

/** Remember what had focus so a data refresh cannot steal it from the keyboard user. */
function captureFocus(): { itemId?: string; action?: string; bucket?: string } | null {
	const active = document.activeElement as HTMLElement | null;
	if (!active) return null;
	const row = active.closest<HTMLElement>("[data-item-id]");
	return {
		itemId: row?.dataset.itemId,
		action: active.dataset.action,
		bucket: active.dataset.bucket,
	};
}

function restoreFocus(saved: ReturnType<typeof captureFocus>): void {
	if (!saved) return;
	const selector = saved.bucket
		? `[data-bucket="${saved.bucket}"]`
		: saved.itemId
			? `[data-item-id="${saved.itemId}"]${saved.action ? ` [data-action="${saved.action}"]` : ""}`
			: null;
	if (!selector) return;
	document.querySelector<HTMLElement>(selector)?.focus({ preventScroll: true });
}

function render(): void {
	const focus = captureFocus();
	const now = Date.now();
	const dashboard = snapshot?.dashboard ?? null;
	updateProject(shell, dashboard?.project ?? (useMock ? "pi-apm (mock)" : "…"));
	updateMetrics(shell, dashboard ? summarize(dashboard) : []);
	updateHeader(shell, {
		connection,
		generatedAt: dashboard?.generated_at ?? null,
		now,
		refreshing: connection.state === "refreshing",
	});
	updateBanner(shell, connection);
	updateFilters(shell, state, dashboard);
	if (dashboard) {
		updateQueue(shell, buildQueue(dashboard, state), state, now);
	} else {
		updateQueue(shell, { items: [], total: 0, filtered: 0, recentlyDone: [], recentlyDoneTotal: 0 }, state, now);
	}
	restoreFocus(focus);
}

/**
 * Header-only update for the 1-second clock tick: relative ages stay honest
 * without rebuilding rows (which would drop focus and scroll position).
 */
function renderClock(): void {
	if (!snapshot) return;
	connection = connectionView({
		lastSuccessAt: poller.lastSuccessAt(),
		lastError: poller.lastError(),
		refreshing: poller.isRefreshing(),
		now: Date.now(),
	});
	updateHeader(shell, {
		connection,
		generatedAt: snapshot.dashboard.generated_at,
		now: Date.now(),
		refreshing: poller.isRefreshing(),
	});
	updateBanner(shell, connection);
}

async function load(signal: AbortSignal): Promise<Snapshot> {
	const [dashboard, items] = await Promise.all([api.getDashboard(signal), api.getItems({ all: true, limit: 500 }, signal)]);
	return { dashboard: dashboard.dashboard, items: items.items, fetchedAt: Date.now() };
}

const poller = createPoller<Snapshot>({
	load,
	intervalMs: 4_000,
	onData(next) {
		const first = snapshot === null;
		snapshot = next;
		// Failure state is owned by connectionView(); a successful refresh clears it.
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

// The done fold is user state, not fetch state: keep it across refreshes.
shell.doneFold.addEventListener("toggle", () => {
	state.showRecentlyDone = shell.doneFold.open;
	if (shell.doneFold.open) render();
});

// The detail drawer is read-only and independent from the poller: it loads on
// open (and on its own Refresh button) so a background refresh can never yank
// focus away from a copy button the user is aiming at.
const drawer = createDrawer({
	api,
	onOpenChange(identifier) {
		// replaceState (not `location.hash = ""`) so closing does not leave a bare
		// "#" behind in the address bar.
		const url = identifier
			? `${window.location.pathname}${window.location.search}#item=${encodeURIComponent(identifier)}`
			: `${window.location.pathname}${window.location.search}`;
		window.history.replaceState(null, "", url);
	},
});

shell.onOpenItem((id) => {
	void drawer.open(id);
});

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
	// The ticket arrives once, in the fragment, and is stripped + exchanged
	// immediately; without it there is nothing to authenticate with.
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
	// Relative timestamps and freshness labels stay honest between fetches.
	setInterval(renderClock, 1_000);
}

void init();
