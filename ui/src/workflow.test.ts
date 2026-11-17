import { describe, expect, it } from "vitest";
import { MOCK_WORKFLOWS } from "./fixtures";
import type { ItemDetail, WorkItem, WorkflowDef } from "./types";
import { acceptanceProgress, categoryMark, guardLabel, stageBadge, stageModel, workflowKey } from "./workflow";

const lookup = (id: string, version: number): WorkflowDef | null | undefined =>
	MOCK_WORKFLOWS[workflowKey(id, version)] ?? null;

function item(patch: Partial<WorkItem> = {}): Pick<WorkItem, "workflow_id" | "workflow_version" | "status"> {
	return { workflow_id: "apm-default", workflow_version: 1, status: "doing", ...patch };
}

describe("stageModel", () => {
	it("renders the stages of the item's own workflow and marks current/next", () => {
		const model = stageModel(item(), lookup);
		expect(model.available).toBe(true);
		expect(model.stages.map((stage) => stage.key)).toEqual(["ready", "doing", "blocked", "review", "done", "cancelled"]);
		expect(model.current?.key).toBe("doing");
		expect(model.stages.filter((stage) => stage.next).map((stage) => stage.key)).toEqual(["review", "done"]);
		expect(model.unknownState).toBe(false);
		expect(model.stateCount).toBe(6);
	});

	it("uses the item's custom workflow, not the default six states", () => {
		const model = stageModel(item({ workflow_id: "review-flow", version: 1, status: "reviewing" } as never), lookup);
		expect(model.stages.map((stage) => stage.key)).toEqual(["triage", "reviewing", "shipped", "dropped"]);
		expect(stageBadge(model)).toBe("Reviewing 2/4");
		expect(model.branches.map((branch) => branch.to)).toEqual(["shipped", "dropped"]);
	});

	it("lists branches with human-readable guard requirements", () => {
		const model = stageModel(item({ status: "reviewing", workflow_id: "review-flow", workflow_version: 1 }), lookup);
		const shipped = model.branches.find((branch) => branch.to === "shipped")!;
		expect(shipped.requires).toEqual(["needs evidence or acceptance", "needs progress or note"]);
		const dropped = model.branches.find((branch) => branch.to === "dropped")!;
		expect(dropped.requires).toEqual(["needs reason"]);
		expect(dropped.category).toBe("closed");
	});

	it("distinguishes loading from missing definitions", () => {
		const loading = stageModel(item(), () => undefined);
		expect(loading.loading).toBe(true);
		expect(loading.missing).toBe(false);
		expect(stageBadge(loading)).toBe("stages…");

		const missing = stageModel(item({ workflow_id: "gone", workflow_version: 9 }), lookup);
		expect(missing.missing).toBe(true);
		expect(missing.available).toBe(false);
		expect(stageBadge(missing)).toBe("no workflow");

		const unset = stageModel(item({ workflow_id: "" } as never), lookup);
		expect(unset.missing).toBe(true);
	});

	it("flags a state the workflow does not declare (data drift) without inventing stages", () => {
		const model = stageModel(item({ status: "vanished" }), lookup);
		expect(model.unknownState).toBe(true);
		expect(model.current).toBeNull();
		expect(model.stages.length).toBe(6);
		expect(stageBadge(model)).toBe("? unknown stage");
		expect(model.branches).toEqual([]);
	});
});

describe("helpers", () => {
	it("names guards and categories for humans", () => {
		expect(guardLabel("evidence_or_acceptance")).toBe("needs evidence or acceptance");
		expect(guardLabel("completion_progress")).toBe("needs progress or note");
		expect(guardLabel("custom_guard")).toBe("custom guard");
		expect(categoryMark("closed")).toEqual({ symbol: "✓", label: "closed" });
		expect(categoryMark("weird").label).toBe("weird");
	});

	it("computes acceptance progress from any closed category", () => {
		const criteria: ItemDetail["acceptance"] = [
			{ seq: 1, text: "a", status: "met" },
			{ seq: 2, text: "b", status: "waived" },
			{ seq: 3, text: "c", status: "open" },
		];
		const progress = acceptanceProgress(criteria)!;
		expect(progress).toMatchObject({ met: 1, waived: 1, open: 1, total: 3 });
		expect(progress.text).toBe("2/3 done · 1 open");
		expect(progress.ratio).toBeCloseTo(2 / 3);
		expect(acceptanceProgress([])!.text).toBe("no acceptance criteria");
		expect(acceptanceProgress(undefined)).toBeUndefined();
	});
});
