/**
 * Real MCP client integration test.
 *
 * This is not a hand-rolled JSONL script: it drives the real `apm-mcp` binary
 * with the official TypeScript SDK (`@modelcontextprotocol/sdk`), i.e. the same
 * code path an MCP host uses — process spawn, `initialize` handshake, capability
 * negotiation, `tools/list`, `tools/call`, protocol errors vs. tool results.
 *
 * Requirements: `cargo build` artifacts next to this repo (built on demand) and
 * a `node` with the SDK installed (`npm install`).
 *
 * What it proves:
 *   1. the full working loop works through a real client (dashboard → get →
 *      claim → doing → progress → context revise → get → release);
 *   2. error paths are structured tool results, not transport failures
 *      (version conflict, claim conflict, lease expiry, idempotent replay);
 *   3. a claim token never appears in anything a client can see.
 */

import { execFileSync } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import {
	StdioClientTransport,
	getDefaultEnvironment,
} from "@modelcontextprotocol/sdk/client/stdio.js";
import { afterAll, describe, expect, it } from "vitest";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const tmpDirs: string[] = [];
/** Anything shaped like a claim token must never reach the client. */
const TOKEN_SHAPE = /apm_[0-9a-f]{32,}/;

afterAll(() => {
	for (const dir of tmpDirs) fs.rmSync(dir, { recursive: true, force: true });
});

function binary(name: "apm" | "apm-mcp"): string {
	const candidate = path.join(repoRoot, "target", "debug", name);
	if (fs.existsSync(candidate)) return candidate;
	execFileSync("cargo", ["build", "--bin", name], {
		cwd: repoRoot,
		stdio: "pipe",
		timeout: 600_000,
	});
	if (!fs.existsSync(candidate)) {
		throw new Error(`cargo build --bin ${name} did not produce ${candidate}`);
	}
	return candidate;
}

type Ledger = { dir: string; db: string };

function apm(ledger: Ledger | null, args: string[], extraEnv: Record<string, string> = {}) {
	const db = ledger?.db;
	const env: Record<string, string> = {
		...getDefaultEnvironment(),
		...(db ? { APM_DB: db } : {}),
		...extraEnv,
	};
	delete env.APM_SESSION_ID;
	delete env.APM_RUN_ID;
	delete env.APM_SIDE_RUN;
	const stdout = execFileSync(binary("apm"), args, {
		env,
		encoding: "utf8",
		timeout: 60_000,
	});
	return JSON.parse(stdout) as Record<string, any>;
}

function makeLedger(): Ledger {
	const dir = fs.mkdtempSync(path.join(os.tmpdir(), "apm-mcp-client-"));
	tmpDirs.push(dir);
	const ledger = { dir, db: path.join(dir, ".apm", "apm.db") };
	apm(ledger, ["init", "--name", "MCP Client Test", "--prefix", "MCT", "--json"]);
	return ledger;
}

function createItem(ledger: Ledger, title: string): string {
	const created = apm(ledger, [
		"create",
		"--title",
		title,
		"--goal",
		"drive the ledger from a real MCP client",
		"--current-state",
		"the server exists and is unit-tested",
		"--constraint",
		"stdio only, no daemon",
		"--reference",
		"docs/mcp.md",
		"--acceptance",
		"a real MCP client can claim and progress this item",
		"--json",
	]);
	return String(created.item.identifier);
}

type Session = {
	client: Client;
	transport: StdioClientTransport;
	stderr: () => string;
};

async function connect(db: string, session: string, extraArgs: string[] = []): Promise<Session> {
	const transport = new StdioClientTransport({
		command: binary("apm-mcp"),
		args: ["--db", db, "--session", session, "--agent", "mcp-client-test", ...extraArgs],
		env: { ...getDefaultEnvironment(), APM_DB: db },
		stderr: "pipe",
	});
	const chunks: string[] = [];
	const sink = transport.stderr;
	sink?.on("data", (chunk: Buffer) => chunks.push(chunk.toString("utf8")));
	const client = new Client({ name: "pi-apm-mcp-client", version: "0.1.0" });
	await client.connect(transport);
	return { client, transport, stderr: () => chunks.join("") };
}

function text(result: Record<string, any>): string {
	const content = result.content as Array<{ type: string; text?: string }> | undefined;
	return (content ?? [])
		.filter((part) => part.type === "text")
		.map((part) => part.text ?? "")
		.join("\n");
}

function structured(result: Record<string, any>): Record<string, any> {
	return (result.structuredContent ?? {}) as Record<string, any>;
}

describe("apm-mcp through the official MCP client", () => {
	it("negotiates capabilities and lists the tool surface", { timeout: 120_000 }, async () => {
		const ledger = makeLedger();
		const session = await connect(ledger.db, "catalogue");
		try {
			const server = session.client.getServerVersion();
			expect(server?.name).toBe("apm-mcp");
			const capabilities = session.client.getServerCapabilities();
			expect(capabilities?.tools).toBeDefined();

			const listed = await session.client.listTools();
			const names = listed.tools.map((tool) => tool.name).sort();
			expect(names).toEqual(
				[
					"apm_claim",
					"apm_context",
					"apm_dashboard",
					"apm_get",
					"apm_progress",
					"apm_release",
					"apm_status",
					"apm_sync",
				].sort(),
			);

			// Workflow-defined state keys must stay open so custom states are usable.
			const status = listed.tools.find((tool) => tool.name === "apm_status");
			const to = (status?.inputSchema.properties as Record<string, any> | undefined)?.to;
			expect(to.type).toBe("string");
			expect(to.enum).toBeUndefined();
			expect(String(status?.description ?? "")).toMatch(/workflow/i);
		} finally {
			await session.client.close();
		}
	});

	it("runs the whole working loop and never leaks a claim token", { timeout: 120_000 }, async () => {
		const ledger = makeLedger();
		const identifier = createItem(ledger, "drive the loop from a real client");
		const session = await connect(ledger.db, "loop");
		const seen: string[] = [];
		const call = async (name: string, args: Record<string, unknown>) => {
			const result = (await session.client.callTool({ name, arguments: args })) as Record<
				string,
				any
			>;
			seen.push(JSON.stringify(result));
			return result;
		};
		try {
			const board = await call("apm_dashboard", {});
			expect(text(board)).toContain("READY / UNCLAIMED (1)");
			expect(text(board)).toContain(identifier);

			const detail = await call("apm_get", { item: identifier });
			const item = structured(detail).item as Record<string, any>;
			expect(item.identifier).toBe(identifier);
			const version = Number(item.version);

			const claimed = await call("apm_claim", { item: identifier, lease_seconds: 600 });
			expect(structured(claimed).token).toBeNull();
			expect(structured(claimed).token_suppressed).toBe(true);
			expect(structured(claimed).token_source).toBe("session_file");
			expect(text(claimed)).not.toMatch(TOKEN_SHAPE);

			const doing = await call("apm_status", {
				item: identifier,
				to: "doing",
				expect_version: version,
			});
			expect(structured(doing).item.status).toBe("doing");
			const doingVersion = Number(structured(doing).item.version);

			const progress = await call("apm_progress", {
				item: identifier,
				summary: "real MCP client drove the loop",
				next_action: "hand it off",
				expect_version: doingVersion,
			});
			expect(structured(progress).substantive).toBe(true);
			expect(text(progress)).toContain("progress recorded");

			const revised = await call("apm_context", {
				item: identifier,
				action: "revise",
				reason: "the transport is verified by a real client now",
				current_state: "verified end to end through @modelcontextprotocol/sdk",
			});
			expect(structured(revised).context.version).toBe(2);

			const reread = await call("apm_get", { item: identifier });
			expect(structured(reread).item.progress_summary).toBe("real MCP client drove the loop");

			const released = await call("apm_release", { item: identifier, reason: "handed off" });
			expect(structured(released).claim.status).toBe("released");

			// Nothing a client saw may contain a token-shaped secret.
			for (const frame of seen) expect(frame).not.toMatch(TOKEN_SHAPE);
			expect(session.stderr()).not.toMatch(TOKEN_SHAPE);
		} finally {
			await session.client.close();
		}
	});

	it("reports domain failures as tool results with codes and hints", { timeout: 120_000 }, async () => {
		const ledger = makeLedger();
		const identifier = createItem(ledger, "exercise the error paths");
		const first = await connect(ledger.db, "errors-a");
		const second = await connect(ledger.db, "errors-b");
		try {
			const detail = await first.client.callTool({ name: "apm_get", arguments: { item: identifier } });
			const version = Number(structured(detail as Record<string, any>).item.version);
			await first.client.callTool({ name: "apm_claim", arguments: { item: identifier } });

			// A write without a claim is a result, not an exception.
			const unclaimed = (await second.client.callTool({
				name: "apm_progress",
				arguments: { item: identifier, summary: "sneaky" },
			})) as Record<string, any>;
			expect(unclaimed.isError).toBe(true);
			expect(structured(unclaimed).error.code).toBe("claim_required");
			expect(String(structured(unclaimed).error.hint)).toMatch(/apm_claim/);

			// A live claim held by another session blocks both claim and write.
			const conflict = (await second.client.callTool({
				name: "apm_claim",
				arguments: { item: identifier },
			})) as Record<string, any>;
			expect(structured(conflict).error.code).toBe("claim_conflict");
			expect(String(structured(conflict).error.hint)).toMatch(/report/);

			// Optimistic locking: a stale version is a structured conflict.
			const stale = (await first.client.callTool({
				name: "apm_progress",
				arguments: { item: identifier, summary: "raced", expect_version: version + 99 },
			})) as Record<string, any>;
			expect(structured(stale).error.code).toBe("version_conflict");
			expect(structured(stale).error.details.expected).toBe(version + 99);
			expect(structured(stale).error.details.actual).toBe(version);

			// Repeating the identical request replays instead of double-writing.
			const key = "e2e-idempotent-progress";
			const once = (await first.client.callTool({
				name: "apm_progress",
				arguments: { item: identifier, summary: "idempotent write", idempotency_key: key, expect_version: version },
			})) as Record<string, any>;
			expect(once.isError).toBeFalsy();
			const replay = (await first.client.callTool({
				name: "apm_progress",
				arguments: { item: identifier, summary: "idempotent write", idempotency_key: key, expect_version: version },
			})) as Record<string, any>;
			expect(structured(replay).item.version).toBe(structured(once).item.version);

			// An unknown tool is a protocol error: the SDK surfaces it as a throw.
			await expect(
				first.client.callTool({ name: "apm_nope", arguments: {} }),
			).rejects.toThrow(/unknown tool/i);
		} finally {
			await first.client.close();
			await second.client.close();
		}
	});

	it("refuses writes on an expired lease and allows explicit recovery", { timeout: 120_000 }, async () => {
		const ledger = makeLedger();
		const identifier = createItem(ledger, "expire the lease mid-flight");
		const t0 = Date.now();

		const owner = await connect(ledger.db, "lease", ["--now", String(t0)]);
		try {
			const claimed = (await owner.client.callTool({
				name: "apm_claim",
				arguments: { item: identifier, lease_seconds: 30 },
			})) as Record<string, any>;
			expect(structured(claimed).claim.lease_seconds).toBe(30);
		} finally {
			await owner.client.close();
		}

		// Same session, later clock: the token is right, the lease is dead.
		const late = await connect(ledger.db, "lease", ["--now", String(t0 + 31_000)]);
		try {
			const expired = (await late.client.callTool({
				name: "apm_progress",
				arguments: { item: identifier, summary: "too late" },
			})) as Record<string, any>;
			expect(structured(expired).error.code).toBe("claim_invalid");
			expect(String(structured(expired).error.message)).toMatch(/expired/);

			const recovered = (await late.client.callTool({
				name: "apm_claim",
				arguments: { item: identifier, takeover: true, reason: "lease expired mid-run" },
			})) as Record<string, any>;
			expect(structured(recovered).takeover).toBe(true);

			const progress = (await late.client.callTool({
				name: "apm_progress",
				arguments: { item: identifier, summary: "back after recovery" },
			})) as Record<string, any>;
			expect(progress.isError).toBeFalsy();
		} finally {
			await late.client.close();
		}
	});
});
