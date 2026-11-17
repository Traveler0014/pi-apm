/**
 * Fixtures for `?mock=1` (parallel frontend work without a running `apm ui`)
 * and for the view-model tests. Shapes mirror the real API exactly.
 */

import type {
	Dashboard,
	EventView,
	Fetcher,
	ItemDetail,
	Settings,
	WorkItem,
} from "./types";

const SETTINGS: Settings = {
	lease_seconds: 900,
	stale_after_seconds: 300,
	no_progress_after_seconds: 86_400,
	done_recent_seconds: 86_400,
};

function item(overrides: Partial<WorkItem> & { identifier: string; title: string; id: string }): WorkItem {
	const now = "2026-11-17T02:10:00.000Z";
	return {
		project: "pi-apm",
		project_id: "p1",
		number: Number(overrides.identifier.split("-")[1] ?? 0),
		item_type: "task",
		status: "doing",
		status_category: "active",
		workflow_id: "apm-default",
		workflow_version: 1,
		priority: "normal",
		parent_id: null,
		parent_identifier: null,
		version: 3,
		context_version: 1,
		progress_summary: null,
		next_action: null,
		blocker: null,
		waiting_on_human: false,
		waiting_reason: null,
		completion_note: null,
		created_by: "planner",
		created_at: now,
		updated_at: now,
		last_heartbeat_at: now,
		last_progress_at: now,
		last_run_sync_at: null,
		done_at: null,
		cancelled_at: null,
		health: "active",
		claim: null,
		labels: [],
		blocked_by: [],
		blocks: [],
		attention: [],
		seconds_since_progress: 30,
		...overrides,
	};
}

export const MOCK_ITEMS: WorkItem[] = [
	item({
		id: "i-15",
		identifier: "APM-15",
		title: "Implement the read-only dashboard",
		status: "doing",
		priority: "high",
		labels: ["frontend", "ui"],
		progress_summary: "Header, metrics and the queue render from /api/dashboard",
		next_action: "Wire the poller and stale-response guard",
		claim: {
			id: "c1",
			holder: "pi",
			holder_kind: "agent",
			status: "active",
			expires_at: "2026-11-17T02:25:00.000Z",
			heartbeat_at: "2026-11-17T02:10:00.000Z",
			lease_seconds: 900,
			session_id: "s1",
			run_id: "r1",
			expired: false,
		},
		health: "active",
	}),
	item({
		id: "i-16",
		identifier: "APM-16",
		title: "Detail drawer with handover context",
		status: "ready",
		status_category: "open",
		priority: "high",
		health: "unclaimed",
		labels: ["frontend"],
		seconds_since_progress: null,
		last_progress_at: null,
	}),
	item({
		id: "i-14",
		identifier: "APM-14",
		title: "Serve the UI from the Rust binary",
		status: "doing",
		priority: "high",
		health: "stale",
		waiting_on_human: true,
		waiting_reason: "needs the dist asset contract",
		labels: ["rust", "ui"],
		claim: {
			id: "c2",
			holder: "apm-worker",
			holder_kind: "agent",
			status: "active",
			expires_at: "2026-11-17T02:20:00.000Z",
			heartbeat_at: "2026-11-17T02:00:00.000Z",
			lease_seconds: 900,
			session_id: "s2",
			run_id: null,
			expired: true,
		},
	}),
	item({
		id: "i-17",
		identifier: "APM-17",
		title: "Browser end-to-end verification",
		status: "blocked",
		status_category: "waiting",
		priority: "normal",
		health: "released",
		blocked_by: [{ id: "i-16", identifier: "APM-16", title: "Detail drawer", status: "ready" }],
		progress_summary: "Waiting for the drawer to land",
		seconds_since_progress: 600,
	}),
	item({
		id: "i-13",
		identifier: "APM-13",
		title: "Roadmap: local read-only web dashboard",
		status: "doing",
		status_category: "active",
		priority: "highest",
		health: "active",
		item_type: "story",
	}),
];

export const MOCK_DONE: WorkItem[] = [
	item({
		id: "i-12",
		identifier: "APM-12",
		title: "Roadmap: run-sync visualisation",
		status: "done",
		status_category: "closed",
		health: "ended",
		done_at: "2026-11-16T12:42:00.000Z",
		updated_at: "2026-11-16T12:42:00.000Z",
		item_type: "story",
	}),
	item({
		id: "i-11",
		identifier: "APM-11",
		title: "Verify the run-sync card and document diagnosis",
		status: "done",
		status_category: "closed",
		health: "ended",
		done_at: "2026-11-16T12:37:00.000Z",
		updated_at: "2026-11-16T12:37:00.000Z",
	}),
];

export function mockDashboard(generatedAt = new Date().toISOString()): Dashboard {
	const byId = new Map(MOCK_ITEMS.map((row) => [row.id, row]));
	const pick = (...ids: string[]): WorkItem[] => ids.map((id) => byId.get(id)!).filter(Boolean);
	return {
		generated_at: generatedAt,
		project: "pi-apm",
		project_id: "p1",
		settings: SETTINGS,
		totals: {
			open_items: 5,
			waiting_human: 1,
			at_risk: 1,
			in_progress: 2,
			ready_unclaimed: 1,
			"status:done": MOCK_DONE.length,
			"status:doing": 3,
		},
		buckets: {
			// APM-14 is in two buckets on purpose: the UI must render one row with
			// both badges, not two rows.
			waiting_human: pick("i-14"),
			in_progress: pick("i-15", "i-13", "i-14"),
			stalled_no_progress: pick("i-17"),
			lost_lease: [],
			recovery_failed: [],
			waiting_dependencies: pick("i-17"),
			ready_unclaimed: pick("i-16"),
			recently_done: MOCK_DONE,
		},
	};
}

export function mockDetail(id: string): ItemDetail {
	const found = MOCK_ITEMS.find((row) => row.id === id || row.identifier === id) ?? MOCK_ITEMS[0]!;
	return {
		item: found,
		context: {
			version: 2,
			doc: {
				goal: `Ship ${found.identifier}: ${found.title}`,
				current_state: "Header, metrics, queue and filter controls are wired; the poller is next.",
				constraints: ["Vanilla TypeScript + Vite, no framework", "Read-only: no writes from the UI"],
				references: ["docs/rest.md", "src/rest.rs", "ui/src/main.ts"],
				notes: "Rendered by the embedded read-only dashboard.",
			},
			reason: "refresh the handover before implementation",
			author: "ui-planner",
			created_at: "2026-11-17T02:05:53.930Z",
			revises_version: 1,
		},
		acceptance: [
			{ seq: 1, text: "Header shows data time, connection state, manual refresh and summary metrics", status: "met", note: "covered by transform/poller tests" },
			{ seq: 2, text: "Queue dedupes by item id with multi-status badges, filters, search, sort and a done fold", status: "open", note: null },
			{ seq: 3, text: "Discards stale responses and keeps filters when a refresh fails", status: "open", note: null },
		],
		evidence: [
			{ kind: "test", value: "vitest ui => 24 tests", note: "transform + poller", created_by: "pi", created_at: "2026-11-17T02:20:00.000Z" },
		],
		attention: [
			{ id: "a1", question: "Should the done fold stay collapsed on mobile?", context: "One row per item", options: ["collapsed", "expanded"], recommendation: "collapsed", blocking: false, status: "open", requested_by: "pi" },
		],
		history_len: 12,
	};
}

export function mockEvents(id: string): EventView[] {
	const base = Date.parse("2026-11-17T02:00:00.000Z");
	// Oldest first, exactly like the server; version deltas are realistic
	// (creation has no previous version, a context revision does not bump it).
	const steps: Array<{ kind: string; actor: string; prev: number | null; next: number | null; payload?: unknown }> = [
		{ kind: "item_created", actor: "ui-planner", prev: null, next: 1, payload: { title: id } },
		{ kind: "context_revised", actor: "ui-planner", prev: 1, next: 1, payload: { context_version: 1 } },
		{ kind: "claimed", actor: "pi", prev: null, next: null, payload: { lease_seconds: 900 } },
		{ kind: "status_changed", actor: "pi", prev: 1, next: 2, payload: { from: "ready", to: "doing" } },
		{ kind: "progress_recorded", actor: "pi", prev: 2, next: 3, payload: { summary: "wired the poller" } },
	];
	return steps.map((step, index) => ({
		id: 100 + index,
		at: new Date(base + index * 60_000).toISOString(),
		kind: step.kind,
		actor: step.actor,
		actor_kind: step.actor === "pi" ? "agent" : "human",
		identifier: id,
		prev_version: step.prev,
		new_version: step.next,
		run_id: step.kind === "claimed" ? "2026-11-17T02:07:31.200Z-12" : null,
		session_id: "s1",
		payload: step.payload ?? {},
	}));
}

/** A `fetch` stand-in that serves the fixtures with optional artificial latency. */
export function createMockFetcher(options: { latencyMs?: number } = {}): Fetcher {
	const latency = options.latencyMs ?? 120;
	return async (url) => {
		await new Promise((resolve) => setTimeout(resolve, latency));
		const parsed = new URL(url, "http://mock.local");
		const path = parsed.pathname.replace(/^\/api/, "");
		const body = (() => {
			if (path === "/health") return { ok: true, service: "apm-rest", version: "mock" };
			if (path === "/dashboard") return { ok: true, dashboard: mockDashboard() };
			if (path === "/items") return { ok: true, count: MOCK_ITEMS.length, items: MOCK_ITEMS };
			const detail = path.match(/^\/items\/([^/]+)$/);
			if (detail) return { ok: true, item: mockDetail(decodeURIComponent(detail[1]!)) };
			const events = path.match(/^\/items\/([^/]+)\/events$/);
			if (events) return { ok: true, events: mockEvents(decodeURIComponent(events[1]!)) };
			return { ok: false, error: { code: "not_found", message: `no mock route for ${path}` } };
		})();
		return {
			ok: (body as { ok?: boolean }).ok !== false,
			status: (body as { ok?: boolean }).ok === false ? 404 : 200,
			text: async () => JSON.stringify(body),
		};
	};
}
