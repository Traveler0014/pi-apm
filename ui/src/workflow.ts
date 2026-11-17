/**
 * Workflow-aware presentation of one item.
 *
 * The stage bar and the branch list are derived from the item's **snapshot**
 * workflow (`workflow_id`/`workflow_version` at creation), never from the six
 * default states: a project may declare any slugs and any categories. When the
 * definition is missing (legacy row, deleted version, server without the API) the
 * model degrades to a "no workflow" marker instead of guessing.
 */

import type { WorkflowDef, WorkItem } from "./types";

/** `undefined` = still loading, `null` = known missing. */
export type WorkflowLookup = (id: string, version: number) => WorkflowDef | null | undefined;

export interface Stage {
	key: string;
	label: string;
	category: string;
	terminal: boolean;
	current: boolean;
	/** One transition away from the current state. */
	next: boolean;
}

export interface Branch {
	to: string;
	label: string;
	category: string;
	/** Human-readable guard requirements, e.g. "needs reason". */
	requires: string[];
}

export interface StageModel {
	available: boolean;
	missing: boolean;
	loading: boolean;
	workflowId: string;
	workflowVersion: number;
	stages: Stage[];
	current: Stage | null;
	branches: Branch[];
	/** True when the item's state is not declared by its own workflow (data drift). */
	unknownState: boolean;
	/** Number of declared states (used by the compact badge). */
	stateCount: number;
}

export const GUARD_LABELS: Record<string, string> = {
	reason: "needs reason",
	evidence_or_acceptance: "needs evidence or acceptance",
	progress_or_note: "needs progress or note",
	completion_evidence: "needs evidence or acceptance",
	completion_progress: "needs progress or note",
	why: "needs reason",
};

export function guardLabel(guard: string): string {
	return GUARD_LABELS[guard] ?? guard.replaceAll("_", " ");
}

export function workflowKey(id: string, version: number): string {
	return `${id}@${version}`;
}

/**
 * Build the stage bar/branch model for an item.
 *
 * `loading` is only true while the definition is still being fetched, so the UI
 * can show "loading stages" instead of "no workflow" during the first paint.
 */
export function stageModel(item: Pick<WorkItem, "workflow_id" | "workflow_version" | "status">, lookup: WorkflowLookup): StageModel {
	const workflowId = item.workflow_id ?? "";
	const workflowVersion = item.workflow_version ?? 0;
	const base: StageModel = {
		available: false,
		missing: false,
		loading: false,
		workflowId,
		workflowVersion,
		stages: [],
		current: null,
		branches: [],
		unknownState: false,
		stateCount: 0,
	};
	if (!workflowId) return { ...base, missing: true };

	const definition = lookup(workflowId, workflowVersion);
	if (definition === undefined) return { ...base, loading: true };
	if (definition === null) return { ...base, missing: true };

	const nextKeys = new Set((definition.transitions ?? []).filter((edge) => edge.from === item.status).map((edge) => edge.to));
	const stages: Stage[] = (definition.states ?? []).map((state) => ({
		key: state.key,
		label: state.label || state.key,
		category: state.category,
		terminal: Boolean(state.terminal),
		current: state.key === item.status,
		next: nextKeys.has(state.key),
	}));
	const current = stages.find((stage) => stage.current) ?? null;
	const branches: Branch[] = (definition.transitions ?? [])
		.filter((edge) => edge.from === item.status)
		.map((edge) => {
			const target = stages.find((stage) => stage.key === edge.to);
			return {
				to: edge.to,
				label: target?.label ?? edge.to,
				category: target?.category ?? "open",
				requires: (edge.requires ?? []).map(guardLabel),
			};
		});

	return {
		...base,
		available: true,
		stages,
		current,
		branches,
		unknownState: current === null,
		stateCount: stages.length,
	};
}

/** Compact stage badge text for a queue row: `reviewing 2/4` or `no workflow`. */
export function stageBadge(model: StageModel): string {
	if (model.loading) return "stages…";
	if (model.missing) return "no workflow";
	if (model.unknownState) return `${"?"} unknown stage`;
	const index = model.stages.findIndex((stage) => stage.current);
	return `${model.current?.label ?? "?"} ${index + 1}/${model.stages.length}`;
}

/** Symbol + word for a workflow category (status is never colour-only). */
export function categoryMark(category: string): { symbol: string; label: string } {
	switch (category) {
		case "open":
			return { symbol: "○", label: "open" };
		case "active":
			return { symbol: "●", label: "active" };
		case "waiting":
			return { symbol: "◆", label: "waiting" };
		case "review":
			return { symbol: "◎", label: "review" };
		case "closed":
			return { symbol: "✓", label: "closed" };
		default:
			return { symbol: "·", label: category };
	}
}

export interface AcceptanceProgress {
	met: number;
	waived: number;
	open: number;
	total: number;
	/** 0..1, used for the progress bar width. */
	ratio: number;
	text: string;
}

/** Acceptance progress for a detail payload; `undefined` while unknown. */
export function acceptanceProgress(
	criteria: Array<{ status: string }> | undefined | null,
): AcceptanceProgress | undefined {
	if (!criteria) return undefined;
	const total = criteria.length;
	const met = criteria.filter((row) => row.status === "met").length;
	const waived = criteria.filter((row) => row.status === "waived").length;
	const open = total - met - waived;
	const done = met + waived;
	return {
		met,
		waived,
		open,
		total,
		ratio: total === 0 ? 1 : done / total,
		text: total === 0 ? "no acceptance criteria" : `${done}/${total} done${open > 0 ? ` · ${open} open` : ""}`,
	};
}
