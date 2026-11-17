import { describe, expect, it } from "vitest";
import { DEFAULT_INTERVAL_MS, MAX_INTERVAL_MS, createPoller } from "./poller";

interface Deferred<T> {
	promise: Promise<T>;
	resolve: (value: T) => void;
	reject: (error: Error) => void;
}

function deferred<T>(): Deferred<T> {
	let resolve!: (value: T) => void;
	let reject!: (error: Error) => void;
	const promise = new Promise<T>((res, rej) => {
		resolve = res;
		reject = rej;
	});
	return { promise, resolve, reject };
}

/** Manual scheduler so the tests decide when a tick happens. */
function scheduler() {
	const timers: Array<{ fn: () => void; ms: number; cancelled: boolean }> = [];
	return {
		timers,
		setTimer(fn: () => void, ms: number) {
			const entry = { fn, ms, cancelled: false };
			timers.push(entry);
			return entry;
		},
		clearTimer(handle: unknown) {
			(handle as { cancelled: boolean }).cancelled = true;
		},
		/** Run the newest live timer (i.e. the next scheduled tick). */
		fire() {
			const live = timers.filter((timer) => !timer.cancelled);
			const next = live[live.length - 1];
			if (!next) throw new Error("no scheduled timer");
			next.cancelled = true;
			next.fn();
		},
		liveCount() {
			return timers.filter((timer) => !timer.cancelled).length;
		},
	};
}

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("createPoller", () => {
	it("clamps the refresh period to at most 5 s", async () => {
		const timers = scheduler();
		const poller = createPoller({
			load: async () => 1,
			onData: () => {},
			onError: () => {},
			intervalMs: 60_000,
			setTimer: timers.setTimer,
			clearTimer: timers.clearTimer,
		});
		poller.start();
		await flush();
		expect(timers.timers[0]!.ms).toBe(MAX_INTERVAL_MS);
		poller.stop();
	});

	it("uses the default period when none is given", async () => {
		const timers = scheduler();
		const poller = createPoller({
			load: async () => 1,
			onData: () => {},
			onError: () => {},
			setTimer: timers.setTimer,
			clearTimer: timers.clearTimer,
		});
		poller.start();
		await flush();
		expect(timers.timers[0]!.ms).toBe(DEFAULT_INTERVAL_MS);
		expect(DEFAULT_INTERVAL_MS).toBeLessThanOrEqual(MAX_INTERVAL_MS);
		poller.stop();
	});

	it("applies the newest response and discards a slower, older one", async () => {
		const timers = scheduler();
		const first = deferred<number>();
		const second = deferred<number>();
		const requests = [first, second];
		const applied: number[] = [];
		const poller = createPoller<number>({
			load: () => requests.shift()!.promise,
			onData: (value) => applied.push(value),
			onError: () => {},
			setTimer: timers.setTimer,
			clearTimer: timers.clearTimer,
		});

		poller.start(); // request #1 (slow)
		poller.refreshNow(); // request #2 aborts #1 and starts immediately
		second.resolve(2);
		await flush();
		// The stale first response lands after the fresh second one.
		first.resolve(1);
		await flush();

		expect(applied).toEqual([2]);
		expect(poller.appliedCount()).toBe(2);
	});

	it("does not stack requests while one is in flight", async () => {
		const timers = scheduler();
		let loads = 0;
		const pending = deferred<number>();
		const poller = createPoller<number>({
			load: () => {
				loads += 1;
				return pending.promise;
			},
			onData: () => {},
			onError: () => {},
			setTimer: timers.setTimer,
			clearTimer: timers.clearTimer,
		});
		poller.start();
		await flush();
		expect(loads).toBe(1);
		// While a request is in flight no timer exists: the next tick can only be
		// scheduled after it settles, which is what prevents pile-ups.
		expect(timers.liveCount()).toBe(0);
		pending.resolve(1);
		await flush();
		expect(loads).toBe(1);
		expect(timers.liveCount()).toBe(1);
		poller.stop();
	});

	it("reports a failure without losing the last success, then clears it", async () => {
		const timers = scheduler();
		const errors: string[] = [];
		const values: number[] = [];
		let call = 0;
		const poller = createPoller<number>({
			load: async () => {
				call += 1;
				if (call === 2) throw new Error("server unreachable");
				return call;
			},
			onData: (value) => values.push(value),
			onError: (error) => errors.push(error.message),
			now: () => 1_000 + call,
			setTimer: timers.setTimer,
			clearTimer: timers.clearTimer,
		});

		poller.start();
		await flush();
		expect(values).toEqual([1]);
		expect(poller.lastSuccessAt()).toBe(1_001);

		timers.fire();
		await flush();
		expect(errors).toEqual(["server unreachable"]);
		expect(poller.lastError()).toBe("server unreachable");
		expect(poller.lastSuccessAt()).toBe(1_001); // cache timestamp kept

		timers.fire();
		await flush();
		expect(values).toEqual([1, 3]);
		expect(poller.lastError()).toBeNull();
		poller.stop();
	});

	it("ignores an abort it caused itself and keeps polling", async () => {
		const timers = scheduler();
		const seen: string[] = [];
		const poller = createPoller<number>({
			load: (signal) =>
				new Promise<number>((resolve, reject) => {
					signal.addEventListener("abort", () => {
						const error = new Error("aborted");
						error.name = "AbortError";
						reject(error);
					});
					setTimeout(() => resolve(7), 5);
				}),
			onData: (value) => seen.push(`data:${value}`),
			onError: (error) => seen.push(`error:${error.name}`),
			setTimer: timers.setTimer,
			clearTimer: timers.clearTimer,
		});
		poller.start();
		poller.refreshNow(); // aborts #1
		await new Promise((resolve) => setTimeout(resolve, 20));
		expect(seen.filter((entry) => entry.startsWith("error"))).toEqual([]);
		expect(seen).toContain("data:7");
		poller.stop();
	});

	it("stops scheduling once stopped", async () => {
		const timers = scheduler();
		let loads = 0;
		const poller = createPoller<number>({
			load: async () => {
				loads += 1;
				return loads;
			},
			onData: () => {},
			onError: () => {},
			setTimer: timers.setTimer,
			clearTimer: timers.clearTimer,
		});
		poller.start();
		await flush();
		poller.stop();
		expect(poller.isRunning()).toBe(false);
		const before = loads;
		expect(() => timers.fire()).toThrow();
		expect(loads).toBe(before);
	});
});
