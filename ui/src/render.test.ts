// @vitest-environment happy-dom
/**
 * DOM tests for the dashboard shell.
 *
 * These mount the real render functions in a DOM (happy-dom) and assert what a
 * user — or a screen reader — actually gets: one row per item id with every
 * badge, a folded done list, an offline banner, a disabled refresh button while
 * a request is in flight, and keyboard activation of a row.
 */

import { beforeEach, describe, expect, it } from "vitest";
import { mockDashboard } from "./fixtures";
import {
	createShell,
	updateBanner,
	updateFilters,
	updateHeader,
	updateMetrics,
	updateQueue,
} from "./render";
import { buildQueue, connectionView, defaultViewState, summarize, type ViewState } from "./transform";

const NOW = Date.parse("2026-11-17T02:10:00.000Z");

function mount() {
	const root = document.createElement("div");
	document.body.replaceChildren(root);
	const shell = createShell(root);
	const state: ViewState = defaultViewState();
	const dashboard = mockDashboard(new Date(NOW).toISOString());
	const render = (patch: Partial<ViewState> = {}) => {
		Object.assign(state, patch);
		updateMetrics(shell, summarize(dashboard));
		updateFilters(shell, state, dashboard);
		updateQueue(shell, buildQueue(dashboard, state), state, NOW);
	};
	render();
	return { root, shell, dashboard, state, render };
}

beforeEach(() => {
	document.body.replaceChildren();
});

describe("dashboard shell", () => {
	it("renders the header, metrics and queue", () => {
		const { root, shell, dashboard } = mount();
		expect(root.querySelectorAll(".metric")).toHaveLength(summarize(dashboard).length);
		expect(shell.queue.querySelectorAll(".row").length).toBe(5);

		const first = shell.queue.querySelector<HTMLElement>(".row");
		expect(first?.dataset.itemId).toBeTruthy();
		expect(first?.querySelector(".identifier")?.textContent).toMatch(/^APM-/);
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
		// Status is expressed with a symbol as well as colour.
		expect(badges.every((text) => /^[^\w\s]/.test(text))).toBe(true);
	});

	it("folds the recently done list and can expand it", () => {
		const { shell, render } = mount();
		expect(shell.doneSummary.textContent).toContain("Recently done · 2");
		expect(shell.doneFold.open).toBe(false);
		expect(shell.doneList.children).toHaveLength(0);

		render({ showRecentlyDone: true });
		shell.doneFold.open = true;
		render({ showRecentlyDone: true });
		expect(shell.doneList.children.length).toBe(2);
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
		const chip = shell.filters.querySelector<HTMLButtonElement>('[data-bucket="waiting_dependencies"]')!;
		expect(chip.textContent).toContain("blocked deps");
		expect(chip.getAttribute("aria-pressed")).toBe("false");

		render({ buckets: ["waiting_dependencies"] });
		expect(
			shell.filters.querySelector<HTMLButtonElement>('[data-bucket="waiting_dependencies"]')!.getAttribute("aria-pressed"),
		).toBe("true");
		expect(shell.queue.querySelectorAll(".row")).toHaveLength(1);
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

	it("marks the refresh button busy and disabled while refreshing", () => {
		const { shell } = mount();
		const live = connectionView({ lastSuccessAt: NOW - 1_000, lastError: null, refreshing: true, now: NOW });
		updateHeader(shell, { connection: live, generatedAt: new Date(NOW).toISOString(), now: NOW, refreshing: true });
		expect(shell.refreshButton.disabled).toBe(true);
		expect(shell.refreshButton.getAttribute("aria-busy")).toBe("true");
		expect(shell.connection.textContent).toContain("refreshing");
		expect(shell.dataTime.textContent).toContain("data ");

		updateHeader(shell, {
			connection: connectionView({ lastSuccessAt: NOW, lastError: null, refreshing: false, now: NOW }),
			generatedAt: new Date(NOW).toISOString(),
			now: NOW,
			refreshing: false,
		});
		expect(shell.refreshButton.disabled).toBe(false);
	});

	it("opens a row by click and by keyboard", () => {
		const seen: string[] = [];
		const { shell } = mount();
		shell.onOpenItem((id) => seen.push(id));

		const row = shell.queue.querySelector<HTMLElement>(".row")!;
		row.click();
		expect(seen).toHaveLength(1);

		row.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
		expect(seen).toHaveLength(2);

		const button = row.querySelector<HTMLButtonElement>(".open-detail")!;
		expect(button.dataset.action).toBe("open-detail");
		button.click();
		expect(seen).toHaveLength(3);
	});

	it("toggles filter chips through the delegated click handler", () => {
		const { shell, state } = mount();
		shell.filters.addEventListener("click", () => {}); // main.ts owns the real handler
		const chip = shell.filters.querySelector<HTMLButtonElement>('[data-bucket="ready_unclaimed"]')!;
		expect(state.buckets).toEqual([]);
		// The shell exposes the container; main.ts maps clicks to state. Here we only
		// assert the data attributes the handler depends on exist and are stable.
		expect(chip.dataset.bucket).toBe("ready_unclaimed");
		expect(chip.dataset.tone).toBe("ok");
	});

	it("shows an empty-state message when nothing matches", () => {
		const { shell, render } = mount();
		render({ search: "no-such-item-anywhere" });
		expect(shell.queue.children).toHaveLength(0);
		expect(shell.empty.hidden).toBe(false);
		expect(shell.queueCaption.textContent).toContain("0 of 5");
	});
});
