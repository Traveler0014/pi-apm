/**
 * Lazy per-item loaders (workflow definition + acceptance detail).
 *
 * Both used to live inside `render()`: a failed request ran `finally(render)`,
 * which immediately asked for the same thing again. A backend that returned
 * 500 (or a flaky network) therefore produced an unbounded request storm from a
 * single tick, and one transient workflow failure pinned every stage badge to
 * "missing" for the rest of the page's life.
 *
 * The rules here are deliberately boring:
 *
 * * a request is only started by a render/selection call, never by its own
 *   completion handler;
 * * a failure records `(item, version, when)` and is not retried until the
 *   cooldown elapses;
 * * only a real `404` caches a workflow as permanently `"missing"` — a 5xx or a
 *   network error stays retryable.
 *
 * Keeping this out of the DOM also makes it testable with a fake clock and a
 * counting fake API.
 */

import { ApiError, type Api } from "./api";
import { cacheIsFresh, pruneCache } from "./selection";
import type { ItemDetail, WorkItem, WorkflowDef } from "./types";
import { acceptanceProgress, stageModel, workflowKey } from "./workflow";

/** Retry window after a failed detail fetch for the same item version. */
export const DETAIL_RETRY_COOLDOWN_MS = 15_000;
/** Retry window after a transient workflow-definition failure. */
export const WORKFLOW_RETRY_COOLDOWN_MS = 15_000;

export interface LazyLoaderOptions {
	api: Pick<Api, "getItem" | "getItemWorkflow">;
	/** Called after new data lands, so the caller can re-render. Never on failure. */
	onUpdate: () => void;
	/** Injected clock (tests); defaults to `Date.now`. */
	now?: () => number;
	detailCooldownMs?: number;
	workflowCooldownMs?: number;
}

export interface LazyLoader {
	/** Stage bar for one item; `undefined` while loading, `null` when missing. */
	stageOf(item: WorkItem): ReturnType<typeof stageModel>;
	/** Acceptance ratio for the selected item; `undefined` until loaded. */
	acceptanceOf(item: WorkItem, selectedId: string | null): ReturnType<typeof acceptanceProgress>;
	/** The cached detail for the selection summary. */
	detailFor(itemId: string): ItemDetail | undefined;
	/** Drop caches for items that are no longer in the snapshot. */
	prune(items: ReadonlyArray<{ id: string }>): void;
}

/** `item.id + version` → when the last detail attempt failed. */
interface DetailFailure {
	version: number;
	at: number;
}

export function createLazyLoader(options: LazyLoaderOptions): LazyLoader {
	const { api, onUpdate } = options;
	const now = options.now ?? (() => Date.now());
	const detailCooldownMs = options.detailCooldownMs ?? DETAIL_RETRY_COOLDOWN_MS;
	const workflowCooldownMs = options.workflowCooldownMs ?? WORKFLOW_RETRY_COOLDOWN_MS;

	const workflows = new Map<string, WorkflowDef | "missing" | "pending">();
	const workflowFailedAt = new Map<string, number>();
	const details = new Map<string, { detail: ItemDetail; version: number }>();
	const detailRequests = new Set<string>();
	const detailFailures = new Map<string, DetailFailure>();

	function coolingDown(failure: DetailFailure | undefined, version: number, at: number): boolean {
		return failure !== undefined && failure.version === version && at - failure.at < detailCooldownMs;
	}

	function stageOf(item: WorkItem): ReturnType<typeof stageModel> {
		return stageModel(item, (id, version) => {
			const key = workflowKey(id, version);
			const cached = workflows.get(key);
			if (cached === "missing") return null;
			if (cached && cached !== "pending") return cached;
			if (cached === "pending") return undefined;

			const at = now();
			const failedAt = workflowFailedAt.get(key);
			if (failedAt !== undefined && at - failedAt < workflowCooldownMs) {
				// Still cooling down: render as loading, do not fire another request.
				return undefined;
			}
			workflows.set(key, "pending");
			api.getItemWorkflow(item.id).then(
				(response) => {
					workflowFailedAt.delete(key);
					workflows.set(key, response.workflow);
					// The item may have moved to another workflow key; cache both.
					workflows.set(workflowKey(response.workflow_id, response.workflow_version), response.workflow);
					onUpdate();
				},
				(error: unknown) => {
					if (error instanceof ApiError && error.status === 404) {
						// A definition really does not exist. This is the only permanent state.
						workflows.set(key, "missing");
						workflowFailedAt.delete(key);
						// A real state change (loading → missing) is worth one render; the
						// cached "missing" makes that render request-free.
						onUpdate();
						return;
					}
					// Transient (network, 5xx, 401 on a dead session): stay retryable
					// after the cooldown instead of degrading to "missing" forever.
					workflows.delete(key);
					workflowFailedAt.set(key, now());
				},
			);
			return undefined;
		});
	}

	function acceptanceOf(item: WorkItem, selectedId: string | null): ReturnType<typeof acceptanceProgress> {
		const cached = details.get(item.id);
		const fresh = cacheIsFresh(cached, item);
		if (selectedId === item.id && !fresh && !detailRequests.has(item.id)) {
			const at = now();
			if (!coolingDown(detailFailures.get(item.id), item.version, at)) {
				detailRequests.add(item.id);
				api.getItem(item.id).then(
					(response) => {
						details.set(item.id, { detail: response.item, version: item.version });
						detailFailures.delete(item.id);
						detailRequests.delete(item.id);
						onUpdate();
					},
					() => {
						// Keyed by item *and* version: a real edit (version bump) retries
						// immediately, a repeat failure of the same data waits.
						detailFailures.set(item.id, { version: item.version, at: now() });
						detailRequests.delete(item.id);
						// No `onUpdate` here — that is what closes the render → fetch →
						// render loop. The 4 s poll re-renders and retries after cooldown.
					},
				);
			}
		}
		return fresh ? acceptanceProgress(cached!.detail.acceptance) : undefined;
	}

	function detailFor(itemId: string): ItemDetail | undefined {
		return details.get(itemId)?.detail;
	}

	function prune(items: ReadonlyArray<{ id: string }>): void {
		for (const id of pruneCache(details, [...items])) {
			detailRequests.delete(id);
			detailFailures.delete(id);
		}
	}

	return { stageOf, acceptanceOf, detailFor, prune };
}
