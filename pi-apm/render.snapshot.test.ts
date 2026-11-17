/**
 * Component and snapshot coverage for the `apm-run-sync` TUI card.
 *
 * `render.test.ts` (APM-10) proves the *text* `formatRunSyncReport` produces.
 * This file proves the *component* Pi actually mounts:
 *
 *   - `renderRunSyncEntry` returns a real `Box` whose children are one `Text`
 *     per line (a string return, or a Box built from a single string with
 *     embedded newlines, would silently break layout and is caught here);
 *   - the section covers the acceptance cases APM-11 names explicitly —
 *     success, skip, abort, **timeout**, and **partial reconciliation failure**;
 *   - an inline snapshot pins the expanded shape so field ordering and wording
 *     changes show up in review instead of drifting;
 *   - narrow terminals and malformed data degrade without throwing.
 */

import { Box, Text } from "@earendil-works/pi-tui";
import { describe, expect, it } from "vitest";
import type { RunSyncReportData } from "./lib.ts";
import { renderRunSyncEntry, type RenderTheme } from "./render.ts";

/** Identity theme: no ANSI escape codes, so assertions see the raw text. */
const theme: RenderTheme = {
	fg: (_token: string, text: string) => text,
	bg: (_token: string, text: string) => text,
};

function report(overrides: Partial<RunSyncReportData> = {}): RunSyncReportData {
	return {
		ran: true,
		mode: "side-run",
		reason: "run completed",
		runId: "2026-11-16T12:00:00.000Z-1",
		sessionId: "01a1e223-540c-71eb-99de-dc58f402558c",
		items: ["APM-7"],
		calls: 2,
		childEvents: 120,
		reconciled: [{ identifier: "APM-7", action: "sync", ok: true, code: null }],
		errors: [],
		durationMs: 1234,
		at: "2026-11-16T12:00:02.000Z",
		...overrides,
	};
}

/** Mount the component the way Pi does and collect the rendered lines. */
function mount(data: unknown, expanded = false, width = 80) {
	const component = renderRunSyncEntry({ data }, { expanded }, theme);
	expect(component).toBeInstanceOf(Box);
	// `Box(1, 1, …)` adds one blank line of padding above and below and pads every
	// line to the requested width, so the raw output is not the content itself.
	const lines = component.render(width);
	return { component, lines };
}

function text(data: unknown, expanded = false, width = 120): string {
	return mount(data, expanded, width).lines.join("\n");
}

/**
 * Same rendering with Box's right-padding removed. Snapshots use this so the
 * committed fixture has no trailing whitespace (`git diff --check` stays clean)
 * while the width assertions above still measure the padded output.
 */
function snapshotText(data: unknown, expanded = false, width = 120): string {
	return mount(data, expanded, width)
		.lines.map((line) => line.trimEnd())
		.join("\n");
}

describe("apm-run-sync component", () => {
	it("mounts one Text child per content line, with Box padding around it", () => {
		const { component, lines } = mount(report());
		const children = (component as unknown as { children: unknown[] }).children;
		expect(Array.isArray(children)).toBe(true);
		expect(children.length).toBeGreaterThan(1);
		for (const child of children) expect(child).toBeInstanceOf(Text);
		// Box(1, 1) pads one blank line above and below the content.
		expect(lines[0].trim()).toBe("");
		expect(lines[lines.length - 1].trim()).toBe("");
		expect(lines.length).toBe(children.length + 2);
		// No Text node may smuggle newlines through a single child.
		for (const line of lines) expect(line).not.toContain("\n");
	});

	it("renders success, skip and abort as distinct states", () => {
		const ok = text(report());
		expect(ok).toContain("[apm] run synced");
		expect(ok).toContain("1.2s");
		expect(ok).toContain("✓ APM-7 sync");

		const skipped = text(report({ mode: "skip", ran: false, reason: "nothing to sync", reconciled: [] }));
		expect(skipped).toContain("[apm] skipped");
		expect(skipped).toContain("nothing to sync");
		expect(skipped).not.toContain("✓");

		const aborted = text(report({ mode: "abort-sync", reason: "run aborted by the user" }));
		expect(aborted).toContain("aborted run · liveness only");
		expect(aborted).toContain("run aborted by the user");
	});

	it("surfaces a side-run timeout as a diagnostic, not as silence", () => {
		const timedOut = report({
			ran: false,
			errors: ["side-run timed out after 180000ms; reconciliation recorded the run"],
			reconciled: [],
			output: "",
		});
		const collapsed = text(timedOut);
		expect(collapsed).toContain("run not synced");
		expect(collapsed).toContain("timed out");
		const expanded = text(timedOut, true);
		expect(expanded).toContain("timed out");
		expect(expanded).toContain("180000ms");
	});

	it("reports partial reconciliation failure with codes and counts", () => {
		const partial = report({
			reconciled: [
				{ identifier: "APM-7", action: "sync", ok: true, code: null },
				{ identifier: "APM-8", action: "progress", ok: true, code: null },
				{ identifier: "APM-9", action: "skip", ok: true, code: null },
				{ identifier: "APM-10", action: "diagnostic", ok: false, code: "claim_invalid" },
			],
			errors: ["reconciliation: APM-10 lease expired"],
		});
		const rendered = text(partial, true);
		expect(rendered).toContain("✓ APM-7 sync");
		expect(rendered).toContain("· APM-9 already recorded");
		expect(rendered).toContain("✗ APM-10");
		expect(rendered).toContain("claim_invalid");
		expect(rendered).toContain("1 issue(s)");
	});

	it("collapses overflow and expands it on demand", () => {
		const many = report({
			reconciled: Array.from({ length: 10 }, (_, index) => ({
				identifier: `APM-${index + 1}`,
				action: "sync",
				ok: true,
				code: null,
			})),
			errors: ["e1", "e2", "e3"],
		});
		const collapsed = text(many);
		expect(collapsed).toContain("+6 more item(s)");
		expect(collapsed).toContain("+2 more error(s)");
		const expanded = text(many, true);
		expect(expanded).not.toContain("more item(s)");
	});

	it("matches the expanded snapshot", () => {
		expect(
			snapshotText(report({ errors: [], output: "2026-11-16 item APM-7 synced" }), true),
		).toMatchInlineSnapshot(`
			"
			 [apm] run synced (1.2s · 1 item(s)) · run 2026-11-16T12:00:0…
			   items: APM-7
			 ✓ APM-7 sync
			   2026-11-16 item APM-7 synced
			   at 2026-11-16T12:00:02.000Z · session 01a1e223-540c-71eb… · run 2026-11-16T12:00:0…
			"
		`);
	});

	it("survives narrow widths and malformed data without throwing", () => {
		for (const width of [20, 40, 80, 120]) {
			const { lines } = mount(report({ output: "x".repeat(500) }), true, width);
			expect(lines.length).toBeGreaterThan(0);
			for (const line of lines) {
				// A terminal-width guarantee, not a formatting one: Box pads to exactly
				// `width`, so nothing may overflow it (the identity theme has no ANSI to
				// inflate the count).
				expect(line.length).toBeLessThanOrEqual(width);
				expect(line).not.toContain("\n");
			}
		}
		expect(text(undefined)).toContain("[apm]");
		expect(text({ mode: "side-run", errors: ["boom"] })).toContain("boom");
		expect(text("not an object")).toContain("[apm]");
	});

	it("never mounts a token-shaped secret", () => {
		const token = `apm_${"ab12cd34".repeat(8)}`;
		const rendered = text(
			report({
				output: `wrote ${token}`,
				errors: [`failed with ${token}`],
			}),
			true,
		);
		expect(rendered).not.toContain(token);
		expect(rendered).toContain("apm_[redacted]");
	});
});
