import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterAll, describe, expect, it } from "vitest";
import {
	type ClaimSnapshot,
	buildSideRunSystemPrompt,
	buildSideRunTaskPrompt,
	decideRunMode,
	digestRun,
	findProjectDir,
	parseClaimListing,
	planAbortSync,
	planReconciliation,
	redactSecrets,
	formatRunSyncReport,
	coveredItemIds,
	preflightSkip,
	runKey,
	summarizeSideRunOutput,
	truncate,
} from "./lib.ts";

const tmpRoots: string[] = [];

function tmpdir(): string {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-apm-test-"));
	tmpRoots.push(dir);
	return dir;
}

afterAll(() => {
	for (const dir of tmpRoots) fs.rmSync(dir, { recursive: true, force: true });
});

function claim(overrides: Partial<ClaimSnapshot> = {}): ClaimSnapshot {
	return {
		itemId: "item-1",
		identifier: "APM-1",
		title: "Do the thing",
		itemStatus: "doing",
		version: 3,
		contextVersion: 2,
		progressSummary: "half way",
		nextAction: "finish",
		lastProgressAt: "2026-01-01T00:00:00.000Z",
		claimId: "claim-1",
		claimStatus: "active",
		claimExpired: false,
		expiresAt: "2026-01-01T00:15:00.000Z",
		holder: "agent-a",
		sessionId: "sess-1",
		...overrides,
	};
}

describe("runKey", () => {
	it("is unique per item and per operation", () => {
		const base = { sessionId: "s1", runId: "r1", itemId: "i1" };
		const keys = new Set([
			runKey({ ...base, op: "sync" }),
			runKey({ ...base, op: "progress" }),
			runKey({ ...base, itemId: "i2", op: "sync" }),
		]);
		expect(keys.size).toBe(3);
	});

	it("sanitises unsafe characters so the key is a single CLI argument", () => {
		const key = runKey({ sessionId: "a b/c", runId: "r 1", itemId: "i:1", op: "sync" });
		expect(key).not.toContain(" ");
		expect(key).toMatch(/^sr:[A-Za-z0-9._-]+:[A-Za-z0-9._-]+:[A-Za-z0-9._-]+:sync$/);
	});
});

describe("parseClaimListing", () => {
	it("reads claims without exposing tokens", () => {
		const listing = {
			ok: true,
			session: "sess-1",
			claims: [
				{
					item: {
						id: "item-1",
						identifier: "APM-1",
						title: "T",
						status: "doing",
						version: 4,
						context_version: 2,
						progress_summary: "p",
						next_action: "n",
						last_progress_at: "2026-01-01T00:00:00.000Z",
					},
					claim: {
						id: "c1",
						status: "active",
						expired: false,
						expires_at: "2026-01-01T00:15:00.000Z",
						holder: "agent-a",
						session_id: "sess-1",
					},
				},
			],
			tokens: { "item-1": "apm_secret" },
		};
		const parsed = parseClaimListing(listing);
		expect(parsed).toHaveLength(1);
		expect(parsed[0]).toMatchObject({
			itemId: "item-1",
			identifier: "APM-1",
			version: 4,
			contextVersion: 2,
			claimExpired: false,
		});
		// Tokens are resolved by the CLI from the 0600 session file; they must
		// never be part of the snapshot (and therefore never enter a prompt).
		expect(parsed[0]).not.toHaveProperty("token");
	});

	it("degrades to an empty list on malformed input", () => {
		expect(parseClaimListing(null)).toEqual([]);
		expect(parseClaimListing({ claims: "nope" })).toEqual([]);
		expect(parseClaimListing({ claims: [{}, {}] })).toEqual([]);
	});
});

describe("digestRun", () => {
	const message = (value: Record<string, unknown>) => value;

	it("collects prompts, assistant text, tool calls, files and errors", () => {
		const digest = digestRun([
			message({ role: "user", content: "please fix the parser" }),
			message({
				role: "assistant",
				content: [
					{ type: "text", text: "I will edit src/parse.rs" },
					{ type: "toolCall", name: "edit", arguments: { file_path: "src/parse.rs" } },
					{ type: "toolCall", name: "bash", arguments: { command: "cargo test --all" } },
				],
			}),
			message({
				role: "toolResult",
				toolName: "bash",
				isError: true,
				content: [{ type: "text", text: "test failed: 3 errors" }],
			}),
		]);
		expect(digest.userPrompts).toEqual(["please fix the parser"]);
		expect(digest.assistantTexts[0]).toContain("src/parse.rs");
		expect(digest.toolCalls.map((t) => t.name)).toEqual(["edit", "bash"]);
		expect(digest.filesTouched).toContain("src/parse.rs");
		expect(digest.errors[0]).toContain("bash: test failed");
	});

	it("is bounded and merges with a previous digest", () => {
		const many = Array.from({ length: 30 }, (_, i) => message({ role: "user", content: `prompt ${i}` }));
		const first = digestRun(many);
		expect(first.userPrompts).toHaveLength(6);
		expect(first.userPrompts.at(-1)).toBe("prompt 29");

		const merged = digestRun([message({ role: "user", content: "later" })], first);
		expect(merged.userPrompts.at(-1)).toBe("later");
		expect(merged.userPrompts).toHaveLength(6);
	});

	it("survives unknown roles and string content", () => {
		const digest = digestRun([{ role: "mystery" }, { role: "user", content: "hi" }, null, 42]);
		expect(digest.userPrompts).toEqual(["hi"]);
	});
});

describe("decideRunMode", () => {
	const base = {
		disabled: false,
		env: {} as Record<string, string | undefined>,
		aborted: false,
		projectDir: "/repo",
		claims: [claim()],
		hasSession: true,
	};

	it("runs a side-run after a normal completed run", () => {
		expect(decideRunMode(base).mode).toBe("side-run");
	});

	it("uses the deterministic abort path for aborted runs", () => {
		const decision = decideRunMode({ ...base, aborted: true });
		expect(decision.mode).toBe("abort-sync");
	});

	it("never recurses", () => {
		const decision = decideRunMode({ ...base, env: { APM_SIDE_RUN: "1" } });
		expect(decision.mode).toBe("skip");
		expect(decision.reason).toContain("recursion");
	});

	it("respects disable flags", () => {
		expect(decideRunMode({ ...base, disabled: true }).mode).toBe("skip");
		expect(decideRunMode({ ...base, env: { APM_SIDE_RUN_DISABLED: "1" } }).mode).toBe("skip");
	});

	it("preflight carries no claim dependency", () => {
		expect(
			preflightSkip({ disabled: false, env: {}, projectDir: "/repo", hasSession: true }),
		).toBeNull();
		expect(
			preflightSkip({ disabled: true, env: {}, projectDir: "/repo", hasSession: true }),
		).toBe("extension disabled");
		expect(
			preflightSkip({
				disabled: false,
				env: { APM_SIDE_RUN: "1" },
				projectDir: "/repo",
				hasSession: true,
			}),
		).toContain("recursion");
	});

	it("skips when there is nothing it is allowed to touch", () => {
		expect(decideRunMode({ ...base, claims: [] }).mode).toBe("skip");
		expect(decideRunMode({ ...base, projectDir: null }).mode).toBe("skip");
		expect(decideRunMode({ ...base, hasSession: false }).mode).toBe("skip");
	});
});

describe("planAbortSync", () => {
	it("writes one liveness sync per live claim and never touches progress or status", () => {
		const plan = planAbortSync({
			sessionId: "sess-1",
			runId: "run-1",
			claims: [claim(), claim({ itemId: "item-2", identifier: "APM-2" })],
			reason: "user pressed Escape",
		});
		expect(plan).toHaveLength(2);
		expect(plan.every((p) => p.kind === "sync-aborted")).toBe(true);
		expect(new Set(plan.map((p) => p.key)).size).toBe(2);
		for (const call of plan) {
			expect(call.args).toContain("aborted");
			expect(call.args.join(" ")).not.toContain("progress");
			expect(call.args).not.toContain("--to");
			expect(call.args).not.toContain("release");
		}
	});

	it("degrades to a diagnostic when the lease is already gone", () => {
		const plan = planAbortSync({
			sessionId: "sess-1",
			runId: "run-1",
			claims: [claim({ claimExpired: true })],
			reason: "aborted",
		});
		expect(plan).toHaveLength(1);
		expect(plan[0].kind).toBe("diagnostic");
		expect(plan[0].args).toContain("diag");
		expect(plan[0].args).toContain("claim_expired_after_abort");
	});
});

describe("planReconciliation", () => {
	const items = [
		{ itemId: "i1", identifier: "A-1", claimActive: true },
		{ itemId: "i2", identifier: "A-2", claimActive: true },
	];

	it("baseline-syncs every uncovered item with stable per-item keys", () => {
		const plan = planReconciliation({
			items,
			covered: new Set<string>(),
			runId: "run-1",
			sessionId: "sess-1",
			aborted: false,
			childFailed: false,
		});
		expect(plan.map((s) => s.action)).toEqual(["sync", "sync"]);
		expect(new Set(plan.map((s) => s.key)).size).toBe(2);
		for (const step of plan) {
			expect(step.args).toContain("sync");
			expect(step.args).toContain("completed");
			expect(step.args.join(" ")).not.toContain("progress");
			expect(step.args.join(" ")).not.toContain("--to");
		}
	});

	it("skips items that already have an event from this run", () => {
		const plan = planReconciliation({
			items,
			covered: new Set(["i1"]),
			runId: "run-1",
			sessionId: "sess-1",
			aborted: false,
			childFailed: false,
		});
		expect(plan.find((s) => s.itemId === "i1")?.action).toBe("skip");
		expect(plan.find((s) => s.itemId === "i2")?.action).toBe("sync");
	});

	it("diagnoses a dead lease instead of writing a sync", () => {
		const plan = planReconciliation({
			items: [{ itemId: "i1", identifier: "A-1", claimActive: false }],
			covered: new Set<string>(),
			runId: "run-1",
			sessionId: "sess-1",
			aborted: false,
			childFailed: true,
		});
		expect(plan[0].action).toBe("diagnostic");
		expect(plan[0].args).toContain("diag");
		expect(plan[0].args).toContain("claim_expired_after_side_run");
	});

	it("is deterministic across retries", () => {
		const args = {
			items,
			covered: new Set<string>(),
			runId: "run-1",
			sessionId: "sess-1",
			aborted: false,
			childFailed: false,
		};
		expect(planReconciliation(args)).toEqual(planReconciliation(args));
	});

	it("counts only this run's progress/run events as coverage", () => {
		const events = [
			{ kind: "progress_recorded", run_id: "run-1", item_id: "i1" },
			{ kind: "run_synced", run_id: "other-run", item_id: "i2" },
			{ kind: "context_revised", run_id: "run-1", item_id: "i2" },
			{ kind: "claimed", run_id: "run-1", item_id: "i2" },
		];
		const covered = coveredItemIds(events, "run-1");
		expect([...covered]).toEqual(["i1"]);
	});
});

describe("formatRunSyncReport", () => {
	const base = {
		ran: true,
		mode: "side-run" as const,
		reason: "run completed",
		runId: "run-1",
		items: ["APM-7"],
		reconciled: [{ identifier: "APM-7", action: "sync", ok: true }],
		errors: [] as string[],
		durationMs: 900,
	};

	it("labels success, skip and abort distinctly", () => {
		expect(formatRunSyncReport(base)[0]).toMatchObject({ tone: "success" });
		expect(formatRunSyncReport(base)[0].text).toContain("run synced");

		const skipped = formatRunSyncReport({ ...base, mode: "skip", ran: false, reason: "no claims" });
		expect(skipped[0].text).toContain("skipped");
		expect(skipped[0].tone).toBe("muted");
		expect(skipped.some((l) => l.text.includes("no claims"))).toBe(true);

		const aborted = formatRunSyncReport({ ...base, mode: "abort-sync" });
		expect(aborted[0].text).toContain("aborted run");
		expect(aborted[0].tone).toBe("warning");
	});

	it("shows reconciliation outcomes and failure codes", () => {
		const lines = formatRunSyncReport({
			...base,
			errors: ["side-run timed out"],
			reconciled: [
				{ identifier: "APM-7", action: "sync", ok: true },
				{ identifier: "APM-8", action: "skip", ok: true },
				{ identifier: "APM-9", action: "sync", ok: false, code: "claim_required" },
			],
		});
		const text = lines.map((l) => l.text).join("\n");
		expect(text).toContain("✓ APM-7 sync");
		expect(text).toContain("· APM-8 already recorded");
		expect(text).toContain("✗ APM-9 failed (claim_required)");
		expect(lines.find((l) => l.text.includes("APM-9"))?.tone).toBe("error");
	});

	it("truncates output and redacts tokens without a terminal", () => {
		const token = `apm_${"1234abcd".repeat(8)}`;
		const output = Array.from({ length: 10 }, (_, i) => `line ${i} ${"y".repeat(300)}`).join("\n");
		const collapsed = formatRunSyncReport({ ...base, output: `${token}\n${output}` });
		const collapsedText = collapsed.map((l) => l.text).join("\n");
		expect(collapsedText).not.toContain(token);
		expect(collapsedText).toContain("apm_[redacted]");
		expect(collapsed.some((l) => l.text.includes("line 1"))).toBe(false);

		const expanded = formatRunSyncReport({ ...base, output }, { expanded: true });
		expect(expanded.some((l) => l.text.includes("line 5"))).toBe(true);
		for (const line of expanded) {
			expect(line.text.length).toBeLessThanOrEqual(210);
		}
	});

	it("degrades to a single muted line for missing data", () => {
		const lines = formatRunSyncReport({});
		expect(lines).toHaveLength(1);
		expect(lines[0].text).toBe("[apm] no run-sync data");
	});
});

describe("redaction", () => {
	const token = `apm_${"a1b2c3d4".repeat(8)}`;

	it("removes claim tokens but keeps tool names and normal text", () => {
		const text = `ran apm claim --json -> {"token":"${token}"} (see apm_claim tool)`;
		const redacted = redactSecrets(text);
		expect(redacted).not.toMatch(/apm_[0-9a-f]{32,}/);
		expect(redacted).toContain("apm_[redacted]");
		expect(redacted).toContain("apm_claim");
	});

	it("scrubs a digest that captured command output", () => {
		const digest = digestRun([
			{ role: "assistant", content: [{ type: "text", text: `output: ${token}` }] },
			{
				role: "toolResult",
				toolName: "bash",
				isError: true,
				content: [{ type: "text", text: `command failed ${token}` }],
			},
		]);
		expect(JSON.stringify(digest)).not.toMatch(/apm_[0-9a-f]{32,}/);
	});

	it("scrubs the child's reported output", () => {
		const out = summarizeSideRunOutput(
			JSON.stringify({
				type: "message_end",
				message: { role: "assistant", content: [{ type: "text", text: `wrote ${token}` }] },
			}),
		);
		expect(out.text).not.toMatch(/apm_[0-9a-f]{32,}/);
	});
});

describe("prompts", () => {
	const claims = [
		claim(),
		claim({ itemId: "item-2", identifier: "APM-2", version: 9 }),
	];
	const task = {
		sessionId: "sess-1",
		runId: "run-1",
		projectDir: "/repo",
		dbPath: "/repo/.apm/apm.db",
		claims,
		digest: digestRun([
			{ role: "user", content: "fix the parser" },
			{ role: "assistant", content: [{ type: "text", text: "patched parser" }] },
		]),
		startedAt: "2026-01-01T00:00:00.000Z",
		finishedAt: "2026-01-01T00:10:00.000Z",
	};

	it("system prompt states the hard rules", () => {
		const prompt = buildSideRunSystemPrompt();
		expect(prompt).toContain("Never claim an item");
		expect(prompt).toContain("Never create items");
		expect(prompt).toContain("--recover-only");
		expect(prompt).toContain("0600 session file");
		expect(prompt).not.toMatch(/apm_[0-9a-f]{32,}/);
		expect(prompt).toContain("Never invent percentages");
		expect(prompt).toContain("Never move an item to done unless the run contains verification evidence");
	});

	it("task prompt carries tokens, versions and per-item idempotency keys", () => {
		const prompt = buildSideRunTaskPrompt(task);
		// No plaintext token may ever appear in the prompt that becomes the child's
		// argv (and the model request).
		expect(prompt).not.toMatch(/apm_[0-9a-f]{32,}/);
		expect(prompt).not.toMatch(/^token: /m);
		expect(prompt).toContain("--session");
		expect(prompt).toContain("item version: 3");
		expect(prompt).toContain("item version: 9");
		expect(prompt).toContain(runKey({ sessionId: "sess-1", runId: "run-1", itemId: "item-1", op: "sync" }));
		expect(prompt).toContain(runKey({ sessionId: "sess-1", runId: "run-1", itemId: "item-2", op: "context" }));
		expect(prompt).toContain("patched parser");
	});

	it("truncation keeps prompts small", () => {
		const long = "x".repeat(5000);
		expect(truncate(long, 100)).toHaveLength(100);
		expect(truncate("short", 100)).toBe("short");
	});
});

describe("summarizeSideRunOutput", () => {
	it("keeps the last assistant text and collects errors", () => {
		const stdout = [
			JSON.stringify({ type: "agent_start" }),
			JSON.stringify({
				type: "message_end",
				message: { role: "assistant", content: [{ type: "text", text: "first" }] },
			}),
			JSON.stringify({
				type: "message_end",
				message: { role: "assistant", content: [{ type: "text", text: "final summary" }] },
			}),
			JSON.stringify({ type: "error", error: "boom" }),
			"not json",
		].join("\n");
		const summary = summarizeSideRunOutput(stdout);
		expect(summary.text).toBe("final summary");
		expect(summary.errors).toHaveLength(1);
		expect(summary.events).toBe(4);
	});

	it("does not throw on garbage", () => {
		const summary = summarizeSideRunOutput("@@@@\n\n");
		expect(summary.text).toBe("");
		expect(summary.errors).toEqual([]);
	});
});

describe("findProjectDir", () => {
	it("finds the project from a nested directory and returns null elsewhere", () => {
		const root = tmpdir();
		fs.mkdirSync(path.join(root, ".apm"), { recursive: true });
		fs.writeFileSync(path.join(root, ".apm", "apm.db"), "");
		const nested = path.join(root, "src", "deep", "deeper");
		fs.mkdirSync(nested, { recursive: true });
		expect(findProjectDir(nested)).toBe(root);
		expect(findProjectDir(tmpdir())).toBeNull();
	});
});
