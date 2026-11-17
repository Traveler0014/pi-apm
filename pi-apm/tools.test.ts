/**
 * Tool-surface contract tests for the pi extension.
 *
 * The extension registers its tools through the `pi` API, so a tiny mock is
 * enough to inspect the schemas the model actually receives. These tests guard
 * the *interface* (what a model is allowed to ask for), while `lib.test.ts`
 * guards the pure logic and `side-run.e2e.test.ts` guards the child process.
 */

import { describe, expect, it } from "vitest";
import extension from "./index.ts";

type ToolSchema = {
	name: string;
	description?: string;
	parameters: {
		type?: string;
		properties?: Record<string, Record<string, unknown>>;
		required?: string[];
	};
	execute: (...args: unknown[]) => Promise<unknown>;
};

function registerTools(): Map<string, ToolSchema> {
	const tools = new Map<string, ToolSchema>();
	const pi = {
		registerTool: (tool: ToolSchema) => {
			tools.set(tool.name, tool);
		},
		registerCommand: () => {},
		registerFlag: () => {},
		getFlag: () => undefined,
		on: () => {},
		sendMessage: () => {},
		appendEntry: () => {},
	};
	(extension as unknown as (api: unknown) => void)(pi);
	return tools;
}

describe("registered tool surface", () => {
	const tools = registerTools();

	it("registers the documented tools", () => {
		expect([...tools.keys()].sort()).toEqual(
			[
				"apm_attention",
				"apm_claim",
				"apm_context",
				"apm_dashboard",
				"apm_progress",
				"apm_release",
				"apm_status",
				"apm_sync",
			].sort(),
		);
	});

	it("keeps every tool's parameters an object schema", () => {
		for (const tool of tools.values()) {
			expect(tool.parameters.type, tool.name).toBe("object");
			expect(tool.description?.length ?? 0, tool.name).toBeGreaterThan(20);
		}
	});

	it("does not pin apm_status targets to the built-in six states", () => {
		// Custom workflows (APM-7) can declare extra states such as `qa`. An enum
		// here would make the model unable to ask for them, so the key stays a
		// free-form string and the ledger validates it.
		const status = tools.get("apm_status");
		expect(status).toBeDefined();
		const to = status?.parameters.properties?.to as Record<string, unknown>;
		expect(to.type).toBe("string");
		expect(to.enum).toBeUndefined();
		expect(String(to.description ?? "")).toMatch(/workflow/i);
		expect(String(status?.description ?? "")).toMatch(/workflow/i);
	});

	it("still enumerates genuinely closed vocabularies", () => {
		// The distinction matters: `to` is workflow-defined, these are not.
		const context = tools.get("apm_context");
		const action = context?.parameters.properties?.action as Record<string, unknown>;
		expect(action.enum).toEqual(["show", "revise"]);

		const sync = tools.get("apm_sync");
		const outcome = sync?.parameters.properties?.outcome as Record<string, unknown>;
		expect(outcome.enum).toEqual(["completed", "aborted", "unknown"]);
	});

	it("requires the mandatory arguments", () => {
		expect(tools.get("apm_claim")?.parameters.required).toEqual(["item"]);
		expect(tools.get("apm_progress")?.parameters.required).toEqual(["item", "summary"]);
		expect(tools.get("apm_status")?.parameters.required).toEqual(["item", "to"]);
		expect(tools.get("apm_context")?.parameters.required).toEqual(["item", "action"]);
	});
});
