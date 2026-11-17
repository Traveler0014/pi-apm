/**
 * Selection reconciliation across refreshes.
 *
 * The user's selection must survive a 4-second data refresh (the topology and the
 * summary panel depend on it), and must only be dropped when the item is really
 * gone from the ledger — never because the list was re-sorted, filtered or
 * momentarily empty.
 */

export interface ReconcileResult {
	selectedId: string | null;
	/** True when the previously selected item is no longer in the snapshot. */
	dropped: boolean;
}

export function reconcileSelection(selectedId: string | null, items: Array<{ id: string }>): ReconcileResult {
	if (selectedId === null) return { selectedId: null, dropped: false };
	return items.some((item) => item.id === selectedId)
		? { selectedId, dropped: false }
		: { selectedId: null, dropped: true };
}

/** Keep cached details only for items that still exist. */
export function pruneCache<T>(cache: Map<string, T>, items: Array<{ id: string }>): string[] {
	const removed: string[] = [];
	for (const id of [...cache.keys()]) {
		if (!items.some((item) => item.id === id)) {
			cache.delete(id);
			removed.push(id);
		}
	}
	return removed;
}

/**
 * A cached detail is only usable for the item version it came from: acceptance
 * criteria and context can change under us, so a version bump invalidates it.
 */
export function cacheIsFresh(cached: { version: number } | undefined, item: { version: number }): boolean {
	return cached !== undefined && cached.version === item.version;
}

/**
 * Resolve a deep link (`#item=APM-15`, or an id/uuid) against the snapshot.
 *
 * Works for *every* item, including closed ones: cancelled/aborted work must be
 * just as reachable by link as a finished task.
 */
export function findByReference<T extends { id: string; identifier: string }>(items: T[], reference: string): T | null {
	const wanted = reference.trim();
	if (!wanted) return null;
	return items.find((item) => item.identifier === wanted || item.id === wanted) ?? null;
}
