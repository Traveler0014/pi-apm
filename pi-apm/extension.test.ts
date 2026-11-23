/**
 * Robustness tests for the extension shell (APM-56).
 *
 * Three defects motivated this file:
 *
 * 1. the mutable run/side-run state was module-global, so two extension
 *    instances in one process shared a run id and a side-run guard;
 * 2. tool `execute` ignored the AbortSignal pi passes in, so cancelling a tool
 *    call did not reach the child `apm` process;
 * 3. `runProcess` buffered stdout without a cap and never removed its abort
 *    listener, leaking memory on a runaway child or a normal completion.
 */

import { getEventListeners } from "node:events";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import extension, { MAX_STDOUT_CHARS, runProcess } from "./index.ts";

const tmpRoots: string[] = [];
const envKeys = ["APM_RUN_ID", "APM_SESSION_ID", "FAKE_APM_LOG"] as const;
const savedEnv = new Map<string, string | undefined>();

beforeAll(() => {
	for (const key of envKeys) savedEnv.set(key, process.env[key]);
});

afterEach(() => {
	for (const key of envKeys) {
		const value = savedEnv.get(key);
		if (value === undefined) delete process.env[key];
		else process.env[key] = value;
	}
});

afterAll(() => {
	for (const dir of tmpRoots) fs.rmSync(dir, { recursive: true, force: true });
});

function tmpdir(): string {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-apm-ext-"));
	tmpRoots.push(dir);
	return dir;
}

/** A project dir with just enough on disk for `findProjectDir` to accept it. */
function projectDir(): string {
	const dir = tmpdir();
	fs.mkdirSync(path.join(dir, ".apm"), { recursive: true });
	fs.writeFileSync(path.join(dir, ".apm", "apm.db"), "");
	return dir;
}

/** Fake `apm`: records argv, then prints a dashboard-shaped JSON document. */
function fakeApm(): string {
	const dir = tmpdir();
	const file = path.join(dir, "fake-apm.cjs");
	fs.writeFileSync(
		file,
		`#!/usr/bin/env node
const fs = require("node:fs");
const argv = process.argv.slice(2);
if (process.env.FAKE_APM_LOG) fs.appendFileSync(process.env.FAKE_APM_LOG, JSON.stringify(argv) + "\\n");
process.stdout.write(JSON.stringify({ ok: true, buckets: {} }));
`,
		{ mode: 0o755 },
	);
	return file;
}

/** Fake `apm` that never exits: proves cancellation reaches the child. */
function hangingApm(): string {
	const dir = tmpdir();
	const file = path.join(dir, "hang-apm.cjs");
	fs.writeFileSync(file, `#!/usr/bin/env node\nsetTimeout(() => {}, 60_000);\n`, { mode: 0o755 });
	return file;
}

interface FakePi {
	handlers: Map<string, Array<(event: unknown, ctx: unknown) => Promise<unknown> | unknown>>;
	tools: Map<string, unknown>;
	flags: Record<string, unknown>;
	on(event: string, handler: (event: unknown, ctx: unknown) => Promise<unknown> | unknown): void;
	registerTool(tool: { name: string }): void;
	registerCommand(): void;
	registerFlag(name: string, options?: { default?: unknown }): void;
	getFlag(name: string): unknown;
	appendEntry(): void;
	sendMessage(): void;
}

function fakePi(flags: Record<string, unknown> = {}): FakePi {
	const handlers = new Map<string, Array<(event: unknown, ctx: unknown) => Promise<unknown> | unknown>>();
	const tools = new Map<string, unknown>();
	const api: FakePi = {
		handlers,
		tools,
		flags,
		on(event, handler) {
			const list = handlers.get(event) ?? [];
			list.push(handler);
			handlers.set(event, list);
		},
		registerTool(tool) {
			tools.set(tool.name, tool);
		},
		registerCommand() {},
		registerFlag(name, options) {
			if (!(name in api.flags)) api.flags[name] = options?.default;
		},
		getFlag(name) {
			return api.flags[name];
		},
		appendEntry() {},
		sendMessage() {},
	};
	(extension as unknown as (pi: unknown) => void)(api);
	return api;
}

function ctxFor(cwd: string, session = "sess-1") {
	return {
		cwd,
		hasUI: false,
		sessionManager: { getSessionId: () => session },
		ui: { notify: () => undefined },
	};
}

async function fire(api: FakePi, event: string, payload: unknown, ctx: unknown): Promise<void> {
	for (const handler of api.handlers.get(event) ?? []) await handler(payload, ctx);
}

interface LooseTool {
	execute: (...args: unknown[]) => Promise<{ content?: Array<{ text?: string }>; details?: unknown }>;
}

function tool(api: FakePi, name: string): LooseTool {
	const found = api.tools.get(name);
	expect(found, name).toBeDefined();
	return found as LooseTool;
}

function loggedArgv(log: string): string[][] {
	return fs
		.readFileSync(log, "utf8")
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line) as string[]);
}

function runArg(argv: string[]): string | undefined {
	const index = argv.indexOf("--run");
	return index >= 0 ? argv[index + 1] : undefined;
}

describe("extension instances", () => {
	it("keeps run state per instance instead of in module scope", async () => {
		const dir = projectDir();
		const log = path.join(dir, "argv.log");
		process.env.FAKE_APM_LOG = log;
		const bin = fakeApm();
		const ctx = ctxFor(dir);

		const first = fakePi({ "apm-bin": bin, "apm-agent": "pi" });
		await fire(first, "agent_start", {}, ctx);
		await tool(first, "apm_dashboard").execute("call-1", {}, undefined, undefined, ctx);

		// A second instance must not inherit the first instance's run, even though
		// the first one is still active in the same process.
		const second = fakePi({ "apm-bin": bin, "apm-agent": "pi" });
		await tool(second, "apm_dashboard").execute("call-2", {}, undefined, undefined, ctx);
		await fire(second, "agent_start", {}, ctx);
		await tool(second, "apm_dashboard").execute("call-3", {}, undefined, undefined, ctx);

		const calls = loggedArgv(log).filter((argv) => argv[0] === "dashboard");
		expect(calls).toHaveLength(3);
		const firstRun = runArg(calls[0]!);
		expect(firstRun).toBeTruthy();
		// Instance 2 had no run before its own agent_start...
		expect(runArg(calls[1]!)).toBeUndefined();
		// ...and gets its own afterwards, not instance 1's.
		const secondRun = runArg(calls[2]!);
		expect(secondRun).toBeTruthy();
		expect(secondRun).not.toBe(firstRun);
	});

	it("passes the tool AbortSignal to the child and reports cancellation", async () => {
		const dir = projectDir();
		const ctx = ctxFor(dir);
		const api = fakePi({ "apm-bin": hangingApm(), "apm-agent": "pi" });
		const controller = new AbortController();
		const started = Date.now();
		setTimeout(() => controller.abort(), 50);

		const result = await tool(api, "apm_dashboard").execute("call-1", {}, controller.signal, undefined, ctx);
		const payload = JSON.parse(String(result.content?.[0]?.text ?? "{}")) as { code?: string };
		expect(payload.code).toBe("cancelled");
		// SIGKILL on abort; the 500ms fallback is the only wait.
		expect(Date.now() - started).toBeLessThan(5_000);
	});
});

describe("runProcess bounds", () => {
	it("caps stdout and keeps the newest output", async () => {
		const result = await runProcess({
			command: process.execPath,
			args: ["-e", `process.stdout.write("x".repeat(${MAX_STDOUT_CHARS + 2_000})); process.stdout.write("TAIL")`],
			cwd: os.tmpdir(),
			timeoutMs: 30_000,
		});
		expect(result.code).toBe(0);
		expect(result.stdout.length).toBeLessThanOrEqual(MAX_STDOUT_CHARS);
		// The tail survives: the side-run summary reads the last events.
		expect(result.stdout.endsWith("TAIL")).toBe(true);
	});

	it("leaves normal JSON output untouched", async () => {
		const result = await runProcess({
			command: process.execPath,
			args: ["-e", `process.stdout.write(JSON.stringify({ ok: true, items: [1, 2, 3] }))`],
			cwd: os.tmpdir(),
			timeoutMs: 30_000,
		});
		expect(result.stdout).toBe('{"ok":true,"items":[1,2,3]}');
	});

	it("removes its abort listener when the child finishes", async () => {
		const controller = new AbortController();
		const before = getEventListeners(controller.signal, "abort").length;
		await runProcess({
			command: process.execPath,
			args: ["-e", "process.stdout.write('done')"],
			cwd: os.tmpdir(),
			timeoutMs: 30_000,
			signal: controller.signal,
		});
		expect(getEventListeners(controller.signal, "abort")).toHaveLength(before);
	});

	it("removes its abort listener after an abort too", async () => {
		const controller = new AbortController();
		const before = getEventListeners(controller.signal, "abort").length;
		const pending = runProcess({
			command: process.execPath,
			args: ["-e", "setTimeout(() => {}, 60_000)"],
			cwd: os.tmpdir(),
			timeoutMs: 30_000,
			signal: controller.signal,
		});
		setTimeout(() => controller.abort(), 50);
		const result = await pending;
		expect(result.aborted).toBe(true);
		expect(getEventListeners(controller.signal, "abort")).toHaveLength(before);
	});
});
