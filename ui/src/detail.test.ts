// @vitest-environment happy-dom
/**
 * Detail drawer tests: content coverage, keyboard/focus behaviour, non-colour
 * status, copy-to-clipboard and long-content safety.
 */

import { beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError, type Api } from "./api";
import { createDrawer } from "./detail";
import { mockDetail, mockEvents } from "./fixtures";
import type { EventView, ItemDetail } from "./types";

const NOW = Date.parse("2026-11-17T02:20:00.000Z");

function fakeApi(overrides: Partial<Api> = {}, detail: ItemDetail = mockDetail("i-15"), events: EventView[] = mockEvents("APM-15")): Api {
	return {
		getDashboard: vi.fn(),
		getItems: vi.fn(),
		getHealth: vi.fn(),
		getItem: vi.fn(async () => ({ ok: true as const, item: detail })),
		getEvents: vi.fn(async () => ({ ok: true as const, events })),
		hasToken: () => true,
		...overrides,
	} as unknown as Api;
}

function mount(api: Api = fakeApi(), copyText: (text: string) => Promise<boolean> = async () => true) {
	const focusChanges: Array<string | null> = [];
	const drawer = createDrawer({ api, now: () => NOW, copyText, onOpenChange: (id) => focusChanges.push(id) });
	return { drawer, api, focusChanges };
}

/** A row the drawer was opened from, so focus restoration can be asserted. */
function makeRow(): HTMLButtonElement {
	const row = document.createElement("article");
	row.dataset.itemId = "i-15";
	row.tabIndex = 0;
	const button = document.createElement("button");
	button.className = "open-detail";
	button.textContent = "Details";
	row.append(button);
	document.body.append(row);
	return button;
}

beforeEach(() => {
	document.body.replaceChildren();
});

describe("detail drawer content", () => {
	it("renders handover context, acceptance, evidence, dependencies, claim, clocks, events and commands", async () => {
		const { drawer } = mount();
		let opened: string | null = null;
		const button = makeRow();
		button.focus();
		await drawer.open("i-15");

		const text = drawer.element.textContent ?? "";
		// context
		expect(text).toContain("Ship APM-15");
		expect(text).toContain("Header, metrics, queue and filter controls are wired");
		expect(text).toContain("Vanilla TypeScript + Vite, no framework");
		expect(text).toContain("docs/rest.md");
		// acceptance with symbols, not colour only
		expect(text).toContain("✓ met");
		expect(text).toContain("○ open");
		expect(text).toContain("1. Header shows data time");
		// evidence
		expect(text).toContain("vitest ui => 24 tests");
		// dependencies
		expect(text).toContain("No dependency edges.");
		// claim
		expect(text).toContain("pi (agent)");
		expect(text).toContain("Lease");
		// three clocks
		expect(text).toContain("Heartbeat");
		expect(text).toContain("Progress");
		expect(text).toContain("Run sync");
		// attention
		expect(text).toContain("Should the done fold stay collapsed");
		// events
		expect(text).toContain("claimed");
		expect(text).toContain("progress_recorded");
		// a creation event has no previous version: show "→v1", not "v?→v1"
		expect(text).toContain("→v1");
		expect(text).not.toContain("v?→");
		// a revision that did not bump the item version shows no arrow at all
		expect(text).not.toContain("v1→v1");
		// commands
		expect(text).toContain("apm get APM-15 --json");
		expect(text).toContain("apm claim APM-15");

		opened = drawer.currentIdentifier();
		expect(opened).toBe("APM-15");
		expect(drawer.isOpen()).toBe(true);
	});

	it("distinguishes 'no events' from an unavailable event feed", async () => {
		const failing = fakeApi({
			getEvents: (async () => {
				throw new ApiError({ status: 404, code: "not_found", message: "no route" });
			}) as Api["getEvents"],
		});
		const { drawer } = mount(failing);
		await drawer.open("i-15");
		expect(drawer.element.textContent).toContain("Event feed unavailable from this server");
		expect(drawer.element.textContent).toContain("not_found");

		drawer.close();
		const empty = mount(fakeApi({}, mockDetail("i-15"), []));
		await empty.drawer.open("i-15");
		expect(empty.drawer.element.textContent).toContain("No events recorded for this item yet.");
	});

	it("shows dependency edges when they exist", async () => {
		const detail = mockDetail("i-17");
		detail.item.blocked_by = [{ id: "i-16", identifier: "APM-16", title: "Detail drawer", status: "ready" }];
		detail.item.blocks = [{ id: "i-18", identifier: "APM-18", title: "Later task", status: "ready" }];
		const { drawer } = mount(fakeApi({}, detail));
		await drawer.open("i-17");
		const text = drawer.element.textContent ?? "";
		expect(text).toContain("Blocked by");
		expect(text).toContain("APM-16 · ready · Detail drawer");
		expect(text).toContain("Blocks");
		expect(text).toContain("APM-18");
	});

	it("states 'no claim' instead of rendering an empty block", async () => {
		const detail = mockDetail("i-16");
		detail.item.claim = null;
		const { drawer } = mount(fakeApi({}, detail, []));
		await drawer.open("i-16");
		expect(drawer.element.textContent).toContain("No claim: the item is free to take.");
		expect(drawer.element.textContent).toContain("No events");
	});

	it("flags an expired lease in words", async () => {
		const detail = mockDetail("i-14");
		detail.item.claim = { ...detail.item.claim!, status: "active", expired: true };
		const { drawer } = mount(fakeApi({}, detail));
		await drawer.open("i-14");
		expect(drawer.element.textContent).toContain("lease expired");
	});
});

describe("detail drawer keyboard and focus", () => {
	it("moves focus into the dialog, traps Tab and restores focus on Escape", async () => {
		const { drawer } = mount();
		const button = makeRow();
		button.focus();
		await drawer.open("i-15");

		const dialog = drawer.element.querySelector<HTMLElement>('[role="dialog"]')!;
		expect(dialog.getAttribute("aria-modal")).toBe("true");
		expect(dialog.getAttribute("aria-labelledby")).toBe("drawer-title");
		expect(dialog.contains(document.activeElement)).toBe(true);

		// Escape closes and returns focus to the invoking button.
		dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
		expect(drawer.isOpen()).toBe(false);
		expect(document.activeElement).toBe(button);

		// Re-open: focus is inside again.
		await drawer.open("i-15");
		expect(dialog.contains(document.activeElement)).toBe(true);
	});

	it("wraps Tab from the last focusable element to the first", async () => {
		const { drawer } = mount();
		await drawer.open("i-15");
		const dialog = drawer.element.querySelector<HTMLElement>('[role="dialog"]')!;
		const focusable = [...dialog.querySelectorAll<HTMLElement>("button, summary")];
		const last = focusable[focusable.length - 1]!;
		last.focus();
		const event = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
		dialog.dispatchEvent(event);
		expect(event.defaultPrevented).toBe(true);
		expect(document.activeElement).toBe(focusable[0]);
	});

	it("closes on a backdrop click but not on a click inside the dialog", async () => {
		const { drawer } = mount();
		await drawer.open("i-15");
		const dialog = drawer.element.querySelector<HTMLElement>('[role="dialog"]')!;
		dialog.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
		expect(drawer.isOpen()).toBe(true);
		drawer.element.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
		expect(drawer.isOpen()).toBe(false);
	});
});

describe("detail drawer copying commands", () => {
	it("copies the command text and announces it", async () => {
		const copied: string[] = [];
		const { drawer } = mount(fakeApi(), async (text) => {
			copied.push(text);
			return true;
		});
		await drawer.open("i-15");
		const copy = drawer.element.querySelector<HTMLButtonElement>('button.copy[data-command]')!;
		copy.click();
		await new Promise((resolve) => setTimeout(resolve, 0));
		expect(copied).toEqual(["apm get APM-15 --json"]);
		const status = drawer.element.querySelector('[role="status"]');
		expect(status?.textContent).toContain("Copied: apm get APM-15 --json");
	});

	it("reports a failed copy instead of pretending it worked", async () => {
		const { drawer } = mount(fakeApi(), async () => false);
		await drawer.open("i-15");
		drawer.element.querySelector<HTMLButtonElement>('button.copy[data-command]')!.click();
		await new Promise((resolve) => setTimeout(resolve, 0));
		expect(drawer.element.querySelector('[role="status"]')?.textContent).toContain("Copy failed");
	});

	it("copies every command with the identifier and no credentials", async () => {
		const copied: string[] = [];
		const { drawer } = mount(fakeApi(), async (text) => {
			copied.push(text);
			return true;
		});
		await drawer.open("i-15");
		for (const button of drawer.element.querySelectorAll<HTMLButtonElement>("button.copy")) button.click();
		await new Promise((resolve) => setTimeout(resolve, 0));
		expect(copied).toHaveLength(6);
		for (const command of copied) {
			expect(command).toContain("APM-15");
			expect(command).not.toMatch(/--token|--session|apm_ui_/);
		}
	});
});

describe("detail drawer resilience", () => {
	it("shows an inline error with retry and recovers", async () => {
		let attempts = 0;
		const api = fakeApi({
			getItem: (async (id: string) => {
				attempts += 1;
				if (attempts === 1) {
					throw new ApiError({ status: 404, code: "not_found", message: "item not found" });
				}
				return { ok: true as const, item: mockDetail(id) };
			}) as Api["getItem"],
			getEvents: async () => ({ ok: true as const, events: [] }),
		});
		const { drawer } = mount(api);
		await drawer.open("i-15");
		expect(drawer.element.textContent).toContain("Could not load this item");
		expect(drawer.element.textContent).toContain("not_found");
		const retry = [...drawer.element.querySelectorAll("button")].find((button) => button.textContent === "Retry")!;
		retry.click();
		await new Promise((resolve) => setTimeout(resolve, 0));
		expect(drawer.element.textContent).toContain("Ship APM-15");
	});

	it("renders ledger text as text, never as markup", async () => {
		const detail = mockDetail("i-15");
		detail.context.doc.goal = '<script>alert("xss")</script> & <img src=x onerror=alert(1)>';
		detail.item.title = "<b>bold</b>";
		const { drawer } = mount(fakeApi({}, detail));
		await drawer.open("i-15");
		expect(drawer.element.querySelector("script")).toBeNull();
		expect(drawer.element.querySelector("img")).toBeNull();
		expect(drawer.element.querySelector("b")).toBeNull();
		expect(drawer.element.textContent).toContain('<script>alert("xss")</script>');
	});

	it("clamps very long values behind a disclosure", async () => {
		const detail = mockDetail("i-15");
		detail.item.progress_summary = "x".repeat(2_000);
		const { drawer } = mount(fakeApi({}, detail));
		await drawer.open("i-15");
		const clamped = drawer.element.querySelector("details.clamped");
		expect(clamped).not.toBeNull();
		expect(clamped!.querySelector("summary")?.textContent?.length).toBeLessThan(320);
	});
});
