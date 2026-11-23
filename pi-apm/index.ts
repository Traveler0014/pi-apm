/**
 * pi-apm — AgentPM integration for the pi coding agent.
 *
 * Responsibilities:
 *   1. expose a small tool surface so the main agent can claim, update, and ask
 *      humans for decisions through `apm`;
 *   2. after every settled run, run a *side-run* that synchronises the claimed
 *      work items from the run's actual context (the main agent is not trusted
 *      to remember to call the tools);
 *   3. never break the main reply: every failure path degrades to a persisted
 *      diagnostic entry and an optional warning.
 *
 * Implementation notes that matter:
 *   - the side-run is a child `pi --mode json -p --no-session --no-extensions`
 *     process with `APM_SIDE_RUN=1`; both are recursion guards;
 *   - claims are *never* created by the side-run: it reuses the session's
 *     existing claim tokens, and may only recover a lease this same session held
 *     (`apm claim --recover-only`), because the Rust side only allows recovery of
 *     an item previously claimed by that session;
 *   - idempotency keys are per (session, run, item, operation), so one run can
 *     update several items and a retry never duplicates an event.
 */

import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { StringEnum } from "@earendil-works/pi-ai";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import {
	type ClaimSnapshot,
	type RunDigest,
	type SideRunOutput,
	buildSideRunSystemPrompt,
	buildSideRunTaskPrompt,
	redactSecrets,
	decideRunMode,
	describeItems,
	digestRun,
	emptyDigest,
	findProjectDir,
	nowIso,
	parseClaimListing,
	planAbortSync,
	planReconciliation,
	coveredItemIds,
	preflightSkip,
	summarizeSideRunOutput,
	truncate,
} from "./lib.ts";
import { renderRunSyncEntry } from "./render.ts";

const ENTRY_TYPE = "apm-run-sync";

interface ActiveRun {
	id: string;
	seq: number;
	startedAt: string;
	digest: RunDigest;
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

function getPiInvocation(args: string[]): { command: string; args: string[] } {
	const currentScript = process.argv[1];
	const isBunVirtualScript =
		currentScript !== undefined &&
		(currentScript.startsWith("/$bunfs/root/") || /^[A-Za-z]:[\\/]~BUN[\\/]root[\\/]/.test(currentScript));
	if (currentScript && !isBunVirtualScript && fs.existsSync(currentScript)) {
		return { command: process.execPath, args: [currentScript, ...args] };
	}
	const execName = path.basename(process.execPath).toLowerCase();
	if (!/^(node|bun)(\.exe)?$/.test(execName)) {
		return { command: process.execPath, args };
	}
	return { command: "pi", args };
}

interface ProcessResult {
	code: number | null;
	stdout: string;
	stderr: string;
	timedOut: boolean;
	/** True when `signal` aborted: the child was killed on purpose, not failed. */
	aborted: boolean;
	spawnError?: string;
}

/**
 * Hard cap on a child's stdout buffer.
 *
 * Five million characters is far above any legitimate `apm --json` payload or
 * side-run event stream (those are already bounded by the digest/prompt limits),
 * and far below what a runaway child would need to exhaust memory. A capped
 * buffer keeps the *tail*: JSON event streams end with the messages that matter,
 * and the line parser skips the one partial first line.
 */
export const MAX_STDOUT_CHARS = 5_000_000;

export function runProcess(input: {
	command: string;
	args: string[];
	cwd: string;
	env?: Record<string, string>;
	timeoutMs: number;
	signal?: AbortSignal;
	/** Own process group, so cancellation reaches `apm` and any grandchildren. */
	detached?: boolean;
}): Promise<ProcessResult> {
	return new Promise((resolve) => {
		let child: ReturnType<typeof spawn>;
		let stdout = "";
		let stderr = "";
		let settled = false;
		let timedOut = false;
		let aborted = false;
		let killTimer: ReturnType<typeof setTimeout> | undefined;
		let abortFallback: ReturnType<typeof setTimeout> | undefined;
		let onAbort: (() => void) | undefined;
		const finish = (result: ProcessResult) => {
			if (settled) return;
			settled = true;
			if (killTimer) clearTimeout(killTimer);
			if (abortFallback) clearTimeout(abortFallback);
			// A normal exit never fires the abort listener; leaving it attached would
			// keep this closure (and the child object) alive for the session's lifetime.
			if (input.signal && onAbort) input.signal.removeEventListener("abort", onAbort);
			resolve(result);
		};
		try {
			child = spawn(input.command, input.args, {
				cwd: input.cwd,
				env: { ...process.env, ...input.env },
				stdio: ["ignore", "pipe", "pipe"],
				detached: input.detached === true,
			});
		} catch (error) {
			finish({
				code: null,
				stdout: "",
				stderr: "",
				timedOut: false,
				aborted: false,
				spawnError: error instanceof Error ? error.message : String(error),
			});
			return;
		}
		child.stdout?.on("data", (chunk: Buffer | string) => {
			stdout += chunk.toString();
			if (stdout.length > MAX_STDOUT_CHARS) {
				// Bounded memory: keep the newest output, which is what the side-run
				// summary and the exit diagnostics need.
				stdout = stdout.slice(-MAX_STDOUT_CHARS);
			}
		});
		child.stderr?.on("data", (chunk: Buffer | string) => {
			stderr += chunk.toString();
			if (stderr.length > 20_000) stderr = stderr.slice(-20_000);
		});
		// The side-run child runs `apm`, and the model CLI can spawn further
		// processes. `child.kill` would orphan those grandchildren, so a detached
		// child is signalled through its process group. Windows has no equivalent
		// here, so it keeps the direct kill.
		const killTree = (signal: NodeJS.Signals): void => {
			if (input.detached === true && process.platform !== "win32" && child.pid) {
				try {
					process.kill(-child.pid, signal);
					return;
				} catch {
					// Already gone, or no process group: fall back to the direct kill.
				}
			}
			try {
				child.kill(signal);
			} catch {
				// Already gone.
			}
		};
		// SIGTERM first so the child can flush, then SIGKILL: a wedged child (the
		// DNS/libuv hang this was written for) ignores the polite signal.
		const escalate = (): void => {
			killTree("SIGTERM");
			killTimer = setTimeout(() => killTree("SIGKILL"), 3_000);
		};
		const timer = setTimeout(() => {
			timedOut = true;
			escalate();
		}, input.timeoutMs);
		child.on("error", (error) => {
			clearTimeout(timer);
			finish({
				code: null,
				stdout,
				stderr,
				timedOut,
				aborted,
				spawnError: error instanceof Error ? error.message : String(error),
			});
		});
		child.on("close", (code) => {
			clearTimeout(timer);
			finish({ code, stdout, stderr, timedOut, aborted });
		});
		if (input.signal) {
			const abort = () => {
				aborted = true;
				// Quitting must be immediate: a wedged child ignores SIGTERM, so the
				// ordinary 3s escalation would make every /quit stall. SIGKILL now; the
				// `close` event still fires, so the child is reaped before we resolve.
				killTree("SIGKILL");
				// `close` waits for the child to exit *and* for stdout/stderr to reach
				// EOF. A grandchild that left the process group (setsid) survives the
				// group kill and keeps those pipes open, so `close` would never fire.
				// Drop our read ends and cap the wait: quitting must not block on
				// something we cannot reap. A process stuck in uninterruptible sleep
				// (D state) ignores SIGKILL too, and this cap covers that as well.
				child.stdout?.destroy();
				child.stderr?.destroy();
				abortFallback = setTimeout(
					() => finish({ code: null, stdout, stderr, timedOut, aborted }),
					500,
				);
			};
			onAbort = abort;
			if (input.signal.aborted) abort();
			else input.signal.addEventListener("abort", abort, { once: true });
		}
	});
}

// ---------------------------------------------------------------------------
// apm invocation
// ---------------------------------------------------------------------------

interface ApmError {
	code: string;
	message: string;
	details: unknown;
	exitCode: number;
}

interface ApmResult<T = unknown> {
	ok: boolean;
	data?: T;
	error?: ApmError;
	raw: string;
}

class ApmBinaryError extends Error {}

async function callApm(
	ctx: ExtensionContext,
	args: string[],
	options: { env?: Record<string, string>; timeoutMs?: number; signal?: AbortSignal } = {},
): Promise<ApmResult> {
	const projectDir = findProjectDir(ctx.cwd);
	if (!projectDir) {
		throw new ApmBinaryError(
			"no .apm/apm.db found: run `apm init --name <project>` in this repository first",
		);
	}
	const bin = (ctx as unknown as { __apmBin?: string }).__apmBin ?? process.env.APM_BIN ?? "apm";
	const result = await runProcess({
		command: bin,
		args,
		cwd: projectDir,
		env: options.env,
		timeoutMs: options.timeoutMs ?? 30_000,
		signal: options.signal,
	});
	if (result.spawnError) {
		throw new ApmBinaryError(
			`cannot execute \`${bin}\` (${result.spawnError}). Install the apm CLI or pass --apm-bin <path>.`,
		);
	}
	if (result.aborted) {
		// Session shutdown, not a failure: callers skip diagnostics on this code.
		return {
			ok: false,
			error: {
				code: "cancelled",
				message: "apm call cancelled: the session is shutting down",
				details: {},
				exitCode: -1,
			},
			raw: "",
		};
	}
	const stdout = result.stdout.trim();
	const stderr = result.stderr.trim();
	const parseJson = (text: string): unknown => {
		try {
			return text ? JSON.parse(text) : null;
		} catch {
			return null;
		}
	};

	// Success: a single JSON document on stdout.
	const parsed = parseJson(stdout);
	if (parsed && typeof parsed === "object" && (parsed as { ok?: boolean }).ok === true) {
		const { ok: _ok, ...data } = parsed as Record<string, unknown>;
		return { ok: true, data, raw: stdout };
	}

	// Failure: the CLI writes `{ok:false,error:{code,message,details,exit_code}}`
	// to *stderr* and leaves stdout empty. Parse it there so the model sees
	// `claim_required` / `version_conflict` / `not_found` and not a stringified
	// blob labelled `apm_error`.
	const stderrJson = parseJson(stderr) as { error?: ApmError } | null;
	const stdoutJson = parsed as { error?: ApmError } | null;
	const envelope = stderrJson?.error ? stderrJson : stdoutJson;
	const error: ApmError =
		envelope?.error ??
		({
			code: result.timedOut ? "timeout" : "apm_error",
			message: result.timedOut
				? `apm timed out after ${options.timeoutMs ?? 30_000}ms`
				: stderr
					? truncate(stderr, 800)
					: `apm exited with ${result.code} and produced no JSON`,
			details: {},
			exitCode: result.code ?? -1,
		} as ApmError);
	return { ok: false, error, raw: stderr || stdout };
}

function jsonText(value: unknown): { type: "text"; text: string } {
	return { type: "text", text: JSON.stringify(value, null, 2) };
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

function sessionIdOf(ctx: ExtensionContext): string | null {
	try {
		const id = ctx.sessionManager.getSessionId();
		if (typeof id === "string" && id.trim()) return id;
	} catch {
		// fall through
	}
	const env = process.env.PI_SESSION_ID;
	return env && env.trim() ? env : null;
}

async function loadClaims(
	ctx: ExtensionContext,
	sessionId: string,
	signal?: AbortSignal,
): Promise<ClaimSnapshot[]> {
	// No `--with-tokens`: claim tokens stay in the 0600 session file and are
	// resolved by the CLI itself. A token must never enter a prompt, a child
	// argv, or a tool result.
	const result = await callApm(
		ctx,
		["claims", "--session", sessionId, "--include-expired", "--json"],
		{ signal },
	);
	if (!result.ok) return [];
	return parseClaimListing(result.data);
}

// ---------------------------------------------------------------------------
// Side-run
// ---------------------------------------------------------------------------

interface SideRunReport {
	ran: boolean;
	mode: "side-run" | "abort-sync" | "skip";
	reason: string;
	/** True when the session shut down and the side-run was cancelled on purpose. */
	aborted?: boolean;
	runId: string | null;
	sessionId: string | null;
	items: string[];
	calls: number;
	childEvents: number;
	reconciled: ReconResult[];
	output?: string;
	errors: string[];
	durationMs: number;
	at: string;
}

function appendReport(pi: ExtensionAPI, report: SideRunReport): void {
	try {
		pi.appendEntry(ENTRY_TYPE, report);
	} catch {
		// Diagnostics must never break the session.
	}
}

async function runSideRun(
	ctx: ExtensionContext,
	input: {
		claims: ClaimSnapshot[];
		runId: string;
		sessionId: string;
		projectDir: string;
		digest: RunDigest;
		startedAt: string;
		timeoutMs: number;
		model?: string;
		agent: string;
		/** Aborted when the session shuts down: the child must not outlive it. */
		signal?: AbortSignal;
	},
): Promise<SideRunOutput> {
	const systemPrompt = buildSideRunSystemPrompt();
	const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "apm-side-run-"));
	const systemPromptPath = path.join(tmpDir, "system.md");
	fs.writeFileSync(systemPromptPath, systemPrompt, { mode: 0o600 });

	// The child calls `apm` through its bash tool. `apm` is not necessarily on the
	// ambient PATH (the extension may have been configured with --apm-bin), so
	// expose the resolved binary as `apm` inside a private temp dir. Without this
	// the child burns its whole timeout on "command not found".
	const resolvedBin = (ctx as unknown as { __apmBin?: string }).__apmBin ?? process.env.APM_BIN;
	let pathPrefix: string | null = null;
	let binForChild = "apm";
	if (resolvedBin && resolvedBin !== "apm" && fs.existsSync(resolvedBin)) {
		try {
			const binDir = path.join(tmpDir, "bin");
			fs.mkdirSync(binDir);
			const target = path.resolve(resolvedBin);
			fs.symlinkSync(target, path.join(binDir, "apm"));
			pathPrefix = binDir;
			binForChild = target;
		} catch {
			binForChild = path.resolve(resolvedBin);
		}
	}
	const task = buildSideRunTaskPrompt({
		sessionId: input.sessionId,
		runId: input.runId,
		projectDir: input.projectDir,
		dbPath: path.join(input.projectDir, ".apm", "apm.db"),
		claims: input.claims,
		digest: input.digest,
		startedAt: input.startedAt,
		finishedAt: nowIso(),
	});

	const args: string[] = [
		"--mode",
		"json",
		"-p",
		"--no-session",
		"--no-extensions",
		"--tools",
		"bash",
		"--append-system-prompt",
		systemPromptPath,
	];
	if (input.model) args.push("--model", input.model);
	args.push(task);

	// `APM_SIDE_RUN_PI` overrides the child command. It exists for packaged
	// installs whose `pi` is not on PATH, and for the end-to-end test that
	// simulates the child without a second real model call.
	const override = process.env.APM_SIDE_RUN_PI;
	const invocation = override ? { command: override, args } : getPiInvocation(args);
	const env: Record<string, string> = {
		APM_SIDE_RUN: "1",
		APM_SESSION_ID: input.sessionId,
		APM_RUN_ID: input.runId,
		APM_AGENT: input.agent,
		APM_DB: path.join(input.projectDir, ".apm", "apm.db"),
		APM_BIN: binForChild,
		...(pathPrefix
			? { PATH: `${pathPrefix}${path.delimiter}${process.env.PATH ?? ""}` }
			: {}),
	};
	const result = await runProcess({
		command: invocation.command,
		args: invocation.args,
		cwd: input.projectDir,
		env,
		timeoutMs: input.timeoutMs,
		signal: input.signal,
		// A detached child owns a process group, which is how cancellation reaches
		// the `apm` grandchildren. Windows has no equivalent and would flash a new
		// console, so it stays attached and relies on the direct kill.
		detached: process.platform !== "win32",
	});
	try {
		fs.rmSync(tmpDir, { recursive: true, force: true });
	} catch {
		// best effort
	}
	const summary = summarizeSideRunOutput(result.stdout);
	if (result.aborted) {
		// The session is going away and the child was killed on purpose. Callers
		// record this as a cancellation, never as a failure or a timeout.
		summary.aborted = true;
		return summary;
	}
	if (result.spawnError) summary.errors.push(result.spawnError);
	if (result.timedOut) summary.errors.push(`side-run timed out after ${input.timeoutMs}ms`);
	if (result.code !== null && result.code !== 0) {
		summary.errors.push(`side-run exited ${result.code}: ${truncateStderr(result.stderr)}`);
	}
	if (!summary.text && result.stderr.trim()) {
		summary.errors.push(`side-run stderr: ${truncateStderr(result.stderr)}`);
	}
	return summary;
}

function truncateStderr(stderr: string): string {
	const clean = stderr.replace(/\s+/g, " ").trim();
	return clean.length > 400 ? `${clean.slice(0, 399)}…` : clean;
}

async function applyAbortPlan(
	ctx: ExtensionContext,
	input: {
		claims: ClaimSnapshot[];
		runId: string;
		sessionId: string;
		reason: string;
		agent: string;
		meta: string[];
		signal?: AbortSignal;
	},
): Promise<{ calls: number; errors: string[] }> {
	const plan = planAbortSync(input);
	let calls = 0;
	const errors: string[] = [];
	for (const call of plan) {
		if (input.signal?.aborted) break;
		// Full metadata: the CLI resolves the claim token from this session's file,
		// and the event carries the run id so reconciliation can see it.
		const result = await callApm(ctx, [...call.args, ...input.meta], {
			timeoutMs: 15_000,
			signal: input.signal,
		});
		calls++;
		if (result.error?.code === "cancelled") break;
		if (!result.ok) {
			errors.push(`${call.kind} ${call.identifier}: ${result.error?.code} ${result.error?.message}`);
			// Best-effort project-level diagnostic: a failed abort sync is exactly
			// the kind of thing a human must see in `apm doctor`.
			await callApm(
				ctx,
				[
					"diag",
					"--item",
					call.identifier,
					"--code",
					"side_run_failed",
					"--message",
					`${call.kind} failed after abort: ${result.error?.code ?? "error"}`,
					"--idempotency-key",
					`${call.key}:failed`,
					"--json",
				],
				{ timeoutMs: 15_000, signal: input.signal },
			).catch(() => undefined);
		}
	}
	return { calls, errors };
}

/**
 * Deterministic post-hook reconciliation.
 *
 * The LLM side-run is best-effort; this step is the guarantee. It looks for an
 * event from *this run* on every claimed item (progress or run sync) and fills
 * the gap with a baseline `apm sync` — never progress, never status, never a
 * release. Items whose lease died get a diagnostic instead.
 */
interface ReconResult {
	identifier: string;
	action: string;
	ok: boolean;
	code: string | null;
}

async function reconcileRun(
	ctx: ExtensionContext,
	input: {
		claims: ClaimSnapshot[];
		runId: string;
		sessionId: string;
		agent: string;
		meta: string[];
		aborted: boolean;
		childFailed: boolean;
		signal?: AbortSignal;
	},
): Promise<{ results: ReconResult[]; errors: string[] }> {
	if (input.claims.length === 0) return { results: [], errors: [] };

	const events = await callApm(ctx, ["events", "--limit", "1000", "--json", ...input.meta], {
		signal: input.signal,
	});
	const covered = events.ok
		? coveredItemIds(
				((events.data as { events?: unknown[] }).events ?? []) as unknown[],
				input.runId,
			)
		: new Set<string>();

	// Re-read claims so a lease that expired (or was released) during the child
	// run is treated honestly.
	const after = await loadClaims(ctx, input.sessionId, input.signal);
	const afterById = new Map(after.map((claim) => [claim.itemId, claim]));
	const items = input.claims.map((claim) => {
		const current = afterById.get(claim.itemId);
		return {
			itemId: claim.itemId,
			identifier: claim.identifier,
			claimActive:
				Boolean(current) &&
				current?.claimStatus === "active" &&
				current?.claimExpired !== true,
		};
	});

	const steps = planReconciliation({
		items,
		covered,
		runId: input.runId,
		sessionId: input.sessionId,
		aborted: input.aborted,
		childFailed: input.childFailed,
	});

	const results: ReconResult[] = [];
	const errors: string[] = [];
	for (const step of steps) {
		if (input.signal?.aborted) break;
		if (step.action === "skip") {
			results.push({ identifier: step.identifier, action: step.action, ok: true, code: null });
			continue;
		}
		const result = await callApm(ctx, [...step.args, ...input.meta], {
			timeoutMs: 15_000,
			signal: input.signal,
		});
		results.push({
			identifier: step.identifier,
			action: step.action,
			ok: result.ok,
			code: result.ok ? null : (result.error?.code ?? "apm_error"),
		});
		if (result.error?.code === "cancelled") break;
		if (!result.ok) {
			errors.push(
				`${step.action} ${step.identifier}: ${result.error?.code ?? "error"} ${result.error?.message ?? ""}`,
			);
			await callApm(
				ctx,
				[
					"diag",
					"--item",
					step.identifier,
					"--code",
					"side_run_reconcile_failed",
					"--message",
					truncate(`${step.action} failed: ${result.error?.code ?? "error"}`, 300),
					"--idempotency-key",
					`${step.key}:failed`,
					"--json",
					...input.meta,
				],
				{ timeoutMs: 15_000, signal: input.signal },
			).catch(() => undefined);
		}
	}
	return { results, errors };
}

// ---------------------------------------------------------------------------
// Extension
// ---------------------------------------------------------------------------

export default function (pi: ExtensionAPI) {
	// Per-instance state. Everything the extension mutates lives in this closure:
	// two pi-apm instances in one process (tests, a future multi-session host)
	// must not share a run, a side-run guard, an injected env list or a PATH shim.
	let activeRun: ActiveRun | null = null;
	let runSeq = 0;
	let sideRunInFlight = false;
	/// Cancels the in-flight side-run when the session shuts down, so quitting
	/// never waits for a slow/hung child (and never orphans it).
	let sideRunController: AbortController | null = null;
	/// Env vars this extension injected for bash-invoked `apm` calls, with their
	/// previous values, so every run restores exactly what it changed.
	let injectedEnv: Array<[string, string | undefined]> = [];
	/// Private PATH shim so the model can literally type `apm ...` in its bash
	/// tool even when the extension was configured with `--apm-bin /some/path/apm`.
	let pathShim: { dir: string; originalPath: string } | null = null;

	function restoreInjectedEnv(): void {
		for (const [key, previous] of injectedEnv) {
			if (previous === undefined) delete process.env[key];
			else process.env[key] = previous;
		}
		injectedEnv = [];
	}
	// Durable run-sync reports are rendered as a compact entry; expanded shows
	// reconciliation details, output and the run/session identifiers.
	// Guarded so older Pi builds (and test doubles) without entry renderers still
	// load; the reports are persisted either way.
	if (typeof pi.registerEntryRenderer === "function") {
		pi.registerEntryRenderer(ENTRY_TYPE, renderRunSyncEntry);
	}

	pi.registerFlag("apm-disabled", {
		description: "Disable the pi-apm run-end side-run (tools still work)",
		type: "boolean",
		default: false,
	});
	pi.registerFlag("apm-bin", {
		description: "Path to the apm binary (default: $APM_BIN or `apm` on PATH)",
		type: "string",
	});
	pi.registerFlag("apm-agent", {
		description: "Actor name recorded for tool and side-run writes",
		type: "string",
		default: "pi",
	});
	pi.registerFlag("apm-side-run-model", {
		description: "Model override for the run-end side-run (default: inherit)",
		type: "string",
	});
	pi.registerFlag("apm-side-run-timeout", {
		description: "Side-run timeout in seconds (default 180)",
		type: "string",
		default: "180",
	});

	const flagString = (name: string): string | undefined => {
		const value = pi.getFlag(name);
		return typeof value === "string" && value.trim() ? value : undefined;
	};

	/**
	 * CLI metadata shared by every write and read this extension performs:
	 * `--session` (claim-token resolution), `--run` (audit correlation) and
	 * `--agent` (actor identity). Kept in one place so tools cannot forget it.
	 */
	function metaArgs(
		ctx: ExtensionContext,
		options: { runId?: string | null; agent?: string; sessionId?: string | null } = {},
	): string[] {
		const out: string[] = [];
		const session = options.sessionId ?? sessionIdOf(ctx);
		if (session) out.push("--session", session);
		const runId = options.runId === undefined ? activeRun?.id : options.runId;
		if (runId) out.push("--run", runId);
		const agent = options.agent ?? flagString("apm-agent");
		if (agent) out.push("--agent", agent);
		return out;
	}

	const whichApm = (): string | null => {
		for (const entry of (process.env.PATH ?? "").split(path.delimiter)) {
			if (!entry) continue;
			const candidate = path.join(entry, "apm");
			try {
				if (fs.existsSync(candidate)) return fs.realpathSync(candidate);
			} catch {
				// unreadable PATH entry: keep looking
			}
		}
		return null;
	};

	/**
	 * An explicit `--apm-bin` must win over whatever `apm` happens to be on PATH:
	 * we only skip the shim when the configured binary *is* the one PATH resolves
	 * to. Comparing real paths also handles symlinked installs.
	 */
	const ensureApmOnPath = (bin: string | undefined): void => {
		if (!bin || bin === "apm" || pathShim) return;
		if (!fs.existsSync(bin)) return;
		let target: string;
		try {
			target = fs.realpathSync(bin);
		} catch {
			return;
		}
		if (whichApm() === target) return;
		try {
			const dir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-apm-bin-"));
			fs.symlinkSync(target, path.join(dir, "apm"));
			pathShim = { dir, originalPath: process.env.PATH ?? "" };
			process.env.PATH = `${dir}${path.delimiter}${process.env.PATH ?? ""}`;
		} catch {
			// Best effort: `$APM_BIN` and the tool surface still work.
		}
	};

	const withBin = <T extends ExtensionContext>(ctx: T): T => {
		const bin = flagString("apm-bin") ?? process.env.APM_BIN;
		if (bin) (ctx as unknown as { __apmBin?: string }).__apmBin = bin;
		return ctx;
	};

	pi.on("session_start", async (_event, ctx) => {
		activeRun = null;
		runSeq = 0;
		sideRunInFlight = false;
		ensureApmOnPath(flagString("apm-bin") ?? process.env.APM_BIN ?? undefined);
		// Resolve the binary once so tools and the side-run agree.
		const bin = flagString("apm-bin") ?? process.env.APM_BIN;
		if (bin) withBin(ctx);
	});

	pi.on("agent_start", async (_event, ctx) => {
		if (!activeRun) {
			runSeq += 1;
			activeRun = {
				id: `${nowIso()}-${runSeq}`,
				seq: runSeq,
				startedAt: nowIso(),
				digest: emptyDigest(),
			};
		}
		// bash tool calls inherit this process env. Always write the *current* run
		// and session (a stale APM_RUN_ID from a previous run would otherwise make
		// bash writes look like they belong to the old run), remembering the
		// previous values so every exit path can restore them.
		restoreInjectedEnv();
		const exports: Array<[string, string | null]> = [
			["APM_RUN_ID", activeRun.id],
			["APM_SESSION_ID", sessionIdOf(ctx)],
		];
		for (const [key, value] of exports) {
			if (!value) continue;
			injectedEnv.push([key, process.env[key]]);
			process.env[key] = value;
		}
	});

	pi.on("agent_end", async (event, _ctx) => {
		if (!activeRun) return;
		try {
			activeRun.digest = digestRun(event.messages as unknown[], activeRun.digest);
		} catch {
			// A malformed message must not affect the main run.
		}
	});

	pi.on("agent_settled", async (event, ctx) => {
		const run = activeRun;
		activeRun = null;
		// Declared before the try so every exit path (including the finally) can
		// clear it. Shutdown aborts it and the signal reaches the child process
		// group and every `apm` call made on this path.
		const controller = new AbortController();
		const signal = controller.signal;
		try {
			if (!run) return;
			const sessionId = sessionIdOf(ctx);
			const projectDir = findProjectDir(ctx.cwd);
			// Audit clarity: side-run writes must be distinguishable from the main
			// agent's own writes for the same session.
			const agentFlag = flagString("apm-agent");
			const agent = agentFlag ? `${agentFlag}-side-run` : "side-run";
			// A positive value in seconds; no hidden 15s floor, so
			// `--apm-side-run-timeout 5` really means five seconds. Anything
			// unusable falls back to the documented default of 180s.
			const configuredSeconds = Number(pi.getFlag("apm-side-run-timeout") ?? "180");
			const timeoutSeconds =
				Number.isFinite(configuredSeconds) && configuredSeconds > 0 ? configuredSeconds : 180;
			const timeoutMs = Math.round(timeoutSeconds * 1000);

			// Cheap guards first: never touch the DB for a non-APM project, and never
			// spawn a side-run from inside a side-run.
			const skipReason = preflightSkip({
				disabled: pi.getFlag("apm-disabled") === true,
				env: process.env as Record<string, string | undefined>,
				projectDir,
				hasSession: Boolean(sessionId),
			});
			if (skipReason) {
				appendReport(pi, {
					ran: false,
					mode: "skip",
					reason: skipReason,
					runId: run.id,
					sessionId,
					items: [],
					calls: 0,
					childEvents: 0,
					reconciled: [],
					errors: [],
					durationMs: 0,
					at: nowIso(),
				});
				return;
			}
			if (!projectDir || !sessionId) return;
			if (sideRunInFlight) {
				appendReport(pi, {
					ran: false,
					mode: "skip",
					reason: "another side-run is still running",
					runId: run.id,
					sessionId,
					items: [],
					calls: 0,
					childEvents: 0,
					reconciled: [],
					errors: [],
					durationMs: 0,
					at: nowIso(),
				});
				return;
			}

			sideRunInFlight = true;
			sideRunController = controller;
			const started = Date.now();
			const report: SideRunReport = {
				ran: false,
				mode: "skip",
				reason: "",
				runId: run.id,
				sessionId,
				items: [],
				calls: 0,
				childEvents: 0,
				reconciled: [],
				errors: [],
				durationMs: 0,
				at: nowIso(),
			};
			try {
				const safeCtx = withBin(ctx);
				const claims = await loadClaims(safeCtx, sessionId, signal);
				const decision = decideRunMode({
					disabled: pi.getFlag("apm-disabled") === true,
					env: process.env as Record<string, string | undefined>,
					aborted: event.aborted,
					projectDir,
					claims,
					hasSession: true,
				});
				report.mode = decision.mode;
				report.reason = decision.reason;
				report.items = claims.map((c) => c.identifier);

				if (decision.mode === "skip") {
					report.durationMs = Date.now() - started;
					appendReport(pi, report);
					return;
				}

				if (decision.mode === "abort-sync") {
					const meta = metaArgs(safeCtx, { sessionId, runId: run.id, agent });
					const outcome = await applyAbortPlan(safeCtx, {
						claims,
						runId: run.id,
						sessionId,
						agent,
						meta,
						signal,
						reason: "run aborted by the user (Escape/interrupt); liveness only, no progress recorded",
					});
					// The abort plan is deterministic; reconciliation covers anything it
					// could not write (e.g. a lease that expired mid-plan).
					const recon = await reconcileRun(safeCtx, {
						claims,
						runId: run.id,
						sessionId,
						agent,
						meta,
						aborted: true,
						childFailed: false,
						signal,
					});
					report.ran = true;
					report.calls = outcome.calls;
					report.reconciled = recon.results;
					report.errors = [...outcome.errors, ...recon.errors];
					report.durationMs = Date.now() - started;
					appendReport(pi, report);
					if (outcome.errors.length && ctx.hasUI) {
						ctx.ui.notify(
							`APM: aborted-run sync had ${outcome.errors.length} problem(s); see /apm-doctor`,
							"warning",
						);
					}
					return;
				}

				const output = await runSideRun(safeCtx, {
					claims,
					runId: run.id,
					sessionId,
					projectDir,
					digest: run.digest,
					startedAt: run.startedAt,
					timeoutMs,
					model: flagString("apm-side-run-model"),
					agent,
					signal,
				});
				const meta = metaArgs(safeCtx, { sessionId, runId: run.id, agent });
				if (output.aborted || signal.aborted) {
					// The session is shutting down and the child was killed on purpose.
					// Record the truth (cancelled, not failed) and do not reconcile: every
					// extra `apm` call would block the exit this cancellation exists for.
					report.mode = "skip";
					report.reason = "session shutdown: side-run cancelled";
					report.aborted = true;
					report.childEvents = output.events;
					report.durationMs = Date.now() - started;
					appendReport(pi, report);
					return;
				}
				// Guarantee, not hope: every claimed item must carry an event for this
				// run, even when the child did nothing or failed.
				const recon = await reconcileRun(safeCtx, {
					claims,
					runId: run.id,
					sessionId,
					agent,
					meta,
					aborted: false,
					childFailed: output.errors.length > 0,
					signal,
				});
				if (signal.aborted) {
					// Shutdown landed while reconciliation was running: it stopped early, so
					// this run did NOT sync. Reporting `ran: true` here would render as
					// "run synced" for a run that wrote nothing — never read as success.
					report.mode = "skip";
					report.reason = "session shutdown: reconciliation cancelled";
					report.aborted = true;
					report.childEvents = output.events;
					report.reconciled = recon.results;
					report.durationMs = Date.now() - started;
					appendReport(pi, report);
					return;
				}
				report.ran = true;
				report.childEvents = output.events;
				report.output = output.text ? redactSecrets(truncateStderr(output.text)) : undefined;
				report.reconciled = recon.results;
				report.errors = [...output.errors, ...recon.errors];
				report.durationMs = Date.now() - started;
				appendReport(pi, report);

				if (output.errors.length) {
					await callApm(
						safeCtx,
						[
							"diag",
							"--code",
							"side_run_failed",
							"--message",
							truncateStderr(output.errors.join("; ")),
							"--idempotency-key",
							`sr:${sessionId}:${run.id}:project:diag`,
							"--json",
						],
						{ timeoutMs: 15_000, signal },
					).catch(() => undefined);
					if (ctx.hasUI) {
						ctx.ui.notify(
							`APM side-run finished with ${output.errors.length} problem(s); run /apm-doctor`,
							"warning",
						);
					}
				}
			} catch (error) {
				report.errors.push(error instanceof Error ? error.message : String(error));
				report.durationMs = Date.now() - started;
				if (signal.aborted) {
					// Aborting kills in-flight work, which can surface as an exception; it is
					// still a cancellation, not a plugin failure.
					report.mode = "skip";
					report.reason = "session shutdown: side-run cancelled";
					report.aborted = true;
					report.errors = [];
					appendReport(pi, report);
					return;
				}
				appendReport(pi, report);
				if (ctx.hasUI) {
					ctx.ui.notify(
						`APM side-run unavailable: ${error instanceof Error ? error.message : String(error)}`,
						"warning",
					);
				}
			}
			} finally {
			if (sideRunController === controller) sideRunController = null;
			sideRunInFlight = false;
			restoreInjectedEnv();
		}
	});

	pi.on("session_shutdown", async () => {
		activeRun = null;
		// Quitting must not wait for a slow or hung side-run, and the child must not
		// outlive the session: abort reaches the process group and every apm call.
		sideRunController?.abort();
		restoreInjectedEnv();
		if (pathShim) {
			process.env.PATH = pathShim.originalPath;
			try {
				fs.rmSync(pathShim.dir, { recursive: true, force: true });
			} catch {
				// best effort
			}
			pathShim = null;
		}
	});

	// -----------------------------------------------------------------------
	// Tools
	// -----------------------------------------------------------------------

	pi.registerTool({
		name: "apm_dashboard",
		label: "APM Dashboard",
		description:
			"Show the AgentPM status board: waiting for a human, in progress, no substantive progress, lost leases, blocked dependencies, recently done. Use at the start of work or when the user asks what is going on.",
		promptSnippet: "Read the agent work ledger status board (apm_dashboard)",
		parameters: Type.Object({}),
		async execute(_toolCallId, _params, signal, _onUpdate, ctx) {
			const result = await callApm(withBin(ctx), ["dashboard", "--json", ...metaArgs(ctx)], { signal });
			if (!result.ok) return apmFailure(result);
			const data = result.data as {
				buckets?: Record<string, Array<Record<string, unknown>>>;
			};
			const buckets = data.buckets ?? {};
			const lines: string[] = [];
			const order: Array<[string, string]> = [
				["waiting_human", "WAITING FOR HUMAN"],
				["lost_lease", "LOST LEASE / AT RISK"],
				["recovery_failed", "RECOVERY FAILED"],
				["stalled_no_progress", "NO SUBSTANTIVE PROGRESS"],
				["waiting_dependencies", "WAITING ON DEPENDENCIES"],
				["in_progress", "IN PROGRESS"],
				["recently_done", "RECENTLY DONE"],
				["ready_unclaimed", "READY / UNCLAIMED"],
			];
			for (const [key, title] of order) {
				const items = buckets[key] ?? [];
				lines.push(`${title} (${items.length})`);
				for (const item of items.slice(0, 12)) {
					lines.push(
						`  ${item.identifier} ${item.status} ${item.health} ${item.title}` +
							(item.progress_summary ? `\n      progress: ${item.progress_summary}` : "") +
							(item.next_action ? `\n      next: ${item.next_action}` : ""),
					);
				}
			}
			return { content: [jsonText(lines.join("\n"))], details: data };
		},
	});

	pi.registerTool({
		name: "apm_claim",
		label: "APM Claim",
		description:
			"Claim a work item before doing any work on it. Every apm write (progress, status, context, evidence) requires this claim. Fails if another agent holds a live claim.",
		promptSnippet: "Claim an APM work item before writing to it",
		promptGuidelines: [
			"Call apm_claim before apm_progress/apm_status/apm_context on a work item, then release it when the work is handed off or finished.",
		],
		parameters: Type.Object({
			item: Type.String({ description: "Work item identifier (e.g. APM-3), uuid, or number" }),
			lease_seconds: Type.Optional(Type.Number({ description: "Lease length in seconds (default 900)" })),
			takeover: Type.Optional(
				Type.Boolean({ description: "Recover an expired lease (or your own session's lease)" }),
			),
			reason: Type.Optional(Type.String({ description: "Why you are taking over" })),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const args: string[] = ["claim", params.item, "--json"];
			if (params.lease_seconds) args.push("--lease", String(params.lease_seconds));
			if (params.takeover) args.push("--takeover");
			if (params.reason) args.push("--reason", params.reason);
			args.push(...metaArgs(ctx));
			const result = await callApm(withBin(ctx), args, { signal });
			if (!result.ok) return apmFailure(result);
			const data = result.data as { claim?: Record<string, unknown>; takeover?: boolean };
			const claim = { ...(data.claim ?? {}) };
			delete claim.token;
			const text = [
				`claimed ${claim.identifier} (lease ${claim.lease_seconds}s, expires ${claim.expires_at})`,
				data.takeover ? "recovered an expired/own lease" : "fresh claim",
				`version ${claim_bracket(claim)}`,
			].join("\n");
			return { content: [jsonText(text)], details: { claim, takeover: data.takeover } };
		},
	});

	pi.registerTool({
		name: "apm_release",
		label: "APM Release",
		description: "Release the claim on a work item when you stop working on it.",
		promptSnippet: "Release an APM claim",
		parameters: Type.Object({
			item: Type.String(),
			reason: Type.Optional(Type.String()),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const args: string[] = ["release", params.item, "--json"];
			if (params.reason) args.push("--reason", params.reason);
			args.push(...metaArgs(ctx));
			const result = await callApm(withBin(ctx), args, { signal });
			if (!result.ok) return apmFailure(result);
			return { content: [jsonText(result.data)], details: result.data };
		},
	});

	pi.registerTool({
		name: "apm_progress",
		label: "APM Progress",
		description:
			"Record substantive progress on a claimed work item, plus next action / blocker / waiting-on-human. Only call this when something actually changed; use apm_sync for liveness only.",
		promptSnippet: "Record real progress on a claimed APM work item",
		parameters: Type.Object({
			item: Type.String(),
			summary: Type.String({ description: "What actually changed, concretely" }),
			next_action: Type.Optional(Type.String()),
			blocker: Type.Optional(Type.String()),
			waiting_on_human: Type.Optional(Type.Boolean()),
			waiting_reason: Type.Optional(Type.String()),
			expect_version: Type.Optional(
				Type.Number({ description: "Optimistic-lock version; defaults to a fresh read" }),
			),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const safeCtx = withBin(ctx);
			const version = params.expect_version ?? (await currentVersion(safeCtx, params.item, signal));
			const args: string[] = [
				"progress",
				params.item,
				"--summary",
				params.summary,
				"--expect-version",
				String(version),
				"--json",
			];
			if (params.next_action) args.push("--next-action", params.next_action);
			if (params.blocker) args.push("--blocker", params.blocker);
			if (params.waiting_on_human) args.push("--waiting-human");
			if (params.waiting_reason) args.push("--waiting-reason", params.waiting_reason);
			args.push(...metaArgs(ctx));
			const result = await callApm(safeCtx, args, { signal });
			if (!result.ok) return apmFailure(result);
			const data = result.data as {
				item?: Record<string, unknown>;
				no_op?: boolean;
				substantive?: boolean;
				changed_fields?: string[];
			};
			const item = data.item ?? {};
			let text: string;
			if (data.no_op) {
				text =
					`${item.identifier}: no substantive change — the request matches the current state, ` +
					`nothing was written (version stays ${item.version}). ` +
					`Use apm_sync if you only need to record liveness.`;
			} else if (data.substantive === false) {
				text =
					`${item.identifier}: bookkeeping updated (${(data.changed_fields ?? []).join(", ") || "fields"}) — ` +
					`last_progress_at unchanged (version ${item.version}). Repeat the same summary only if ` +
					`nothing new happened, and prefer apm_sync then.`;
			} else {
				text = `${item.identifier} progress recorded (version ${item.version})\n${params.summary}`;
			}
			return { content: [jsonText(text)], details: data };
		},
	});

	pi.registerTool({
		name: "apm_sync",
		label: "APM Run Sync",
		description:
			"Record that a run happened (liveness + lease renewal) WITHOUT claiming progress. Use when a run produced no verifiable change.",
		promptSnippet: "Record an APM run sync without recording progress",
		parameters: Type.Object({
			item: Type.String(),
			outcome: Type.Optional(StringEnum(["completed", "aborted", "unknown"] as const)),
			note: Type.Optional(Type.String()),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const args: string[] = [
				"sync",
				params.item,
				"--outcome",
				String(params.outcome ?? "completed"),
				"--json",
			];
			if (params.note) args.push("--note", params.note);
			args.push(...metaArgs(ctx));
			const result = await callApm(withBin(ctx), args, { signal });
			if (!result.ok) return apmFailure(result);
			const data = result.data as { identifier?: string; run?: { outcome?: string } };
			return {
				content: [
					jsonText(
						`${data.identifier} run sync (${data.run?.outcome ?? "completed"}); last_progress_at unchanged`,
					),
				],
				details: data,
			};
		},
	});

	pi.registerTool({
		name: "apm_status",
		label: "APM Status",
		description:
			"Move a claimed work item through its workflow. The legal target states belong to the item's workflow, not to this tool: the default workflow offers ready/doing/blocked/review/done/cancelled, and a project may bind a custom workflow with extra states (e.g. qa) to an item type. Done requires every acceptance criterion to be met or waived (or, when the item defines none, verification evidence) plus a progress summary or completion note.",
		promptSnippet: "Change the status of a claimed APM work item",
		parameters: Type.Object({
			item: Type.String(),
			to: Type.String({
				description:
					"Target state key from the item's workflow (default workflow: ready/doing/blocked/review/done/cancelled; custom workflows may add states). An illegal key or transition fails with workflow_violation.",
			}),
			reason: Type.Optional(Type.String({ description: "Required for blocked/reopen/cancel" })),
			note: Type.Optional(Type.String({ description: "Completion note or context" })),
			expect_version: Type.Optional(
				Type.Number({ description: "Optimistic-lock version; defaults to a fresh read" }),
			),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const safeCtx = withBin(ctx);
			const version = params.expect_version ?? (await currentVersion(safeCtx, params.item, signal));
			const args: string[] = [
				"status",
				params.item,
				"--to",
				String(params.to),
				"--expect-version",
				String(version),
				"--json",
			];
			if (params.reason) args.push("--reason", params.reason);
			if (params.note) args.push("--note", params.note);
			args.push(...metaArgs(ctx));
			const result = await callApm(safeCtx, args, { signal });
			if (!result.ok) return apmFailure(result);
			const data = result.data as { item?: Record<string, unknown> };
			const item = data.item ?? {};
			return {
				content: [jsonText(`${item.identifier} → ${item.status} (version ${item.version})`)],
				details: data,
			};
		},
	});

	pi.registerTool({
		name: "apm_context",
		label: "APM Context",
		description:
			"Read or revise the handover context of a work item. A successor has no chat history, so goal/current_state must stand alone.",
		promptSnippet: "Read or revise APM handover context",
		parameters: Type.Object({
			item: Type.String(),
			action: StringEnum(["show", "revise"] as const),
			reason: Type.Optional(Type.String({ description: "Required for revise: what changed and why" })),
			goal: Type.Optional(Type.String()),
			current_state: Type.Optional(Type.String()),
			constraints: Type.Optional(Type.Array(Type.String())),
			references: Type.Optional(Type.Array(Type.String())),
			notes: Type.Optional(Type.String()),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const safeCtx = withBin(ctx);
			if (params.action === "show") {
				const result = await callApm(
					safeCtx,
					["context", "show", params.item, "--json", ...metaArgs(ctx)],
					{ signal },
				);
				if (!result.ok) return apmFailure(result);
				return { content: [jsonText(result.data)], details: result.data };
			}
			if (!params.reason) {
				return {
					content: [
						{
							type: "text",
							text: "revise requires `reason` (what changed and why)",
						},
					],
					details: {},
				};
			}
			const current = await callApm(
				safeCtx,
				["context", "show", params.item, "--json", ...metaArgs(ctx)],
				{ signal },
			);
			if (!current.ok) return apmFailure(current);
			const doc =
				((current.data as { context?: { doc?: Record<string, unknown> } }).context?.doc as
					| Record<string, unknown>
					| undefined) ?? {};
			const merged = {
				goal: params.goal ?? String(doc.goal ?? ""),
				current_state: params.current_state ?? String(doc.current_state ?? ""),
				constraints: params.constraints ?? ((doc.constraints as string[] | undefined) ?? []),
				references: params.references ?? ((doc.references as string[] | undefined) ?? []),
				notes: params.notes ?? ((doc.notes as string | undefined) ?? undefined),
			};
			const version = await currentVersion(safeCtx, params.item, signal);
			const result = await callApm(
				safeCtx,
				[
					"context",
					"revise",
					params.item,
					"--context-json",
					JSON.stringify(merged),
					"--reason",
					params.reason,
					"--expect-version",
					String(version),
					"--json",
					...metaArgs(ctx),
				],
				{ signal },
			);
			if (!result.ok) return apmFailure(result);
			return { content: [jsonText(result.data)], details: result.data };
		},
	});

	pi.registerTool({
		name: "apm_attention",
		label: "APM Ask Human",
		description:
			"Register a structured question for a human (question, context, options, recommendation, blocking). Use instead of burying a decision in chat.",
		promptSnippet: "Ask a human a structured question via the APM ledger",
		parameters: Type.Object({
			item: Type.Optional(Type.String()),
			question: Type.String(),
			context: Type.String({ description: "Everything a human needs to decide, without the chat log" }),
			options: Type.Optional(Type.Array(Type.String())),
			recommendation: Type.Optional(Type.String()),
			blocking: Type.Optional(Type.Boolean()),
		}),
		async execute(_toolCallId, params, signal, _onUpdate, ctx) {
			const args: string[] = ["attention", "request", "--question", params.question, "--context", params.context];
			if (params.item) args.push("--item", params.item);
			for (const option of params.options ?? []) args.push("--option", option);
			if (params.recommendation) args.push("--recommend", params.recommendation);
			if (params.blocking === false) args.push("--non-blocking");
			args.push("--json", ...metaArgs(ctx));
			const result = await callApm(withBin(ctx), args, { signal });
			if (!result.ok) return apmFailure(result);
			const data = result.data as { attention?: Record<string, unknown> };
			const attention = data.attention ?? {};
			return {
				content: [
					jsonText(
						`attention ${attention.id} registered (${attention.blocking ? "blocking" : "non-blocking"})\n` +
							`question: ${attention.question}\n` +
							`a human can answer with: apm attention reply ${attention.id} --response "..."`,
					),
				],
				details: data,
			};
		},
	});

	// -----------------------------------------------------------------------
	// Commands
	// -----------------------------------------------------------------------

	pi.registerCommand("apm-status", {
		description: "Show the AgentPM status board",
		handler: async (_args, ctx) => {
			const result = await callApm(withBin(ctx), ["dashboard", "--json", ...metaArgs(ctx)]);
			if (!result.ok) {
				ctx.ui.notify(`APM: ${result.error?.message ?? "dashboard failed"}`, "error");
				return;
			}
			const data = result.data as { buckets?: Record<string, unknown[]> };
			const buckets = data.buckets ?? {};
			const summary = Object.entries(buckets)
				.map(([key, items]) => `${key}: ${items.length}`)
				.join("  ");
			ctx.ui.notify(`APM ${summary}`, "info");
			pi.sendMessage({
				customType: "apm-dashboard",
				content: JSON.stringify(data, null, 2),
				display: true,
			});
		},
	});

	pi.registerCommand("apm-doctor", {
		description: "Check the AgentPM database, claims, and side-run diagnostics",
		handler: async (_args, ctx) => {
			const sessionId = sessionIdOf(ctx);
			const doctor = await callApm(withBin(ctx), ["doctor", "--json"]);
			if (!doctor.ok) {
				ctx.ui.notify(`APM: ${doctor.error?.message ?? "doctor failed"}`, "error");
				return;
			}
			const claims = sessionId ? await loadClaims(withBin(ctx), sessionId) : [];
			const d = doctor.data as {
				warnings?: string[];
				open_items?: number;
				active_claims?: number;
				open_attention?: number;
				pending_diagnostics?: unknown[];
			};
			const lines = [
				`open items: ${d.open_items ?? "?"}  active claims: ${d.active_claims ?? "?"}  open attention: ${d.open_attention ?? "?"}`,
				`this session holds: ${claims.length ? describeItems(claims) : "nothing"}`,
				`pending diagnostics: ${d.pending_diagnostics?.length ?? 0}`,
				...(d.warnings ?? []).map((w) => `warning: ${w}`),
			];
			ctx.ui.notify("APM doctor", "info");
			pi.sendMessage({
				customType: "apm-doctor",
				content: lines.join("\n"),
				display: true,
			});
		},
	});
}

// ---------------------------------------------------------------------------
// Tool helpers
// ---------------------------------------------------------------------------

function claim_bracket(claim: Record<string, unknown>): string {
	return String(claim.version ?? "?");
}

async function currentVersion(ctx: ExtensionContext, item: string, signal?: AbortSignal): Promise<number> {
	const result = await callApm(ctx, ["get", item, "--json"], { signal });
	if (!result.ok) {
		throw new ApmBinaryError(
			`cannot read ${item}: ${result.error?.code} ${result.error?.message}`,
		);
	}
	const version = Number((result.data as { item?: { version?: number } }).item?.version);
	return Number.isFinite(version) ? version : 1;
}

/**
 * Turn an apm failure into model-readable guidance instead of a hard error:
 * claim/version/workflow failures are all recoverable by the model.
 */
function apmFailure(result: ApmResult): {
	content: Array<{ type: "text"; text: string }>;
	details: unknown;
	isError?: boolean;
} {
	const error = result.error;
	const hints: Record<string, string> = {
		claim_required: "claim the item first with apm_claim",
		claim_invalid:
			"the lease is expired or the token is stale: call apm_claim with takeover=true, or stop and report",
		claim_conflict: "another agent holds a live claim: do not write; report who holds it",
		version_conflict: "re-read the item (apm_context show / a fresh apm_progress call) and retry once",
		workflow_violation: "read the allowed transitions with `apm workflow`, then use a legal status",
		completion_evidence_required:
			"mark every acceptance criterion met/waived (apm accept set) or, if the item has none, add verification evidence before done",
		not_found: "check the identifier with apm_dashboard",
	};
	const code = error?.code ?? "apm_error";
	const payload = {
		ok: false,
		code,
		message: error?.message ?? "apm call failed",
		details: error?.details ?? {},
		hint: hints[code] ?? "inspect with `apm doctor`",
	};
	return { content: [jsonText(payload)], details: { error: payload } };
}
