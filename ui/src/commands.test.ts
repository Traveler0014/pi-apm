import { describe, expect, it } from "vitest";
import { cliCommands, shellQuote } from "./commands";
import { mockDetail } from "./fixtures";

describe("cliCommands", () => {
	const item = mockDetail("i-15").item;
	const specs = cliCommands(item);
	const byLabel = new Map(specs.map((spec) => [spec.label, spec.command]));

	it("offers get, claim, progress, status, context and events", () => {
		expect([...byLabel.keys()]).toEqual([
			"Read the item",
			"Claim it",
			"Record progress",
			"Change status",
			"Read the handover",
			"Recent events",
		]);
	});

	it("substitutes the identifier and never leaks a token or session", () => {
		for (const spec of specs) {
			expect(spec.command).toContain(item.identifier);
			expect(spec.command).not.toMatch(/apm_[0-9a-f]{16,}/);
			expect(spec.command).not.toMatch(/--token\b/);
			expect(spec.command).not.toMatch(/--session\b/);
			expect(spec.hint.length).toBeGreaterThan(10);
		}
		expect(byLabel.get("Read the item")).toBe("apm get APM-15 --json");
		expect(byLabel.get("Claim it")).toBe("apm claim APM-15");
		expect(byLabel.get("Change status")).toBe("apm status APM-15 --to doing");
	});

	it("quotes identifiers that need it", () => {
		expect(cliCommands({ identifier: "APM-15" })[0]!.command).toContain(" APM-15 ");
		expect(cliCommands({ identifier: "weird id" })[0]!.command).toContain("'weird id'");
		expect(shellQuote("it's")).toBe(`'it'\\''s'`);
	});
});
