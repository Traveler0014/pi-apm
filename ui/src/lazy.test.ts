// @vitest-environment happy-dom
/**
 * Regression tests for the lazy loaders (APM-55).
 *
 * The bugs these pin down were both "invisible until the backend hiccups":
 *
 * 1. `acceptanceOf()` retried from its own failure path, so one failing
 *    `getItem` produced an unbounded request/render loop;
 * 2. `stageOf()` cached `"missing"` for *any* error, so one 5xx pinned every
 *    stage badge to "no workflow" until the page was reloaded.
 */

import { describe, expect, it } from "vitest";
import { ApiError, type Api } from "./api";
import { createLazyLoader } from "./lazy";
import type { ItemDetailResponse, ItemWorkflowResponse, WorkItem, WorkflowDef } from "./types";

const flush = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

const ITEM = {
	id: "item-1",
	identifier: "APM-1",
	version: 3,
	workflow_id: "apm-default",
	workflow_version: 1,
	status: "doing",
} as unknown as WorkItem;

const WORKFLOW = {
	workflow_id: "apm-default",
	workflow_version: 1,
	workflow: {
		id: "apm-default",
		version: 1,
		name: "Default",
		initial: "ready",
		states: [
			{ key: "ready", label: "Ready", category: "open", terminal: false },
			{ key: "doing", label: "Doing", category: "active", terminal: false },
		],
		transitions: [{ from: "ready", to: "doing" }],
	} as unknown as WorkflowDef,
};

type WorkflowApi = Pick<Api, "getItem" | "getItemWorkflow">;

/** Only the two methods the loader calls are needed; count the calls. */
function fakeApi(overrides: Partial<WorkflowApi>): WorkflowApi {
	return {
		getItem: async () => {
			throw new Error("getItem not configured");
		},
		getItemWorkflow: async () => {
			throw new Error("getItemWorkflow not configured");
		},
		...overrides,
	};
}

function workflowResponse(): ItemWorkflowResponse {
	return {
		ok: true,
		identifier: ITEM.identifier,
		workflow_id: WORKFLOW.workflow_id,
		workflow_version: WORKFLOW.workflow_version,
		workflow: WORKFLOW.workflow,
	};
}

function detailResponse(version: number): ItemDetailResponse {
	return {
		ok: true,
		item: {
			item: { ...ITEM, version },
			acceptance: [{ status: "met" }, { status: "open" }, { status: "waived" }],
		},
	} as unknown as ItemDetailResponse;
}

describe("stageOf workflow loading", () => {
	it("caches 'missing' only for a real 404 and never re-requests", async () => {
		let calls = 0;
		let updates = 0;
		const loader = createLazyLoader({
			api: fakeApi({
				getItemWorkflow: async () => {
					calls += 1;
					throw new ApiError({ status: 404, code: "not_found", message: "no workflow for item" });
				},
			}),
			onUpdate: () => {
				updates += 1;
			},
		});

		expect(loader.stageOf(ITEM).loading).toBe(true);
		await flush();
		expect(loader.stageOf(ITEM).missing).toBe(true);
		expect(calls).toBe(1);
		// Loading → missing is a visible change: exactly one render, and that
		// render is request-free because "missing" is cached.
		expect(updates).toBe(1);

		// Twenty more renders (the poller, user interactions) stay at one request.
		for (let index = 0; index < 20; index += 1) loader.stageOf(ITEM);
		await flush();
		expect(calls).toBe(1);
		expect(updates).toBe(1);
	});

	it("keeps a transient 5xx retryable after a cooldown instead of locking 'missing'", async () => {
		let clock = 0;
		let calls = 0;
		let updates = 0;
		let fail = true;
		const loader = createLazyLoader({
			api: fakeApi({
				getItemWorkflow: async () => {
					calls += 1;
					if (fail) throw new ApiError({ status: 503, code: "internal", message: "temporarily unavailable" });
					return workflowResponse();
				},
			}),
			onUpdate: () => {
				updates += 1;
			},
			now: () => clock,
			workflowCooldownMs: 10_000,
		});

		expect(loader.stageOf(ITEM).loading).toBe(true);
		await flush();
		// Transient failure: still "loading", never "missing".
		const afterFailure = loader.stageOf(ITEM);
		expect(afterFailure.missing).toBe(false);
		expect(afterFailure.loading).toBe(true);
		expect(calls).toBe(1);

		// Inside the cooldown, more renders do not retry.
		clock = 1_000;
		for (let index = 0; index < 10; index += 1) loader.stageOf(ITEM);
		await flush();
		expect(calls).toBe(1);

		// After the cooldown the next render retries; this time it succeeds.
		fail = false;
		clock = 10_000;
		expect(loader.stageOf(ITEM).loading).toBe(true);
		await flush();
		expect(calls).toBe(2);
		expect(updates).toBe(1);
		const loaded = loader.stageOf(ITEM);
		expect(loaded.available).toBe(true);
		expect(loaded.missing).toBe(false);

		// The definition is cached: no further requests.
		for (let index = 0; index < 5; index += 1) loader.stageOf(ITEM);
		await flush();
		expect(calls).toBe(2);
	});
});

describe("acceptanceOf detail loading", () => {
	it("bounds a failing selected-item fetch to one request per cooldown", async () => {
		let clock = 0;
		let calls = 0;
		let updates = 0;
		const loader = createLazyLoader({
			api: fakeApi({
				getItem: async () => {
					calls += 1;
					throw new ApiError({ status: 500, code: "internal", message: "boom" });
				},
			}),
			onUpdate: () => {
				updates += 1;
			},
			now: () => clock,
			detailCooldownMs: 5_000,
		});

		// Simulate a render loop: every call looks at the selected item.
		for (let index = 0; index < 25; index += 1) loader.acceptanceOf(ITEM, ITEM.id);
		await flush();
		expect(calls).toBe(1);

		// Still failing, still cooling down: no storm.
		for (let index = 0; index < 25; index += 1) loader.acceptanceOf(ITEM, ITEM.id);
		await flush();
		expect(calls).toBe(1);
		expect(updates).toBe(0);

		// After the cooldown the next render retries exactly once.
		clock = 5_000;
		loader.acceptanceOf(ITEM, ITEM.id);
		await flush();
		expect(calls).toBe(2);

		// A version bump is real new data: it retries immediately, not after the
		// cooldown that belongs to the old version.
		const bumped = { ...ITEM, version: ITEM.version + 1 };
		clock = 5_001;
		loader.acceptanceOf(bumped, ITEM.id);
		await flush();
		expect(calls).toBe(3);
	});

	it("caches a successful detail and renders once", async () => {
		let calls = 0;
		let updates = 0;
		const loader = createLazyLoader({
			api: fakeApi({
				getItem: async () => {
					calls += 1;
					return detailResponse(ITEM.version);
				},
			}),
			onUpdate: () => {
				updates += 1;
			},
		});

		expect(loader.acceptanceOf(ITEM, ITEM.id)).toBeUndefined();
		expect(loader.acceptanceOf(ITEM, ITEM.id)).toBeUndefined();
		await flush();
		expect(calls).toBe(1);
		expect(updates).toBe(1);
		const progress = loader.acceptanceOf(ITEM, ITEM.id);
		expect(progress?.met).toBe(1);
		expect(progress?.total).toBe(3);
		expect(loader.detailFor(ITEM.id)?.acceptance).toHaveLength(3);

		// Cached: no further request and no further render callback.
		await flush();
		expect(calls).toBe(1);
		expect(updates).toBe(1);
	});

	it("does not loop when the server reports a different item version", async () => {
		let calls = 0;
		const loader = createLazyLoader({
			api: fakeApi({
				getItem: async () => {
					calls += 1;
					// A skewed clock or a mid-write read can disagree with the snapshot.
					return detailResponse(ITEM.version + 99);
				},
			}),
			onUpdate: () => {
				// Simulate the worst case: the render callback immediately asks again.
				loader.acceptanceOf(ITEM, ITEM.id);
			},
		});
		loader.acceptanceOf(ITEM, ITEM.id);
		await flush();
		expect(calls).toBe(1);
		// The cache is keyed by the version we asked for, so the snapshot stays
		// fresh and the mismatch cannot become a reload loop.
		expect(loader.acceptanceOf(ITEM, ITEM.id)).toBeDefined();
		await flush();
		expect(calls).toBe(1);
	});

	it("does not load details for unselected items", async () => {
		let calls = 0;
		const loader = createLazyLoader({
			api: fakeApi({
				getItem: async () => {
					calls += 1;
					return detailResponse(ITEM.version);
				},
			}),
			onUpdate: () => undefined,
		});
		expect(loader.acceptanceOf(ITEM, "another-item")).toBeUndefined();
		await flush();
		expect(calls).toBe(0);
	});
});
