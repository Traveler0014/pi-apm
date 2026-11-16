/**
 * End-to-end hook test: drives the real extension handlers against a real APM
 * database, with a fake `pi` child (`APM_SIDE_RUN_PI`) that performs the same
 * `apm` calls a real side-run would. No model is called.
 *
 * This is what proves the wiring the unit tests cannot: the hook reads the
 * session's claims, passes tokens + per-item idempotency keys to the child, the
 * child writes, retries are idempotent, abort takes the deterministic path, and
 * failures never throw out of the hook.
 */

import { execFileSync, spawnSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import extension from "./index.ts";

const repoRoot = path.resolve(__dirname, "..");
let apmBin = "";
const tmpRoots: string[] = [];

function tmpdir(): string {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-apm-e2e-"));
	tmpRoots.push(dir);
	return dir;
}

function resolveApm(): string {
	const candidates = [
		process.env.APM_BIN,
		path.join(repoRoot, "target", "debug", "apm"),
		path.join(repoRoot, "target", "release", "apm"),
	].filter((p): p is string => Boolean(p));
	for (const candidate of candidates) {
		if (fs.existsSync(candidate)) return candidate;
	}
	execFileSync("cargo", ["build"], { cwd: repoRoot, stdio: "ignore" });
	return path.join(repoRoot, "target", "debug", "apm");
}

beforeAll(() => {
	apmBin = resolveApm();
});

afterAll(() => {
	for (const dir of tmpRoots) fs.rmSync(dir, { recursive: true, force: true });
});

interface FakeApi {
	handlers: Map<string, Array<(event: unknown, ctx: unknown) => Promise<unknown> | unknown>>;
	entries: Array<{ customType: string; data: unknown }>;
	tools: Map<string, unknown>;
	flags: Record<string, unknown>;
	on(event: string, handler: (event: unknown, ctx: unknown) => Promise<unknown> | unknown): void;
	registerTool(tool: { name: string }): void;
	registerCommand(name: string): void;
	registerFlag(name: string, options: { default?: unknown }): void;
	getFlag(name: string): unknown;
	appendEntry(customType: string, data: unknown): void;
	sendMessage(): void;
	exec(): Promise<unknown>;
}

function fakeApi(flags: Record<string, unknown>): FakeApi {
	const handlers = new Map<string, Array<(event: unknown, ctx: unknown) => Promise<unknown> | unknown>>();
	const api: FakeApi = {
		handlers,
		entries: [],
		tools: new Map(),
		flags,
		on(event, handler) {
			const list = handlers.get(event) ?? [];
			list.push(handler);
			handlers.set(event, list);
		},
		registerTool(tool) {
			api.tools.set(tool.name, tool);
		},
		registerCommand() {},
		registerFlag(name, options) {
			if (!(name in api.flags)) api.flags[name] = options.default;
		},
		getFlag(name) {
			return api.flags[name];
		},
		appendEntry(customType, data) {
			api.entries.push({ customType, data });
		},
		sendMessage() {},
		async exec() {
			return { code: 0, stdout: "", stderr: "" };
		},
	};
	return api;
}

async function fire(api: FakeApi, event: string, payload: unknown, ctx: unknown) {
	for (const handler of api.handlers.get(event) ?? []) {
		await handler(payload, ctx);
	}
}

function fakeCtx(cwd: string, sessionId: string) {
	const notes: string[] = [];
	return {
		notes,
		cwd,
		hasUI: false,
		sessionManager: { getSessionId: () => sessionId },
		ui: {
			notify: (message: string) => notes.push(message),
		},
	};
}

function apmJson(args: string[], cwd: string, env: Record<string, string> = {}): unknown {
	const out = spawnSync(apmBin, [...args, "--json"], {
		cwd,
		encoding: "utf8",
		env: { ...process.env, APM_BIN: apmBin, ...env },
	});
	if (out.status !== 0) {
		throw new Error(`apm ${args.join(" ")} failed: ${out.stdout} ${out.stderr}`);
	}
	return JSON.parse(out.stdout);
}

function writeFakePi(): string {
	const dir = tmpdir();
	const file = path.join(dir, "fake-pi.cjs");
	fs.writeFileSync(
		file,
		`#!/usr/bin/env node
const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const args = process.argv.slice(2);
const prompt = args[args.length - 1] || "";
// The model calls apm through bash, so the extension must make the CLI
// resolvable from PATH even when it was configured with --apm-bin.
let apmOnPath = false;
let apmVersion = "";
try {
  apmVersion = execFileSync("apm", ["--version"], { encoding: "utf8" }).trim();
  apmOnPath = true;
} catch (error) {
  apmVersion = "missing: " + String(error && error.message);
}
fs.writeFileSync(
  process.env.FAKE_PI_LOG,
  JSON.stringify({
    args,
    prompt,
    env: {
      APM_SIDE_RUN: process.env.APM_SIDE_RUN,
      APM_SESSION_ID: process.env.APM_SESSION_ID,
      APM_RUN_ID: process.env.APM_RUN_ID,
      APM_DB: process.env.APM_DB,
      APM_BIN: process.env.APM_BIN,
      PATH: process.env.PATH,
    },
    apmOnPath,
    apmVersion,
  }, null, 2),
);
const mode = process.env.FAKE_PI_MODE || "normal";
const runRaw = (a) => execFileSync(process.env.APM_BIN, [...a, "--session", process.env.APM_SESSION_ID, "--run", process.env.APM_RUN_ID, "--json"], { encoding: "utf8", env: process.env });
if (mode === "do-nothing") {
  process.stdout.write(JSON.stringify({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "nothing to report" }] } }) + "\\n");
  process.exit(0);
}
// Scan the task prompt: section headers, per-item idempotency keys, versions.
const items = [];
let current = null;
for (const line of prompt.split("\\n")) {
  const header = line.match(/^### (\\S+) \\(/);
  if (header) { current = { id: header[1], keys: {}, version: 1 }; items.push(current); continue; }
  if (!current) continue;
  const key = line.match(/^  (sync|progress|context|status|evidence|attention|diag): (sr:\\S+)$/);
  if (key) { current.keys[key[1]] = key[2]; continue; }
  const version = line.match(/item version: (\\d+)/);
  if (version) current.version = Number(version[1]);
}
if (items.length === 0) { process.stdout.write('{"type":"error","error":"missing item in prompt"}\\n'); process.exit(1); }
if (mode === "recover") {
  // Exercise the recovery path exactly like a real side-run: the claim output is
  // captured (and checked by the test) but never fed back to the model.
  const out = runRaw(["claim", items[0].id, "--recover-only", "--takeover", "--reason", "run-end recovery after lease expiry"]);
  fs.appendFileSync(process.env.FAKE_PI_LOG, "\\n--RECOVER-OUTPUT--\\n" + out);
  runRaw(["sync", items[0].id, "--outcome", "completed", "--idempotency-key", items[0].keys.sync]);
  process.stdout.write(JSON.stringify({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "recovered " + items[0].id + " and synced" }] } }) + "\\n");
  process.exit(0);
}
const targets = mode === "first-only" ? items.slice(0, 1) : items;
// No token here by design: the CLI reads it from the 0600 session file.
const run = (a) => { runRaw(a); };
for (const item of targets) {
  run(["sync", item.id, "--outcome", "completed", "--idempotency-key", item.keys.sync]);
  run(["progress", item.id, "--expect-version", String(item.version), "--summary", "patched the parser and ran the suite", "--next-action", "review the diff", "--idempotency-key", item.keys.progress]);
  // Retry the same run: the ledger must not duplicate events.
  run(["sync", item.id, "--outcome", "completed", "--idempotency-key", item.keys.sync]);
}
process.stdout.write(JSON.stringify({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "synced " + targets.length + " item(s)" }] } }) + "\\n");
`,
		{ mode: 0o755 },
	);
	return file;
}

function project(): { dir: string; identifier: string; sessionId: string } {
	const dir = tmpdir();
	execFileSync(apmBin, ["init", "--name", "E2E", "--prefix", "E2E", "--json"], {
		cwd: dir,
		env: { ...process.env, APM_DB: path.join(dir, ".apm", "apm.db") },
	});
	const sessionId = "sess-e2e";
	const env = { APM_DB: path.join(dir, ".apm", "apm.db"), APM_AGENT: "agent-a", APM_SESSION_ID: sessionId };
	const created = apmJson(
		[
			"create",
			"--title",
			"Fix the parser",
			"--goal",
			"parser handles empty input",
			"--current-state",
			"parser crashes on empty input",
			"--acceptance",
			"empty input returns ok",
		],
		dir,
		env,
	) as { item: { identifier: string } };
	apmJson(["claim", created.item.identifier], dir, env);
	apmJson(["status", created.item.identifier, "--to", "doing", "--expect-version", "1"], dir, env);
	apmJson(
		["progress", created.item.identifier, "--expect-version", "2", "--summary", "reproduced the crash"],
		dir,
		env,
	);
	return { dir, identifier: created.item.identifier, sessionId };
}

function projectExpired(): { dir: string; identifier: string; sessionId: string } {
	const dir = tmpdir();
	execFileSync(apmBin, ["init", "--name", "Expired", "--prefix", "EXP", "--json"], {
		cwd: dir,
		env: { ...process.env, APM_DB: path.join(dir, ".apm", "apm.db") },
	});
	const sessionId = "sess-expired";
	const env = { APM_DB: path.join(dir, ".apm", "apm.db"), APM_AGENT: "agent-a", APM_SESSION_ID: sessionId };
	const created = apmJson(
		["create", "--title", "Expired lease", "--goal", "recover it", "--current-state", "claimed long ago"],
		dir,
		env,
	) as { item: { identifier: string } };
	// A lease that is already expired, so only `--recover-only --takeover` works.
	apmJson(
		[
			"claim",
			created.item.identifier,
			"--lease",
			"30",
			"--now",
			String(Date.now() - 120_000),
		],
		dir,
		env,
	);
	return { dir, identifier: created.item.identifier, sessionId };
}

function projectTwo(): { dir: string; a: string; b: string; sessionId: string } {
	const dir = tmpdir();
	execFileSync(apmBin, ["init", "--name", "E2E2", "--prefix", "E2E", "--json"], {
		cwd: dir,
		env: { ...process.env, APM_DB: path.join(dir, ".apm", "apm.db") },
	});
	const sessionId = "sess-e2e2";
	const env = { APM_DB: path.join(dir, ".apm", "apm.db"), APM_AGENT: "agent-a", APM_SESSION_ID: sessionId };
	const mk = (title: string) =>
		(
			apmJson(
				[
					"create",
					"--title",
					title,
					"--goal",
					"goal",
					"--current-state",
					"state",
				],
				dir,
				env,
			) as { item: { identifier: string; version: number } }
		).item;
	const a = mk("First item");
	const b = mk("Second item");
	apmJson(["claim", a.identifier], dir, env);
	apmJson(["claim", b.identifier], dir, env);
	return { dir, a: a.identifier, b: b.identifier, sessionId };
}

function bareProject(): { dir: string; identifier: string; sessionId: string } {
	const dir = tmpdir();
	execFileSync(apmBin, ["init", "--name", "Tools", "--prefix", "TOOL", "--json"], {
		cwd: dir,
		env: { ...process.env, APM_DB: path.join(dir, ".apm", "apm.db") },
	});
	const sessionId = "sess-tools";
	const env = { APM_DB: path.join(dir, ".apm", "apm.db"), APM_AGENT: "agent-a", APM_SESSION_ID: sessionId };
	const created = apmJson(
		[
			"create",
			"--title",
			"Tool surface",
			"--goal",
			"tools drive the ledger",
			"--current-state",
			"nothing claimed yet",
		],
		dir,
		env,
	) as { item: { identifier: string } };
	return { dir, identifier: created.item.identifier, sessionId };
}

type Tool = {
	execute: (
		id: string,
		params: Record<string, unknown>,
		signal: unknown,
		onUpdate: unknown,
		ctx: unknown,
	) => Promise<{ content: Array<{ type: string; text: string }>; details: unknown; isError?: boolean }>;
};

function tool(api: FakeApi, name: string): Tool {
	const found = api.tools.get(name) as Tool | undefined;
	if (!found) throw new Error(`tool ${name} not registered`);
	return found;
}

describe("agent tools", () => {
	it("claims, records progress, asks a human and reads the board", async () => {
		const p = bareProject();
		const api = fakeApi({ "apm-agent": "agent-a", "apm-side-run-timeout": "60" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);
		const env = { APM_DB: path.join(p.dir, ".apm", "apm.db") };
		const previousBin = process.env.APM_BIN;
		process.env.APM_BIN = apmBin;
		try {
			// claim
			const claimed = await tool(api, "apm_claim").execute("t1", { item: p.identifier }, null, null, ctx);
			expect(claimed.content[0].text).toContain(`claimed ${p.identifier}`);
			expect(JSON.stringify(claimed.details)).not.toContain("apm_");

			// progress (auto-resolves the version, reuses the session token)
			const progressed = await tool(api, "apm_progress").execute(
				"t2",
				{ item: p.identifier, summary: "wired the tool surface", next_action: "write tests" },
				null,
				null,
				ctx,
			);
			expect(progressed.content[0].text).toContain("progress recorded");

			// repeating the identical progress is reported as a no-op, not success
			const repeated = await tool(api, "apm_progress").execute(
				"t3",
				{ item: p.identifier, summary: "wired the tool surface", next_action: "write tests" },
				null,
				null,
				ctx,
			);
			expect(repeated.content[0].text.toLowerCase()).toContain("no substantive change");

			// attention
			const asked = await tool(api, "apm_attention").execute(
				"t4",
				{
					item: p.identifier,
					question: "Ship it?",
					context: "CI is green and the tool surface is covered.",
				},
				null,
				null,
				ctx,
			);
			expect(asked.content[0].text).toContain("attention");

			// dashboard
			const board = await tool(api, "apm_dashboard").execute("t5", {}, null, null, ctx);
			expect(board.content[0].text).toContain("WAITING FOR HUMAN");
			expect(board.content[0].text).toContain(p.identifier);
		} finally {
			if (previousBin === undefined) delete process.env.APM_BIN;
			else process.env.APM_BIN = previousBin;
		}

		const detail = apmJson(["get", p.identifier], p.dir, env) as {
			item: { progress_summary: string; version: number; attention: unknown[] };
		};
		expect(detail.item.progress_summary).toBe("wired the tool surface");
		expect(detail.item.version).toBe(2);
		expect(detail.item.attention).toHaveLength(1);
	});
});

describe("run-end hook", () => {
	it("spawns a guarded side-run that syncs the claimed item with per-item idempotency", async () => {
		const p = project();
		const fakePi = writeFakePi();
		const logPath = path.join(p.dir, "fake-pi.json");
		const api = fakeApi({
			"apm-agent": "pi",
			"apm-side-run-timeout": "60",
			"apm-disabled": false,
		});
		extension(api as unknown as Parameters<typeof extension>[0]);
		expect(api.tools.has("apm_claim")).toBe(true);
		expect(api.handlers.has("agent_settled")).toBe(true);

		const ctx = fakeCtx(p.dir, p.sessionId);
		const previousEnv = {
			APM_BIN: process.env.APM_BIN,
			APM_SIDE_RUN_PI: process.env.APM_SIDE_RUN_PI,
			FAKE_PI_LOG: process.env.FAKE_PI_LOG,
			APM_DB: process.env.APM_DB,
		};
		process.env.APM_BIN = apmBin;
		process.env.APM_SIDE_RUN_PI = fakePi;
		process.env.FAKE_PI_LOG = logPath;
		process.env.APM_DB = path.join(p.dir, ".apm", "apm.db");
		try {
			await fire(api, "agent_start", {}, ctx);
			await fire(
				api,
				"agent_end",
				{
					messages: [
						{ role: "user", content: "fix the parser crash" },
						{
							role: "assistant",
							content: [
								{ type: "text", text: "patched src/parse.rs and ran cargo test" },
								{ type: "toolCall", name: "edit", arguments: { file_path: "src/parse.rs" } },
							],
						},
					],
				},
				ctx,
			);
			await fire(api, "agent_settled", { aborted: false }, ctx);
		} finally {
			for (const [key, value] of Object.entries(previousEnv)) {
				if (value === undefined) delete process.env[key];
				else process.env[key] = value;
			}
		}

		// The child was invoked as a real pi run: JSON mode, one-shot, isolated.
		const log = JSON.parse(fs.readFileSync(logPath, "utf8")) as {
			args: string[];
			prompt?: string;
			env: Record<string, string>;
			apmOnPath: boolean;
			apmVersion: string;
		};
		expect(log.args).toEqual(expect.arrayContaining(["--mode", "json", "-p", "--no-session", "--no-extensions"]));
		expect(log.env.APM_SIDE_RUN).toBe("1");
		expect(log.env.APM_SESSION_ID).toBe(p.sessionId);
		expect(log.env.APM_RUN_ID).toBeTruthy();
		// Neither the argv (visible in `ps`) nor the prompt may contain a token.
		expect(JSON.stringify(log)).not.toMatch(/apm_[0-9a-f]{32,}/);
		expect(log.prompt).toContain("--session");
		// ...but `apm` must be resolvable from the child's PATH: a configured
		// --apm-bin is exposed privately, otherwise the model's bash calls fail.
		expect(log.apmOnPath).toBe(true);
		expect(log.apmVersion).toContain("apm 0");

		// The ledger reflects the child's writes.
		const env = { APM_DB: path.join(p.dir, ".apm", "apm.db") };
		const detail = apmJson(["get", p.identifier], p.dir, env) as {
			item: { progress_summary: string; version: number; last_run: { outcome: string } | null };
		};
		expect(detail.item.progress_summary).toBe("patched the parser and ran the suite");
		expect(detail.item.last_run?.outcome).toBe("completed");

		const events = apmJson(["events", p.identifier], p.dir, env) as {
			events: Array<{ kind: string; actor: string; idempotency_key: string | null }>;
		};
		const synced = events.events.filter((e) => e.kind === "run_synced");
		expect(synced).toHaveLength(1);
		expect(
			events.events.filter(
				(e) => e.kind === "progress_recorded" && e.actor.endsWith("side-run"),
			),
		).toHaveLength(1);

		const report = api.entries.find((e) => e.customType === "apm-run-sync");
		expect(report).toBeTruthy();
		expect(report?.data).toMatchObject({ ran: true, mode: "side-run", items: [p.identifier] });
	});

	it("takes the deterministic abort path: liveness only, no progress, no status change", async () => {
		const p = project();
		const logPath = path.join(p.dir, "fake-pi-abort.json");
		const api = fakeApi({ "apm-agent": "pi", "apm-side-run-timeout": "60" });
		extension(api as unknown as Parameters<typeof extension>[0]);

		const ctx = fakeCtx(p.dir, p.sessionId);
		const previousEnv = {
			APM_BIN: process.env.APM_BIN,
			APM_SIDE_RUN_PI: process.env.APM_SIDE_RUN_PI,
			FAKE_PI_LOG: process.env.FAKE_PI_LOG,
			APM_DB: process.env.APM_DB,
		};
		process.env.APM_BIN = apmBin;
		process.env.APM_SIDE_RUN_PI = writeFakePi();
		process.env.FAKE_PI_LOG = logPath;
		process.env.APM_DB = path.join(p.dir, ".apm", "apm.db");
		let before: { item: { last_progress_at: string; status: string; version: number } };
		try {
			before = apmJson(["get", p.identifier], p.dir, {
				APM_DB: path.join(p.dir, ".apm", "apm.db"),
			}) as typeof before;
			await fire(api, "agent_start", {}, ctx);
			await fire(api, "agent_end", { messages: [{ role: "user", content: "fix the parser" }] }, ctx);
			await fire(api, "agent_settled", { aborted: true }, ctx);
		} finally {
			for (const [key, value] of Object.entries(previousEnv)) {
				if (value === undefined) delete process.env[key];
				else process.env[key] = value;
			}
		}

		// No LLM child was spawned for an aborted run.
		expect(fs.existsSync(logPath)).toBe(false);

		const env = { APM_DB: path.join(p.dir, ".apm", "apm.db") };
		const after = apmJson(["get", p.identifier], p.dir, env) as {
			item: { last_progress_at: string; status: string; version: number; last_run: { outcome: string } };
		};
		expect(after.item.last_progress_at).toBe(before.item.last_progress_at);
		expect(after.item.status).toBe(before.item.status);
		expect(after.item.version).toBe(before.item.version);
		expect(after.item.last_run.outcome).toBe("aborted");

		const events = apmJson(["events", p.identifier], p.dir, env) as {
			events: Array<{ kind: string }>;
		};
		expect(events.events.filter((e) => e.kind === "run_aborted")).toHaveLength(1);
		const report = api.entries.find((e) => e.customType === "apm-run-sync");
		expect(report?.data).toMatchObject({ ran: true, mode: "abort-sync", errors: [] });
	});

	it("degrades to a diagnostic when the side-run binary is missing (never throws)", async () => {
		const p = project();
		const api = fakeApi({ "apm-agent": "pi", "apm-side-run-timeout": "30" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);
		const previousBin = process.env.APM_BIN;
		const previousChild = process.env.APM_SIDE_RUN_PI;
		process.env.APM_BIN = path.join(p.dir, "does-not-exist");
		process.env.APM_SIDE_RUN_PI = path.join(p.dir, "also-missing");
		try {
			await fire(api, "agent_start", {}, ctx);
			await fire(api, "agent_end", { messages: [] }, ctx);
			await expect(fire(api, "agent_settled", { aborted: false }, ctx)).resolves.toBeUndefined();
		} finally {
			if (previousBin === undefined) delete process.env.APM_BIN;
			else process.env.APM_BIN = previousBin;
			if (previousChild === undefined) delete process.env.APM_SIDE_RUN_PI;
			else process.env.APM_SIDE_RUN_PI = previousChild;
		}
		const report = api.entries.find((e) => e.customType === "apm-run-sync");
		expect(report).toBeTruthy();
		expect((report?.data as { errors: string[] }).errors.length).toBeGreaterThan(0);
	});
});

// ---------------------------------------------------------------------------
// Review round 2: reconciliation, token hygiene, context tool, error mapping,
// tool metadata.
// ---------------------------------------------------------------------------

async function withEnv<T>(vars: Record<string, string | undefined>, fn: () => Promise<T>): Promise<T> {
	const previous: Record<string, string | undefined> = {};
	for (const key of Object.keys(vars)) previous[key] = process.env[key];
	const apply = () => {
		for (const [key, value] of Object.entries(vars)) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	};
	apply();
	try {
		return await fn();
	} finally {
		for (const [key, value] of Object.entries(previous)) {
			if (value === undefined) delete process.env[key];
			else process.env[key] = value;
		}
	}
}

function lastReport(api: FakeApi) {
	const report = api.entries.filter((entry) => entry.customType === "apm-run-sync").at(-1);
	return report?.data as
		| {
				ran: boolean;
				mode: string;
				runId: string;
				items: string[];
				reconciled: Array<{ identifier: string; action: string; ok: boolean; code: string | null }>;
				errors: string[];
		  }
		| undefined;
}

interface EventRow {
	kind: string;
	actor: string;
	run_id: string | null;
	item_id: string | null;
	idempotency_key: string | null;
}

function eventsOf(dir: string, identifier?: string): EventRow[] {
	const args = identifier ? ["events", identifier] : ["events", "--limit", "500"];
	const out = apmJson(args, dir, { APM_DB: path.join(dir, ".apm", "apm.db") }) as { events: EventRow[] };
	return out.events;
}

describe("run-end reconciliation", () => {
	it("baseline-syncs every claimed item when the child does nothing, and never repeats", async () => {
		const p = project();
		const fakePi = writeFakePi();
		const logPath = path.join(p.dir, "fake-pi-donothing.json");
		const api = fakeApi({ "apm-agent": "agent-a", "apm-side-run-timeout": "60" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);

		await withEnv(
			{
				APM_BIN: apmBin,
				APM_SIDE_RUN_PI: fakePi,
				FAKE_PI_LOG: logPath,
				FAKE_PI_MODE: "do-nothing",
				APM_DB: path.join(p.dir, ".apm", "apm.db"),
			},
			async () => {
				await fire(api, "agent_start", {}, ctx);
				await fire(api, "agent_end", { messages: [{ role: "user", content: "read the code" }] }, ctx);
				await fire(api, "agent_settled", { aborted: false }, ctx);
			},
		);

		const report = lastReport(api);
		expect(report?.ran).toBe(true);
		expect(report?.reconciled).toEqual([
			{ identifier: p.identifier, action: "sync", ok: true, code: null },
		]);

		// The child ran but wrote nothing; the prompt/argv still must not carry a token.
		const log = fs.readFileSync(logPath, "utf8");
		expect(log).not.toMatch(/apm_[0-9a-f]{32,}/);
		expect(log).not.toMatch(/^token: /m);

		const synced = eventsOf(p.dir, p.identifier).filter(
			(e) => e.kind === "run_synced" && e.run_id === report?.runId,
		);
		expect(synced).toHaveLength(1);
		expect(synced[0].idempotency_key).toMatch(/:reconcile:sync#run_synced$/);
		expect(synced[0].actor).toBe("agent-a-side-run");

		// A second settle without a new run must be a no-op (hook re-fire guard).
		const before = eventsOf(p.dir, p.identifier).length;
		await fire(api, "agent_settled", { aborted: false }, ctx);
		expect(eventsOf(p.dir, p.identifier).length).toBe(before);
	});

	it("syncs only the claimed items the child missed", async () => {
		const p = projectTwo();
		const fakePi = writeFakePi();
		const logPath = path.join(p.dir, "fake-pi-first-only.json");
		const api = fakeApi({ "apm-agent": "agent-a", "apm-side-run-timeout": "60" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);

		await withEnv(
			{
				APM_BIN: apmBin,
				APM_SIDE_RUN_PI: fakePi,
				FAKE_PI_LOG: logPath,
				FAKE_PI_MODE: "first-only",
				APM_DB: path.join(p.dir, ".apm", "apm.db"),
			},
			async () => {
				await fire(api, "agent_start", {}, ctx);
				await fire(api, "agent_end", { messages: [{ role: "user", content: "fix both" }] }, ctx);
				await fire(api, "agent_settled", { aborted: false }, ctx);
			},
		);

		const report = lastReport(api);
		expect([...(report?.items ?? [])].sort()).toEqual([p.a, p.b].sort());

		const a = eventsOf(p.dir, p.a).filter((e) => e.run_id === report?.runId);

		// The child processes the first block in the prompt (claims are ordered by
		// lease time), so exactly one item carries child progress.
		const childItem = a.some((e) => e.kind === "progress_recorded") ? p.a : p.b;
		const missedItem = childItem === p.a ? p.b : p.a;

		const actions = new Map((report?.reconciled ?? []).map((r) => [r.identifier, r]));
		expect(actions.get(childItem)?.action).toBe("skip");
		expect(actions.get(missedItem)?.action).toBe("sync");

		const missed = eventsOf(p.dir, missedItem).filter((e) => e.run_id === report?.runId);
		expect(missed.filter((e) => e.kind === "run_synced")).toHaveLength(1);
		expect(missed.some((e) => e.kind === "progress_recorded")).toBe(false);
	});
});

describe("tool error mapping", () => {
	it("surfaces real apm codes and hints instead of apm_error", async () => {
		const p = bareProject();
		const api = fakeApi({ "apm-agent": "agent-a", "apm-side-run-timeout": "60" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);

		await withEnv({ APM_BIN: apmBin, APM_DB: path.join(p.dir, ".apm", "apm.db") }, async () => {
			// not_found
			const missing = await tool(api, "apm_claim").execute("e1", { item: "NOPE-99" }, null, null, ctx);
			let payload = JSON.parse(missing.content[0].text);
			expect(payload.code).toBe("not_found");
			expect(payload.hint).toContain("apm_dashboard");

			// claim_required (no claim)
			const denied = await tool(api, "apm_progress").execute(
				"e2",
				{ item: p.identifier, summary: "sneaky" },
				null,
				null,
				ctx,
			);
			payload = JSON.parse(denied.content[0].text);
			expect(payload.code).toBe("claim_required");
			expect(payload.hint).toContain("apm_claim");

			// claim, then force a stale version through the tool
			await tool(api, "apm_claim").execute("e3", { item: p.identifier }, null, null, ctx);
			const stale = await tool(api, "apm_progress").execute(
				"e4",
				{ item: p.identifier, summary: "too early", expect_version: 99 },
				null,
				null,
				ctx,
			);
			payload = JSON.parse(stale.content[0].text);
			expect(payload.code).toBe("version_conflict");
			expect(payload.hint).toContain("retry");

			// workflow guard
			const illegal = await tool(api, "apm_status").execute(
				"e5",
				{ item: p.identifier, to: "done" },
				null,
				null,
				ctx,
			);
			payload = JSON.parse(illegal.content[0].text);
			expect(payload.code).toBe("workflow_violation");
			expect(payload.hint).toContain("apm workflow");
		});
	});
});

describe("context tool", () => {
	it("shows and revises the handover document with a real version bump", async () => {
		const p = bareProject();
		const api = fakeApi({ "apm-agent": "agent-a", "apm-side-run-timeout": "60" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);

		await withEnv({ APM_BIN: apmBin, APM_DB: path.join(p.dir, ".apm", "apm.db") }, async () => {
			await tool(api, "apm_claim").execute("c1", { item: p.identifier }, null, null, ctx);
			const shown = await tool(api, "apm_context").execute(
				"c2",
				{ item: p.identifier, action: "show" },
				null,
				null,
				ctx,
			);
			expect(shown.content[0].text).toContain("tools drive the ledger");

			const revised = await tool(api, "apm_context").execute(
				"c3",
				{
					item: p.identifier,
					action: "revise",
					reason: "acceptance clarified after the design review",
					current_state: "claimed, waiting for the design review",
				},
				null,
				null,
				ctx,
			);
			const payload = JSON.parse(revised.content[0].text);
			expect(payload.context.version).toBe(2);
			expect(payload.changed_fields).toContain("current_state");
		});

		const detail = apmJson(["get", p.identifier], p.dir, {
			APM_DB: path.join(p.dir, ".apm", "apm.db"),
		}) as { item: { context_version: number }; context: { doc: { current_state: string } } };
		expect(detail.item.context_version).toBe(2);
		expect(detail.context.doc.current_state).toBe("claimed, waiting for the design review");
	});
});

describe("tool audit metadata", () => {
	it("records the active run and configured agent on tool writes", async () => {
		const p = bareProject();
		const fakePi = writeFakePi();
		const logPath = path.join(p.dir, "fake-pi-meta.json");
		const api = fakeApi({ "apm-agent": "agent-a", "apm-side-run-timeout": "60" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);

		await withEnv(
			{
				APM_BIN: apmBin,
				APM_SIDE_RUN_PI: fakePi,
				FAKE_PI_LOG: logPath,
				FAKE_PI_MODE: "do-nothing",
				APM_DB: path.join(p.dir, ".apm", "apm.db"),
			},
			async () => {
				await fire(api, "agent_start", {}, ctx);
				await tool(api, "apm_claim").execute("m1", { item: p.identifier }, null, null, ctx);
				await tool(api, "apm_progress").execute(
					"m2",
					{ item: p.identifier, summary: "wrote the metadata plumbing" },
					null,
					null,
					ctx,
				);
				await fire(api, "agent_end", { messages: [] }, ctx);
				await fire(api, "agent_settled", { aborted: false }, ctx);
			},
		);

		const report = lastReport(api);
		const progress = eventsOf(p.dir, p.identifier).find((e) => e.kind === "progress_recorded");
		expect(progress?.run_id).toBe(report?.runId);
		expect(progress?.actor).toBe("agent-a");
		// The run id is also exported for bash-invoked apm calls, then restored.
		expect(process.env.APM_RUN_ID).toBeUndefined();

		// The activity came from the tool during the run, so reconciliation
		// correctly skipped it — no synthetic side-run event was needed.
		expect(
			eventsOf(p.dir, p.identifier).some(
				(e) => e.run_id === report?.runId && e.actor.endsWith("-side-run") && e.kind === "run_synced",
			),
		).toBe(false);
		expect(report?.reconciled).toEqual([
			{ identifier: p.identifier, action: "skip", ok: true, code: null },
		]);
	});
});

describe("apm on PATH for bash tools", () => {
	it("exposes a configured --apm-bin as `apm` for the main agent and restores PATH", async () => {
		const p = bareProject();
		const api = fakeApi({
			"apm-agent": "agent-a",
			"apm-bin": apmBin,
			"apm-side-run-timeout": "30",
		});
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);
		const before = process.env.PATH;
		const previousBin = process.env.APM_BIN;
		delete process.env.APM_BIN;
		try {
			await fire(api, "session_start", { reason: "startup" }, ctx);
			expect(process.env.PATH).not.toBe(before);
			// The model can now type `apm ...` in its bash tool.
			const version = execFileSync("apm", ["--version"], { encoding: "utf8" }).trim();
			expect(version).toContain("apm 0");
		} finally {
			await fire(api, "session_shutdown", { reason: "quit" }, ctx);
			if (previousBin === undefined) delete process.env.APM_BIN;
			else process.env.APM_BIN = previousBin;
		}
		expect(process.env.PATH).toBe(before);
	});
});

describe("run env lifecycle", () => {
	it("overrides stale APM_RUN_ID/APM_SESSION_ID and restores them on every settle path", async () => {
		const p = bareProject();
		const api = fakeApi({ "apm-agent": "agent-a", "apm-side-run-timeout": "30" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, "current-session");
		const previous = {
			APM_RUN_ID: process.env.APM_RUN_ID,
			APM_SESSION_ID: process.env.APM_SESSION_ID,
			APM_SIDE_RUN_DISABLED: process.env.APM_SIDE_RUN_DISABLED,
			APM_BIN: process.env.APM_BIN,
		};
		process.env.APM_RUN_ID = "stale-run";
		process.env.APM_SESSION_ID = "stale-session";
		// Disabled side-run takes the *early skip return* inside agent_settled:
		// that path must still clean up.
		process.env.APM_SIDE_RUN_DISABLED = "1";
		process.env.APM_BIN = apmBin;
		try {
			await fire(api, "agent_start", {}, ctx);
			expect(process.env.APM_RUN_ID).not.toBe("stale-run");
			expect(process.env.APM_RUN_ID).toMatch(/-\d+$/);
			expect(process.env.APM_SESSION_ID).toBe("current-session");

			await fire(api, "agent_settled", { aborted: false }, ctx);
			expect(process.env.APM_RUN_ID).toBe("stale-run");
			expect(process.env.APM_SESSION_ID).toBe("stale-session");

			// A second run gets a fresh id and leaks nothing either.
			await fire(api, "agent_start", {}, ctx);
			const secondRun = process.env.APM_RUN_ID;
			expect(secondRun).not.toBe("stale-run");
			expect(secondRun).toMatch(/-2$/);
			await fire(api, "agent_settled", { aborted: false }, ctx);
			expect(process.env.APM_RUN_ID).toBe("stale-run");

			// shutdown restores once more even with no active run.
			await fire(api, "session_shutdown", { reason: "quit" }, ctx);
			expect(process.env.APM_RUN_ID).toBe("stale-run");
		} finally {
			for (const [key, value] of Object.entries(previous)) {
				if (value === undefined) delete process.env[key];
				else process.env[key] = value;
			}
		}
	});
});

describe("apm binary precedence", () => {
	it("prefers an explicit --apm-bin over another apm already on PATH", async () => {
		const dir = tmpdir();
		const fakeDir = path.join(dir, "fake-bins");
		fs.mkdirSync(fakeDir, { recursive: true });
		const fakeApm = path.join(fakeDir, "apm");
		fs.writeFileSync(fakeApm, "#!/bin/sh\necho FAKE-APM 9.9.9\n", { mode: 0o755 });

		const originalPath = process.env.PATH;
		process.env.PATH = `${fakeDir}${path.delimiter}${originalPath ?? ""}`;
		const previousBin = process.env.APM_BIN;
		delete process.env.APM_BIN;
		// Sanity: the decoy really is what PATH resolves to before we start.
		expect(execFileSync("apm", ["--version"], { encoding: "utf8" })).toContain("FAKE-APM");

		const api = fakeApi({ "apm-agent": "agent-a", "apm-bin": apmBin, "apm-side-run-timeout": "30" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(dir, "sess-path");
		try {
			await fire(api, "session_start", { reason: "startup" }, ctx);
			// The configured binary wins even though a different `apm` existed.
			expect(execFileSync("apm", ["--version"], { encoding: "utf8" })).toContain("apm 0");
		} finally {
			await fire(api, "session_shutdown", { reason: "quit" }, ctx);
			if (previousBin === undefined) delete process.env.APM_BIN;
			else process.env.APM_BIN = previousBin;
			process.env.PATH = originalPath;
		}
	});
});

describe("side-run recovery", () => {
	it("recovers an expired lease inside the child without exposing the token", async () => {
		const p = projectExpired();
		const fakePi = writeFakePi();
		const logPath = path.join(p.dir, "fake-pi-recover.json");
		const api = fakeApi({ "apm-agent": "agent-a", "apm-side-run-timeout": "60" });
		extension(api as unknown as Parameters<typeof extension>[0]);
		const ctx = fakeCtx(p.dir, p.sessionId);

		await withEnv(
			{
				APM_BIN: apmBin,
				APM_SIDE_RUN_PI: fakePi,
				FAKE_PI_LOG: logPath,
				FAKE_PI_MODE: "recover",
				APM_DB: path.join(p.dir, ".apm", "apm.db"),
			},
			async () => {
				await fire(api, "agent_start", {}, ctx);
				await fire(api, "agent_end", { messages: [{ role: "user", content: "unblock it" }] }, ctx);
				await fire(api, "agent_settled", { aborted: false }, ctx);
			},
		);

		const log = fs.readFileSync(logPath, "utf8");
		// The child really did call claim and captured its output...
		expect(log).toContain("--RECOVER-OUTPUT--");
		expect(log).toContain("\"claim\"");
		// ...and that output carried no plaintext token, so nothing could leak into
		// a progress/context field even if the model echoed it.
		expect(log).not.toMatch(/apm_[0-9a-f]{32,}/);

		const report = lastReport(api);
		expect(report?.errors).toEqual([]);

		const events = eventsOf(p.dir, p.identifier);
		expect(
			events.some((e) => e.kind === "claim_taken_over" && e.actor.endsWith("-side-run")),
		).toBe(true);
		const synced = events.filter((e) => e.kind === "run_synced" && e.run_id === report?.runId);
		expect(synced.length).toBeGreaterThanOrEqual(1);

		// The item is still writable through the session file afterwards.
		const detail = apmJson(["get", p.identifier], p.dir, {
			APM_DB: path.join(p.dir, ".apm", "apm.db"),
		}) as { item: { last_run: { outcome: string } | null; health: string } };
		expect(detail.item.last_run?.outcome).toBe("completed");
	});
});
