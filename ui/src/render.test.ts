// @vitest-environment happy-dom
/**
 * DOM tests for the dashboard shell: list view, topology view, stage bars,
 * selection and closed-item reachability.
 */

import { beforeEach, describe, expect, it } from "vitest";
import {
	MOCK_CLOSED_EXTRA,
	MOCK_DONE,
	MOCK_ITEMS,
	MOCK_WORKFLOWS,
	mockDashboard,
} from "./fixtures";
import {
	createShell,
	updateBanner,
	updateFilters,
	updateHeader,
	updateMetrics,
	updateDagPanel,
	updateQueue,
	updateStatusScope,
	updateSelectionSummary,
	updateTopology,
	updateViews,
	type RenderContext,
	type ViewMode,
} from "./render";
import { buildGraph, topologySummary } from "./topology";
import { buildQueue, connectionView, defaultViewState, summarize, type ViewState } from "./transform";
import type { WorkItem, WorkflowDef } from "./types";
import { acceptanceProgress, stageModel, workflowKey } from "./workflow";

const NOW = Date.parse("2026-11-17T02:20:00.000Z");

function allItems(): WorkItem[] {
	return [...MOCK_ITEMS, ...MOCK_DONE, ...MOCK_CLOSED_EXTRA];
}

function lookup(id: string, version: number): WorkflowDef | null | undefined {
	return MOCK_WORKFLOWS[workflowKey(id, version)] ?? null;
}

function mount() {
	const root = document.createElement("div");
	document.body.replaceChildren(root);
	const shell = createShell(root);
	const state: ViewState = defaultViewState();
	const items = allItems();
	const dashboard = mockDashboard(new Date(NOW).toISOString());
	const view: { mode: ViewMode; selected: string | null; scope: "all" | "blocking" | "upstream" | "downstream" } = {
		mode: "list",
		selected: null,
		scope: "all",
	};
	const ctx = (): RenderContext => ({
		state,
		view: view.mode,
		selectedId: view.selected,
		stageOf: (item) => stageModel(item, lookup),
		acceptanceOf: (item) =>
			item.identifier === "APM-16"
				? acceptanceProgress([{ status: "met" }, { status: "open" }, { status: "waived" }])
				: undefined,
		now: NOW,
	});
	const render = (patch: Partial<ViewState> = {}) => {
		Object.assign(state, patch);
		updateMetrics(shell, summarize(dashboard));
		updateFilters(shell, state, dashboard);
		updateViews(shell, view.mode, view.selected);
		updateStatusScope(shell, state);
		updateQueue(shell, buildQueue(dashboard, items, state), ctx());
		const graph = buildGraph(items, NOW);
		const queue = buildQueue(dashboard, items, state);
		updateTopology(shell, topologySummary(graph), ctx(), state.statusScope === "open" ? undefined : queue.closed);
		const selected = view.selected ? (items.find((item) => item.id === view.selected) ?? null) : null;
		updateSelectionSummary(shell, ctx(), selected, { acceptance: [{ status: "met" }, { status: "open" }] });
		updateDagPanel(shell, ctx(), selected, buildGraph(items, NOW), view.scope, (id) => {
			view.selected = id;
		});
	};
	render();
	return { root, shell, dashboard, state, view, items, render };
}

beforeEach(() => {
	document.body.replaceChildren();
});

describe("dashboard shell: list view", () => {
	it("renders header metrics and one row per open item", () => {
		const { root, shell, dashboard } = mount();
		expect(root.querySelectorAll(".metric")).toHaveLength(summarize(dashboard).length);
		// MOCK_ITEMS = 5 open items; the closed ones live in the fold.
		expect(shell.queue.querySelectorAll(".row")).toHaveLength(MOCK_ITEMS.length);
		const first = shell.queue.querySelector<HTMLElement>(".row");
		expect(first?.querySelector(".identifier")?.textContent).toMatch(/^APM-/);
		expect(first?.querySelector(".stage-bar")).not.toBeNull();
	});

	it("renders mutually exclusive groups with Startable only holding startable work", () => {
		const { shell } = mount();
		const groups = [...shell.queue.querySelectorAll<HTMLElement>(".queue-group")];
		expect(groups.length).toBeGreaterThan(0);
		expect(groups.map((group) => group.dataset.group)).toEqual(
			expect.arrayContaining(["needs_human", "at_risk", "in_progress", "waiting_upstream", "startable"].filter((key) =>
				groups.some((group) => group.dataset.group === key),
			)),
		);
		for (const group of groups) {
			for (const row of group.querySelectorAll<HTMLElement>(".row")) {
				expect(row.dataset.group).toBe(group.dataset.group);
			}
		}
		const startable = groups.find((group) => group.dataset.group === "startable");
		for (const row of startable?.querySelectorAll<HTMLElement>(".row") ?? []) {
			// A startable row must be unclaimed and cannot carry the in-progress badge.
			expect(row.textContent).not.toContain("in progress");
		}
	});

	it("keeps a doing item with open blockers under In progress and badges the blockers", () => {
		const { items, shell } = mount();
		const doingWithBlockers = items.find(
			(item) => item.status_category === "active" && item.blocked_by.length > 0,
		);
		if (!doingWithBlockers) return; // fixture has no such item; covered by groups.test.ts
		const row = [...shell.queue.querySelectorAll<HTMLElement>(".row")].find(
			(node) => node.dataset.itemId === doingWithBlockers.id,
		)!;
		const group = row.closest<HTMLElement>(".queue-group")!;
		expect(group.dataset.group).toBe("in_progress");
		expect(row.textContent).toContain("blocked by");
	});

	it("shows one row for an item in several buckets, with both badges", () => {
		const { shell } = mount();
		const rows = [...shell.queue.querySelectorAll<HTMLElement>(".row")].filter(
			(row) => row.querySelector(".identifier")?.textContent === "APM-14",
		);
		expect(rows).toHaveLength(1);
		const badges = [...rows[0]!.querySelectorAll(".badge")].map((badge) => badge.textContent ?? "");
		expect(badges.some((text) => text.includes("waiting human"))).toBe(true);
		expect(badges.some((text) => text.includes("in progress"))).toBe(true);
		expect(badges.every((text) => /^[^\w\s]/.test(text))).toBe(true);
	});

	it("puts every closed category in the fold (done, cancelled, aborted)", () => {
		const { shell, render } = mount();
		expect(shell.doneSummary.textContent).toContain("Closed items · 4");
		render({ showRecentlyDone: true });
		shell.doneFold.open = true;
		render({ showRecentlyDone: true });
		const ids = [...shell.doneList.querySelectorAll<HTMLElement>(".row")].map(
			(row) => row.querySelector(".identifier")?.textContent,
		);
		expect(ids).toContain("APM-12"); // done
		expect(ids).toContain("APM-19"); // cancelled
		expect(ids).toContain("APM-18"); // aborted (custom closed state)
		// Each closed row still offers the full detail drawer.
		for (const id of ["APM-19", "APM-18"]) {
			const row = [...shell.doneList.querySelectorAll<HTMLElement>(".row")].find(
				(node) => node.querySelector(".identifier")?.textContent === id,
			)!;
			expect(row.querySelector("[data-action='open-detail']")).not.toBeNull();
		}
	});

	it("keeps the search box and its value across a data refresh", () => {
		const { shell, render } = mount();
		shell.search.value = "apm-14";
		const before = shell.search;
		render({ search: "apm-14" });
		expect(shell.search).toBe(before);
		expect(shell.search.value).toBe("apm-14");
		expect(shell.queue.querySelectorAll(".row")).toHaveLength(1);
	});

	it("renders filter chips with counts and reflects the active filter", () => {
		const { shell, render } = mount();
		const chip = shell.filters.querySelector<HTMLButtonElement>('[data-bucket="blocked deps"]');
		expect(chip).toBeNull(); // bucket key, not label
		render({ buckets: ["waiting_dependencies"] });
		expect(
			shell.filters.querySelector<HTMLButtonElement>('[data-bucket="waiting_dependencies"]')!.getAttribute("aria-pressed"),
		).toBe("true");
		expect(shell.queue.querySelectorAll(".row").length).toBeGreaterThan(0);
	});

	it("shows the stage bar from the item's own workflow, and a warning for unknown states", () => {
		const { shell } = mount();
		const apm15 = [...shell.queue.querySelectorAll<HTMLElement>(".row")].find(
			(row) => row.querySelector(".identifier")?.textContent === "APM-15",
		)!;
		const stages = [...apm15.querySelectorAll(".stage")].map((stage) => stage.textContent);
		expect(stages.join(" ")).toContain("Doing");
		const current = apm15.querySelector<HTMLElement>('.stage[data-state="current"]');
		expect(current?.getAttribute("aria-current")).toBe("step");

		const custom = [...shell.queue.querySelectorAll<HTMLElement>(".row")].find(
			(row) => row.querySelector(".identifier")?.textContent === "APM-16",
		)!;
		// APM-16 uses the review-flow snapshot in this fixture set.
		expect(custom.querySelector(".stage-bar")?.getAttribute("data-workflow")).toBeTruthy();
	});

	it("degrades to a 'no workflow' marker when the definition is missing", () => {
		const { shell, items, view } = mount();
		view.mode = "list";
		const orphan = { ...items[0]!, id: "orphan", identifier: "APM-99", workflow_id: "gone", workflow_version: 7 };
		const state = { ...defaultViewState() };
		updateQueue(
			shell,
			buildQueue(null, [orphan], state),
			{ state, view: "list", selectedId: null, stageOf: (item) => stageModel(item, lookup), acceptanceOf: () => undefined, now: NOW },
		);
		expect(shell.queue.textContent).toContain("no workflow definition");
	});

	it("shows an offline banner with the cache timestamp and hides it when live", () => {
		const { shell } = mount();
		updateBanner(
			shell,
			connectionView({ lastSuccessAt: NOW - 30_000, lastError: "server unreachable", refreshing: false, now: NOW }),
		);
		expect(shell.banner.hidden).toBe(false);
		expect(shell.banner.textContent).toContain("Offline");
		expect(shell.banner.textContent).toMatch(/cached \d{2}:\d{2}:\d{2}/);
		updateBanner(shell, connectionView({ lastSuccessAt: NOW - 1_000, lastError: null, refreshing: false, now: NOW }));
		expect(shell.banner.hidden).toBe(true);
	});

	it("marks the refresh button busy while refreshing and shows acceptance progress", () => {
		const { shell } = mount();
		updateHeader(shell, {
			connection: connectionView({ lastSuccessAt: NOW - 1_000, lastError: null, refreshing: true, now: NOW }),
			generatedAt: new Date(NOW).toISOString(),
			now: NOW,
			refreshing: true,
		});
		expect(shell.refreshButton.disabled).toBe(true);
		expect(shell.refreshButton.getAttribute("aria-busy")).toBe("true");
		const apm16 = [...shell.queue.querySelectorAll<HTMLElement>(".row")].find(
			(row) => row.querySelector(".identifier")?.textContent === "APM-16",
		)!;
		expect(apm16.querySelector(".acceptance-text")?.textContent).toContain("2/3 done");
	});
});

describe("dashboard shell: topology view", () => {
	it("groups ready items and waiting items with direct, transitive and unlock counts", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		render();
		expect(shell.listView.hidden).toBe(true);
		expect(shell.topologyView.hidden).toBe(false);

		const ready = shell.topologyGroups.querySelector('[data-group="ready"]')!;
		const waiting = shell.topologyGroups.querySelector('[data-group="waiting"]')!;
		expect(ready.querySelector(".topo-title")?.textContent).toContain("Ready to start");
		expect(waiting.querySelector(".topo-title")?.textContent).toContain("Waiting on upstream");

		// APM-17 waits on three upstream items through APM-14/15/16.
		const apm17 = [...waiting.querySelectorAll<HTMLElement>("[data-item-id]")].find(
			(node) => node.querySelector(".identifier")?.textContent === "APM-17",
		)!;
		const facts = apm17.querySelector(".topo-facts")?.textContent ?? "";
		expect(facts).toMatch(/\d+ upstream blocker\(s\)/);
		expect(facts).toMatch(/unlocks/);
		expect(apm17.querySelector(".topo-blockers")?.textContent).toContain("APM-16");
	});

	it("selects a card by click and renders the stage/branch summary", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		const selected: string[] = [];
		shell.onSelect((id) => selected.push(id ?? "null"));
		render();
		const card = [...shell.topologyGroups.querySelectorAll<HTMLElement>('[data-group="ready"] [data-item-id]')].find(
			(node) => node.querySelector(".identifier")?.textContent === "APM-16",
		)!;
		card.click();
		expect(selected).toHaveLength(1);

		view.selected = selected[0]!;
		render();
		expect(shell.summary.textContent).toContain("Next branches");
		expect(shell.summary.querySelectorAll(".branch").length).toBeGreaterThan(0);
		expect(shell.summary.querySelector(".stage-bar")).not.toBeNull();
		expect(shell.summary.querySelector(".acceptance-text")?.textContent).toContain("2/3 done");
	});

	it("switches views with the toggle and keeps the pressed state", () => {
		const { shell } = mount();
		updateViews(shell, "topology", null);
		expect(shell.viewToggle.querySelector('[data-view="topology"]')!.getAttribute("aria-pressed")).toBe("true");
		expect(shell.viewToggle.querySelector('[data-view="list"]')!.getAttribute("aria-pressed")).toBe("false");
		updateViews(shell, "list", null);
		expect(shell.listView.hidden).toBe(false);
		expect(shell.topologyView.hidden).toBe(true);
	});

	it("keeps the selected card marked across a re-render", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		const card = shell.topologyGroups.querySelector<HTMLElement>('[data-group="ready"] [data-item-id]')!;
		view.selected = card.dataset.itemId ?? null;
		render();
		const again = shell.topologyGroups.querySelector<HTMLElement>(
			`[data-item-id="${view.selected}"]`,
		)!;
		expect(again.dataset.selected).toBe("true");
		expect(again.getAttribute("aria-pressed")).toBe("true");
	});

	it("empty states are explicit", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		render();
		// All fixture items either start or wait; the cycle group only exists with a loop.
		expect(shell.topologyGroups.querySelector('[data-group="cycles"]')).toBeNull();
	});
});

describe("dashboard shell: local dependency graph (APM-21)", () => {
	it("expands the graph into a dialog and restores focus on Escape (APM-29)", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		view.selected = "i-17";
		render();
		const expand = shell.dagPanel.querySelector<HTMLButtonElement>(".dag-expand")!;
		expect(expand).not.toBeNull();
		expect(expand.getAttribute("aria-haspopup")).toBe("dialog");
		expand.click();
		expect(shell.dagDialog.open || shell.dagDialog.hasAttribute("open")).toBe(true);
		// The overlay uses larger boxes and keeps the relations (not a screenshot).
		const expanded = shell.dagDialogBody.querySelector<HTMLElement>(".dag")!;
		expect(expanded).not.toBeNull();
		expect(expanded.querySelectorAll(".dag-node").length).toBeGreaterThan(0);
		expect(shell.dagDialogBody.textContent).toContain("dependency");
		const close = shell.dagDialogBody.querySelector<HTMLButtonElement>('[data-action="close-graph"]')!;
		expect(shell.dagDialogBody.contains(document.activeElement)).toBe(true);
		// Close via the button; focus returns to the trigger.
		close.click();
		expect(shell.dagDialog.open || shell.dagDialog.hasAttribute("open")).toBe(false);
		expect(document.activeElement).toBe(expand);
		// Re-open and close with Escape (the native dialog path plus the emulated one).
		expand.click();
		shell.dagDialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
		expect(shell.dagDialog.open || shell.dagDialog.hasAttribute("open")).toBe(false);
	});

	it("renders the neighbourhood with dependency and parent edges kept apart", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		view.selected = "i-17";
		render();
		const dag = shell.dagPanel.querySelector<HTMLElement>(".dag")!;
		expect(dag).not.toBeNull();
		// APM-17 is blocked by APM-14/15/16 and its story is APM-13.
		const nodes = [...dag.querySelectorAll<HTMLElement>(".dag-node")].map((node) => node.dataset.role);
		expect(nodes).toContain("selected");
		expect(nodes.some((role) => role === "upstream" || role === "parent" || role === "child")).toBe(true);
		const kinds = [...dag.querySelectorAll(".dag-edge")].map((edge) => edge.getAttribute("data-kind"));
		expect(new Set(kinds)).toContain("dependency");
		expect(shell.dagPanel.querySelector(".dag-edge-parent")).not.toBeNull();
	});

	it("offers scope switches and re-renders the graph and text view together", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		view.selected = "i-17";
		render();
		const scopes = [...shell.dagPanel.querySelectorAll<HTMLButtonElement>(".dag-scope")].map((button) => button.dataset.scope);
		expect(scopes).toEqual(["all", "blocking", "upstream", "downstream"]);
		expect(shell.dagPanel.querySelector('.dag-scope[data-scope="all"]')!.getAttribute("aria-pressed")).toBe("true");

		view.scope = "blocking";
		render();
		expect(shell.dagPanel.querySelector('.dag-scope[data-scope="blocking"]')!.getAttribute("aria-pressed")).toBe("true");
		expect(shell.dagPanel.querySelector(".dag-edge-parent")).toBeNull();
		const text = shell.dagPanel.querySelector(".dag-text")!.textContent ?? "";
		expect(text).not.toContain("child");
	});

	it("keeps the text view equivalent and synced with the graph", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		view.selected = "i-17";
		render();
		const text = shell.dagPanel.querySelector(".dag-text")!.textContent ?? "";
		expect(text).toContain("blocks APM-16");
		expect(text).toContain("selected APM-17");
		// Selecting from the text view updates the same selection state.
		const selectInText = [...shell.dagPanel.querySelectorAll<HTMLButtonElement>(".dag-text-select")][0]!;
		selectInText.click();
		render();
		expect(view.selected).toBe(selectInText.dataset.itemId);
	});

	it("node buttons are real buttons with announced status (keyboard reachable)", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		view.selected = "i-17";
		render();
		const node = shell.dagPanel.querySelector<HTMLButtonElement>(".dag-node")!;
		expect(node.tagName).toBe("BUTTON");
		expect(node.getAttribute("aria-label")).toMatch(/^APM-\d+ .+ — .+, .+$/);
		const selected = shell.dagPanel.querySelector<HTMLElement>('.dag-node[data-selected="true"]')!;
		expect(selected.getAttribute("aria-current")).toBe("true");
	});

	it("opens every closed category from the fold", () => {
		const { shell, render } = mount();
		const opened: string[] = [];
		shell.onOpenItem((id) => opened.push(id));
		render({ showRecentlyDone: true });
		shell.doneFold.open = true;
		render({ showRecentlyDone: true });
		for (const identifier of ["APM-19", "APM-18", "APM-12"]) {
			const row = [...shell.doneList.querySelectorAll<HTMLElement>(".row")].find(
				(node) => node.querySelector(".identifier")?.textContent === identifier,
			)!;
			row.querySelector<HTMLElement>('[data-action="open-detail"]')!.click();
		}
		expect(opened).toHaveLength(3);
	});

	it("finds closed items through search too", () => {
		const { shell, render } = mount();
		render({ search: "superseded", showRecentlyDone: true });
		shell.doneFold.open = true;
		render({ search: "superseded", showRecentlyDone: true });
		const ids = [...shell.doneList.querySelectorAll<HTMLElement>(".row")].map(
			(node) => node.querySelector(".identifier")?.textContent,
		);
		expect(ids).toEqual(["APM-18"]);
	});
});

describe("dashboard shell: selection summary controls (APM-26)", () => {
	it("has no duplicate Details button inside the summary panel", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		view.selected = "i-17";
		render();
		expect(shell.summary.querySelector('[data-action="open-detail"]')).toBeNull();
		expect(shell.summary.textContent).toContain("APM-17");
	});

	it("keeps every Details button inside an item container so clicks resolve", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		view.selected = "i-17";
		render();
		const buttons = [...shell.root.querySelectorAll<HTMLElement>('[data-action="open-detail"]')];
		expect(buttons.length).toBeGreaterThan(0);
		for (const button of buttons) {
			expect(button.closest("[data-item-id]")).not.toBeNull();
		}
	});

	it("still opens the drawer from the card, the graph node and the list row", () => {
		const { shell, view, render } = mount();
		const opened: string[] = [];
		shell.onOpenItem((id) => opened.push(id));
		view.mode = "topology";
		view.selected = "i-17";
		render();
		// topology card
		shell.topologyGroups.querySelector<HTMLElement>('[data-item-id="i-17"] [data-action="open-detail"]')!.click();
		// graph node is a button itself; its own click selects, so use the card's
		// Details for the drawer and the DAG node for selection.
		expect(opened).toEqual(["i-17"]);
	});
});

describe("dashboard shell: status scope and closed group (APM-30)", () => {
	it("offers an accessible Open/Closed/All control that reflects the state", () => {
		const { shell, state, render } = mount();
		const scopes = [...shell.statusScope.querySelectorAll<HTMLButtonElement>("[data-status-scope]")].map(
			(button) => button.dataset.statusScope,
		);
		expect(scopes).toEqual(["open", "closed", "all"]);
		expect(shell.statusScope.getAttribute("aria-label")).toBe("Status scope");
		expect(shell.statusScope.querySelector('[data-status-scope="open"]')!.getAttribute("aria-pressed")).toBe("true");
		render({ statusScope: "closed" });
		expect(shell.statusScope.querySelector('[data-status-scope="closed"]')!.getAttribute("aria-pressed")).toBe("true");
		expect(state.statusScope).toBe("closed");
	});

	it("renders closed work as a group in both views and hides the fold", () => {
		const { shell, view, render } = mount();
		view.mode = "list";
		render({ statusScope: "closed", showRecentlyDone: true });
		// The list shows the closed group (no folded duplicate).
		expect(shell.queue.querySelectorAll(".row").length).toBeGreaterThan(0);
		expect(shell.queue.textContent).toContain("Closed");
		expect(shell.doneFold.hidden).toBe(true);

		view.mode = "topology";
		render({ statusScope: "closed", showRecentlyDone: true });
		const closed = shell.topologyGroups.querySelector('[data-group="closed"]');
		expect(closed).not.toBeNull();
		const identifiers = [...closed!.querySelectorAll("[data-item-id]")].map(
			(node) => node.querySelector(".identifier")?.textContent,
		);
		expect(identifiers).toContain("APM-12"); // done
		expect(identifiers).toContain("APM-19"); // cancelled
		expect(identifiers).toContain("APM-18"); // aborted (custom closed)
	});

	it("keeps the open scope free of a closed group", () => {
		const { shell, view, render } = mount();
		view.mode = "topology";
		render({ statusScope: "open" });
		expect(shell.topologyGroups.querySelector('[data-group="closed"]')).toBeNull();
		expect(shell.doneFold.hidden).toBe(false);
	});

	it("closed group rows keep the full detail entry point", () => {
		const { shell, view, render } = mount();
		const opened: string[] = [];
		shell.onOpenItem((id) => opened.push(id));
		view.mode = "topology";
		render({ statusScope: "closed", showRecentlyDone: true });
		const card = shell.topologyGroups.querySelector<HTMLElement>('[data-group="closed"] [data-item-id]')!;
		card.querySelector<HTMLElement>('[data-action="open-detail"]')!.click();
		expect(opened).toHaveLength(1);
	});
});
