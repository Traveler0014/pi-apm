/**
 * Refresh loop with stale-response protection.
 *
 * The dashboard polls at most every [`MAX_INTERVAL_MS`] (5 s, per the product
 * requirement). Three rules keep it honest:
 *
 * 1. **No overlap** — the next tick is scheduled *after* the previous request
 *    settles, so a slow server cannot pile up requests.
 * 2. **Stale responses are discarded** — every request gets a monotonic
 *    sequence number and only a response newer than the last applied one is
 *    handed to the UI, so a slow reply cannot overwrite a fresh one.
 * 3. **A failure never clears the screen** — the caller keeps the last good
 *    snapshot, and the poller reports `lastError` plus the time of the last
 *    success so the UI can render "cached data".
 */

/** Hard ceiling for the auto-refresh period (acceptance: ≤ 5 s). */
export const MAX_INTERVAL_MS = 5_000;
export const DEFAULT_INTERVAL_MS = 4_000;

export interface PollerOptions<T> {
	load: (signal: AbortSignal) => Promise<T>;
	onData: (data: T) => void;
	onError: (error: Error) => void;
	/** Refresh period; clamped to [`MAX_INTERVAL_MS`]. */
	intervalMs?: number;
	/** Injectable clock + scheduler for tests. */
	now?: () => number;
	setTimer?: (fn: () => void, ms: number) => unknown;
	clearTimer?: (handle: unknown) => void;
}

export interface Poller {
	start(): void;
	stop(): void;
	/** Manual refresh (button / keyboard). Safe while a request is in flight. */
	refreshNow(): void;
	isRunning(): boolean;
	isRefreshing(): boolean;
	lastSuccessAt(): number | null;
	lastError(): string | null;
	/** Number of applied responses — useful for tests and debugging. */
	appliedCount(): number;
}

export function createPoller<T>(options: PollerOptions<T>): Poller {
	const interval = Math.min(Math.max(options.intervalMs ?? DEFAULT_INTERVAL_MS, 500), MAX_INTERVAL_MS);
	const now = options.now ?? (() => Date.now());
	const setTimer = options.setTimer ?? ((fn, ms) => setTimeout(fn, ms));
	const clearTimer = options.clearTimer ?? ((handle) => clearTimeout(handle as ReturnType<typeof setTimeout>));

	let running = false;
	let timer: unknown = null;
	let inFlight: AbortController | null = null;
	/** Monotonic request id; only increasing ids may be applied. */
	let issued = 0;
	let applied = 0;
	let successAt: number | null = null;
	let error: string | null = null;
	let refreshing = false;

	function schedule(): void {
		if (!running) return;
		timer = setTimer(tick, interval);
	}

	function tick(): void {
		void run(false);
	}

	async function run(manual: boolean): Promise<void> {
		if (!running && !manual) return;
		// A manual refresh supersedes an in-flight automatic one: abort it so its
		// (older) response can never win.
		inFlight?.abort();
		const controller = new AbortController();
		inFlight = controller;
		const id = ++issued;
		refreshing = true;
		try {
			const data = await options.load(controller.signal);
			if (id > applied) {
				applied = id;
				successAt = now();
				error = null;
				options.onData(data);
			}
		} catch (cause) {
			const isAbort = cause instanceof Error && (cause.name === "AbortError" || /abort/i.test(cause.message));
			if (!isAbort && id > applied) {
				applied = id;
				error = cause instanceof Error ? cause.message : String(cause);
				options.onError(cause instanceof Error ? cause : new Error(String(cause)));
			}
		} finally {
			if (inFlight === controller) inFlight = null;
			refreshing = false;
			if (manual || id === issued) schedule();
		}
	}

	return {
		start() {
			if (running) return;
			running = true;
			void run(false);
		},
		stop() {
			running = false;
			inFlight?.abort();
			inFlight = null;
			if (timer !== null) {
				clearTimer(timer);
				timer = null;
			}
		},
		refreshNow() {
			void run(true);
		},
		isRunning: () => running,
		isRefreshing: () => refreshing,
		lastSuccessAt: () => successAt,
		lastError: () => error,
		appliedCount: () => applied,
	};
}
