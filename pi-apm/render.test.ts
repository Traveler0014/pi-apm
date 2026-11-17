import { describe, expect, it } from "vitest";
import type { RunSyncReportData } from "./lib.ts";
import { renderRunSyncEntry } from "./render.ts";

// Minimal theme: identity colours, so rendered lines stay identifiable in tests.
const theme = {
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

function render(data: unknown, expanded = false): string {
	return renderRunSyncEntry({ data }, { expanded }, theme)
		.render(400)
		.join("\n");
}

describe("apm-run-sync entry renderer", () => {
	it("renders a successful sync compactly", () => {
		const text = render(report());
		expect(text).toContain("[apm] run synced");
		expect(text).toContain("1.2s");
		expect(text).toContain("APM-7 sync");
		expect(text).not.toContain("session 01a1"); // collapsed hides metadata
	});

	it("renders skip and abort modes with their reasons", () => {
		const skipped = render(report({ mode: "skip", ran: false, reason: "recursion guard", reconciled: [] }));
		expect(skipped).toContain("[apm] skipped");
		expect(skipped).toContain("recursion guard");
		expect(skipped).not.toContain("items:");

		const aborted = render(report({ mode: "abort-sync", reason: "run aborted by the user" }));
		expect(aborted).toContain("aborted run · liveness only");
		expect(aborted).toContain("run aborted by the user");
	});

	it("renders failures, reconciliation results and the error count", () => {
		const text = render(
			report({
				errors: ["side-run timed out after 180000ms"],
				reconciled: [
					{ identifier: "APM-7", action: "sync", ok: true, code: null },
					{ identifier: "APM-8", action: "diagnostic", ok: false, code: "claim_required" },
				],
			}),
		);
		expect(text).toContain("issue(s)");
		expect(text).toContain("✓ APM-7 sync");
		expect(text).toContain("✗ APM-8");
		expect(text).toContain("claim_required");
		expect(text).toContain("side-run timed out");
	});

	it("truncates long output when collapsed and shows more when expanded", () => {
		const long = Array.from({ length: 8 }, (_, i) => `line ${i} ${"x".repeat(400)}`).join("\n");
		const collapsed = render(report({ output: long }));
		expect(collapsed).toContain("line 0");
		expect(collapsed).not.toContain("line 1");
		const expanded = render(report({ output: long }), true);
		expect(expanded).toContain("line 5");
		expect(expanded).toContain("session 01a1e223");
		for (const line of expanded.split("\n")) {
			// Box pads to the render width; measure the visible content.
			expect(line.trimEnd().length).toBeLessThanOrEqual(210);
		}
	});

	it("never leaks claim tokens from output or errors", () => {
		const token = `apm_${"ab12cd34".repeat(8)}`;
		const text = render(
			report({
				output: `wrote token ${token}`,
				errors: [`failed with ${token}`],
			}),
			true,
		);
		expect(text).not.toContain(token);
		expect(text).toContain("apm_[redacted]");
	});

	it("never throws on malformed report data", () => {
		expect(render(undefined)).toContain("[apm]");
		expect(render({ mode: "side-run" })).toContain("[apm]");
	});
});
