/**
 * Pure logic for the pi-apm extension: no pi imports, so it can be unit tested
 * with vitest and reasoned about without the harness.
 *
 * The split mirrors the Rust design: this file decides *whether* and *what* to
 * synchronise, `index.ts` only does IO (spawning `apm`, spawning the side-run
 * child, rendering diagnostics).
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

export const SIDE_RUN_ENV = "APM_SIDE_RUN";
export const SIDE_RUN_DISABLE_ENV = "APM_SIDE_RUN_DISABLED";

// ---------------------------------------------------------------------------
// Project discovery
// ---------------------------------------------------------------------------

/**
 * Walk up from `cwd` looking for `.apm/apm.db`. Returns the project root (the
 * directory that contains `.apm`), or null when this is not an APM project.
 */
export function findProjectDir(cwd: string, maxDepth = 12): string | null {
	let dir = path.resolve(cwd);
	for (let i = 0; i <= maxDepth; i++) {
		const candidate = path.join(dir, ".apm", "apm.db");
		if (fs.existsSync(candidate)) return dir;
		const parent = path.dirname(dir);
		if (parent === dir) break;
		dir = parent;
	}
	return null;
}

// ---------------------------------------------------------------------------
// Idempotency keys
// ---------------------------------------------------------------------------

export type SideRunOp =
	| "sync"
	| "progress"
	| "context"
	| "status"
	| "evidence"
	| "attention"
	| "diag"
	| "reconcile";

/**
 * Per-(session, run, item, operation) idempotency key.
 *
 * A single run touches several items and performs several operations per item,
 * so `run:session:runId` alone is not enough: it would silently collapse the
 * second item's sync into the first item's replay.
 */
export function runKey(input: {
	sessionId: string;
	runId: string;
	itemId: string;
	op: SideRunOp;
}): string {
	const clean = (v: string) => v.replace(/[^A-Za-z0-9._-]/g, "_");
	return `sr:${clean(input.sessionId)}:${clean(input.runId)}:${clean(input.itemId)}:${input.op}`;
}

// ---------------------------------------------------------------------------
// Claim listing (shape of `apm claims --json`)
// ---------------------------------------------------------------------------

export interface ClaimSnapshot {
	itemId: string;
	identifier: string;
	title: string;
	itemStatus: string;
	version: number;
	contextVersion: number;
	progressSummary: string | null;
	nextAction: string | null;
	lastProgressAt: string | null;
	claimId: string;
	claimStatus: string;
	claimExpired: boolean;
	expiresAt: string;
	holder: string;
	sessionId: string | null;
}

interface RawClaimListing {
	ok?: boolean;
	session?: string | null;
	claims?: Array<{
		item?: Record<string, unknown>;
		claim?: Record<string, unknown>;
	}>;
}

function str(value: unknown): string | null {
	return typeof value === "string" ? value : null;
}

function num(value: unknown): number {
	return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

/**
 * Normalise `apm claims --with-tokens --json`. Tolerates extra fields and
 * missing tokens (older CLI) instead of throwing: a malformed listing must
 * degrade to "no claims", never crash the hook.
 */
export function parseClaimListing(json: unknown): ClaimSnapshot[] {
	const raw = json as RawClaimListing | null;
	if (!raw || !Array.isArray(raw.claims)) return [];
	const out: ClaimSnapshot[] = [];
	for (const entry of raw.claims) {
		const item = entry.item as Record<string, unknown> | undefined;
		const claim = entry.claim as Record<string, unknown> | undefined;
		if (!item || !claim) continue;
		const itemId = str(item.id);
		const identifier = str(item.identifier);
		if (!itemId || !identifier) continue;
		out.push({
			itemId,
			identifier,
			title: str(item.title) ?? "(untitled)",
			itemStatus: str(item.status) ?? "unknown",
			version: num(item.version),
			contextVersion: num(item.context_version),
			progressSummary: str(item.progress_summary),
			nextAction: str(item.next_action),
			lastProgressAt: str(item.last_progress_at),
			claimId: str(claim.id) ?? "",
			claimStatus: str(claim.status) ?? "unknown",
			claimExpired: claim.expired === true,
			expiresAt: str(claim.expires_at) ?? "",
			holder: str(claim.holder) ?? "unknown",
			sessionId: str(claim.session_id),
		});
	}
	return out;
}

// ---------------------------------------------------------------------------
// Run digest
// ---------------------------------------------------------------------------

export interface ToolCallSummary {
	name: string;
	summary: string;
	isError?: boolean;
}

export interface RunDigest {
	userPrompts: string[];
	assistantTexts: string[];
	toolCalls: ToolCallSummary[];
	filesTouched: string[];
	errors: string[];
}

export function emptyDigest(): RunDigest {
	return { userPrompts: [], assistantTexts: [], toolCalls: [], filesTouched: [], errors: [] };
}

/// Claim tokens are `apm_` + 32+ hex chars. They must never reach a prompt, a
/// child argv, a ledger field, or a report: models happily echo command output.
const TOKEN_PATTERN = /apm_[0-9a-f]{32,}/g;

export function redactSecrets(text: string): string {
	return text.replace(TOKEN_PATTERN, "apm_[redacted]");
}

export function truncate(text: string, max: number): string {
	const clean = text.replace(/\s+/g, " ").trim();
	if (clean.length <= max) return clean;
	return `${clean.slice(0, Math.max(0, max - 1))}…`;
}

function pushBounded<T>(list: T[], value: T, max: number): void {
	list.push(value);
	if (list.length > max) list.splice(0, list.length - max);
}

function contentParts(message: Record<string, unknown>): Array<Record<string, unknown>> {
	const content = message.content;
	if (typeof content === "string") return [{ type: "text", text: content }];
	if (Array.isArray(content)) return content as Array<Record<string, unknown>>;
	return [];
}

/**
 * Fold new messages into a digest. Bounded on purpose: the side-run prompt is
 * size-sensitive and a 300-turn run must not be pasted into a child process.
 */
export function digestRun(messages: unknown[], previous: RunDigest = emptyDigest()): RunDigest {
	const digest: RunDigest = {
		userPrompts: [...previous.userPrompts],
		assistantTexts: [...previous.assistantTexts],
		toolCalls: [...previous.toolCalls],
		filesTouched: [...previous.filesTouched],
		errors: [...previous.errors],
	};
	for (const raw of messages) {
		if (!raw || typeof raw !== "object") continue;
		const message = raw as Record<string, unknown>;
		const role = message.role;
		if (role === "user") {
			const text = contentParts(message)
				.filter((p) => p.type === "text")
				.map((p) => String(p.text ?? ""))
				.join("\n");
			if (text.trim()) pushBounded(digest.userPrompts, truncate(redactSecrets(text), 900), 6);
			continue;
		}
		if (role === "assistant") {
			for (const part of contentParts(message)) {
				if (part.type === "text") {
					const text = String(part.text ?? "");
					if (text.trim())
						pushBounded(digest.assistantTexts, truncate(redactSecrets(text), 1800), 6);
				} else if (part.type === "toolCall") {
					const name = String(part.name ?? "tool");
					const args = (part.arguments ?? {}) as Record<string, unknown>;
					const filePath = str(args.file_path) ?? str(args.path);
					if (filePath) pushBounded(digest.filesTouched, filePath, 24);
					pushBounded(
						digest.toolCalls,
						{ name, summary: redactSecrets(summariseToolArgs(name, args)) },
						24,
					);
				}
			}
			continue;
		}
		if (role === "toolResult") {
			if (message.isError === true) {
				const text = contentParts(message)
					.filter((p) => p.type === "text")
					.map((p) => String(p.text ?? ""))
					.join("\n");
				pushBounded(
					digest.errors,
					truncate(redactSecrets(`${String(message.toolName ?? "tool")}: ${text}`), 400),
					6,
				);
			}
		}
	}
	return digest;
}

function summariseToolArgs(name: string, args: Record<string, unknown>): string {
	switch (name) {
		case "bash":
			return truncate(String(args.command ?? ""), 220);
		case "read":
		case "write":
		case "edit":
			return truncate(String(args.file_path ?? args.path ?? ""), 220);
		case "grep":
		case "find":
			return truncate(`${String(args.pattern ?? "")} in ${String(args.path ?? ".")}`, 220);
		default: {
			try {
				return truncate(JSON.stringify(args), 220);
			} catch {
				return "";
			}
		}
	}
}

// ---------------------------------------------------------------------------
// Decision + plans
// ---------------------------------------------------------------------------

export type RunMode = "side-run" | "abort-sync" | "skip";

export interface RunDecision {
	mode: RunMode;
	reason: string;
}

/**
 * Cheap pre-checks that need no database access. Returns a skip reason or null.
 * Kept separate from `decideRunMode` because the "no claims" check must happen
 * *after* loading claims, while these checks must happen before any IO.
 */
export function preflightSkip(input: {
	disabled: boolean;
	env?: Record<string, string | undefined>;
	projectDir: string | null;
	hasSession: boolean;
}): string | null {
	const env = input.env ?? {};
	if (input.disabled) return "extension disabled";
	if (env[SIDE_RUN_DISABLE_ENV] === "1") return "APM_SIDE_RUN_DISABLED=1";
	if (env[SIDE_RUN_ENV] === "1") return "recursion guard: already inside a side-run";
	if (!input.hasSession) return "no session id";
	if (!input.projectDir) return "no .apm/apm.db found from cwd";
	return null;
}

/**
 * Decide what the run-end hook should do.
 *
 * - a child side-run must never trigger another side-run (`APM_SIDE_RUN`);
 * - no project DB or no held claim means there is nothing to synchronise, and
 *   the side-run must not go looking for new work;
 * - an aborted run gets a deterministic, evidence-free abort sync (no LLM): it
 *   must not speculate about progress.
 */
export function decideRunMode(input: {
	disabled: boolean;
	env?: Record<string, string | undefined>;
	aborted: boolean;
	projectDir: string | null;
	claims: ClaimSnapshot[];
	hasSession: boolean;
}): RunDecision {
	const skip = preflightSkip(input);
	if (skip) return { mode: "skip", reason: skip };
	if (input.claims.length === 0) {
		return { mode: "skip", reason: "session holds no claim (side-run never claims new work)" };
	}
	if (input.aborted) return { mode: "abort-sync", reason: "run aborted: liveness only" };
	return { mode: "side-run", reason: "run completed" };
}

export type PlannedCall =
	| { kind: "sync-aborted"; itemId: string; identifier: string; args: string[]; key: string }
	| { kind: "diagnostic"; itemId: string; identifier: string; args: string[]; key: string; code: string };

/**
 * Deterministic plan for an aborted run.
 *
 * Expired leases cannot be written to (by design), so those items get a
 * diagnostic instead of a fabricated sync. Nothing here touches progress,
 * status, or releases a claim.
 */
export function planAbortSync(input: {
	sessionId: string;
	runId: string;
	claims: ClaimSnapshot[];
	reason: string;
}): PlannedCall[] {
	const plan: PlannedCall[] = [];
	for (const claim of input.claims) {
		const key = runKey({
			sessionId: input.sessionId,
			runId: input.runId,
			itemId: claim.itemId,
			op: "sync",
		});
		if (claim.claimExpired || claim.claimStatus !== "active") {
			const diagKey = runKey({
				sessionId: input.sessionId,
				runId: input.runId,
				itemId: claim.itemId,
				op: "diag",
			});
			plan.push({
				kind: "diagnostic",
				itemId: claim.itemId,
				identifier: claim.identifier,
				code: "claim_expired_after_abort",
				key: diagKey,
				args: [
					"diag",
					"--item",
					claim.identifier,
					"--code",
					"claim_expired_after_abort",
					"--message",
					`run ${input.runId} was aborted after the lease expired (${claim.expiresAt}); progress was not recorded`,
					"--idempotency-key",
					diagKey,
					"--json",
				],
			});
			continue;
		}
		plan.push({
			kind: "sync-aborted",
			itemId: claim.itemId,
			identifier: claim.identifier,
			key,
			args: [
				"sync",
				claim.identifier,
				"--outcome",
				"aborted",
				"--note",
				truncate(input.reason, 300),
				"--idempotency-key",
				key,
				"--json",
			],
		});
	}
	return plan;
}

// ---------------------------------------------------------------------------
// Reconciliation
// ---------------------------------------------------------------------------

export interface RunCoverageItem {
	itemId: string;
	identifier: string;
	/// Whether the item is *still* held by this session after the side-run.
	claimActive: boolean;
}

export interface ReconStep {
	itemId: string;
	identifier: string;
	action: "skip" | "sync" | "diagnostic";
	reason: string;
	key: string;
	/// CLI args without session/run/agent metadata (the caller appends those).
	args: string[];
}

/**
 * Which items already have evidence from *this* run.
 *
 * Evidence means an event written by this run: `progress_*` or `run_*` with our
 * `run_id`. A context-only edit is not liveness, and events from a previous run
 * (or from the setup work) must not count.
 */
export function coveredItemIds(events: unknown[], runId: string): Set<string> {
	const covered = new Set<string>();
	if (!runId) return covered;
	for (const raw of events) {
		if (!raw || typeof raw !== "object") continue;
		const event = raw as Record<string, unknown>;
		if (event.run_id !== runId) continue;
		const kind = String(event.kind ?? "");
		if (!kind.startsWith("progress_") && !kind.startsWith("run_")) continue;
		const itemId = event.item_id;
		if (typeof itemId === "string" && itemId) covered.add(itemId);
	}
	return covered;
}

/**
 * Deterministic guarantee: after the LLM side-run (or after it failed), every
 * item this session claimed must carry at least one event for this run.
 *
 * Missing items get a baseline `apm sync` (liveness only — never progress), with
 * a stable per-(session, run, item) key so retries replay instead of duplicating.
 * Items whose lease is gone get a diagnostic instead, because writing would
 * violate the claim rule.
 */
export function planReconciliation(input: {
	items: RunCoverageItem[];
	covered: Set<string>;
	runId: string;
	sessionId: string;
	aborted: boolean;
	childFailed: boolean;
}): ReconStep[] {
	const steps: ReconStep[] = [];
	const note = input.childFailed
		? "baseline sync: run-end side-run failed or was unavailable"
		: "baseline sync: side-run produced no ledger update for this run";
	for (const item of input.items) {
		if (input.covered.has(item.itemId)) {
			steps.push({
				itemId: item.itemId,
				identifier: item.identifier,
				action: "skip",
				reason: "this run already recorded progress/run sync",
				key: "",
				args: [],
			});
			continue;
		}
		if (!item.claimActive) {
			const key = `${runKey({
				sessionId: input.sessionId,
				runId: input.runId,
				itemId: item.itemId,
				op: "reconcile",
			})}:diag`;
			steps.push({
				itemId: item.itemId,
				identifier: item.identifier,
				action: "diagnostic",
				reason: "lease expired before the run could be recorded",
				key,
				args: [
					"diag",
					"--item",
					item.identifier,
					"--code",
					"claim_expired_after_side_run",
					"--message",
					truncate(
						`run ${input.runId} ended but the lease was gone; no sync/progress was recorded`,
						300,
					),
					"--idempotency-key",
					key,
					"--json",
				],
			});
			continue;
		}
		const key = `${runKey({
			sessionId: input.sessionId,
			runId: input.runId,
			itemId: item.itemId,
			op: "reconcile",
		})}:sync`;
		steps.push({
			itemId: item.itemId,
			identifier: item.identifier,
			action: "sync",
			reason: note,
			key,
			args: [
				"sync",
				item.identifier,
				"--outcome",
				input.aborted ? "aborted" : "completed",
				"--note",
				note,
				"--idempotency-key",
				key,
				"--json",
			],
		});
	}
	return steps;
}

// ---------------------------------------------------------------------------
// Prompt construction
// ---------------------------------------------------------------------------

export function buildSideRunSystemPrompt(): string {
	return [
		"You are the AgentPM run-end synchroniser: a short-lived background process started by the",
		"pi-apm extension after the main agent finished a run. You are NOT the main agent and the",
		"human is not talking to you.",
		"",
		"Your only job is to persist what this run actually changed into the local AgentPM ledger",
		"with the `apm` command line, then stop.",
		"",
		"Hard rules (violations are worse than doing nothing):",
		"1. Only touch the work items listed in the task. Never create items. Never claim an item",
		"   that is not listed. Never release a claim.",
		"2. Always pass `--session <session>`; the CLI reuses this session's claim token from its",
		"   0600 session file, so you never see or type a token. If a lease has expired you may",
		"   recover that same item at most once:",
		"     apm claim <identifier> --session <session> --recover-only --takeover \\",
		"       --reason \"run-end recovery after lease expiry\" --json",
		"   (the CLI stores the new token in the session file for you.) If that fails, record the",
		"   failure and stop touching that item:",
		"     apm diag --item <identifier> --code claim_recovery_failed --message \"<why>\" \\",
		"       --idempotency-key <key> --json",
		"3. Progress requires a verifiable change from this run (files edited, tests/commands run,",
		"   a decision made). Never invent percentages or restate the request. Repeating the existing",
		"   summary is rejected as a no-op and must never be used to look busy. If nothing substantive",
		"   happened, write NO progress: call `apm sync` once per listed item so liveness is recorded.",
		"4. Never move an item to done unless the run contains verification evidence. Add the evidence",
		"   with `apm evidence add` before `apm status --to done`. Never close an item because the",
		"   conversation ended.",
		"5. Revise context only when the handover document is now wrong or incomplete; the reason is",
		"   mandatory and must name what changed. Remember a successor has no chat history.",
		"6. If the run needs a human decision, use `apm attention request` with a self-contained",
		"   question and context.",
		"7. Always pass the exact `--idempotency-key` listed for the item and operation, and",
		"   `--expect-version` with the version you last read. Re-read with `apm get <id> --json`",
		"   after every successful write (the version increments).",
		"8. Keep it short: at most about 10 CLI calls. If `apm` is not on PATH, call `$APM_BIN`",
		"   instead (it always points at the CLI).",
		"9. Never paste `apm --json` output into the ledger. It can contain a claim token, and",
		"   progress/context fields are human- and model-facing. Summarise in your own words.",
	].join("\n");
}

export interface SideRunTask {
	sessionId: string;
	runId: string;
	projectDir: string;
	dbPath: string;
	claims: ClaimSnapshot[];
	digest: RunDigest;
	startedAt: string;
	finishedAt: string;
}

export function buildSideRunTaskPrompt(task: SideRunTask): string {
	const lines: string[] = [];
	lines.push(`Session: ${task.sessionId}`);
	lines.push(`Run: ${task.runId}`);
	lines.push(`Project dir: ${task.projectDir}`);
	lines.push(`Database: ${task.dbPath}`);
	lines.push(
		"You are running as this session: the CLI resolves your claim token from the " +
			"session file when you pass `--session`. Never print or copy a token.",
	);
	lines.push(`Run window: ${task.startedAt} .. ${task.finishedAt}`);
	lines.push("");
	lines.push("## Work items this session holds a claim on");
	for (const claim of task.claims) {
		lines.push("");
		lines.push(`### ${claim.identifier} (${claim.title})`);
		lines.push(`item_id: ${claim.itemId}`);
		lines.push(`status: ${claim.itemStatus}   item version: ${claim.version}   context version: ${claim.contextVersion}`);
		lines.push(`claim: ${claim.claimId} holder=${claim.holder} expires=${claim.expiresAt} expired=${claim.claimExpired}`);
		lines.push(`progress_summary: ${claim.progressSummary ?? "(none)"}`);
		lines.push(`next_action: ${claim.nextAction ?? "(none)"}`);
		lines.push(`last_progress_at: ${claim.lastProgressAt ?? "(never)"}`);
		lines.push("idempotency keys (use exactly these):");
		for (const op of ["sync", "progress", "context", "status", "evidence", "attention", "diag"] as const) {
			lines.push(
				`  ${op}: ${runKey({ sessionId: task.sessionId, runId: task.runId, itemId: claim.itemId, op })}`,
			);
		}
	}
	lines.push("");
	lines.push("## What happened in this run");
	const digest = task.digest;
	lines.push("");
	lines.push("User requests:");
	if (digest.userPrompts.length === 0) lines.push("  (none captured)");
	for (const prompt of digest.userPrompts) lines.push(`  - ${prompt}`);
	lines.push("");
	lines.push("Assistant output (most recent last):");
	if (digest.assistantTexts.length === 0) lines.push("  (none captured)");
	for (const text of digest.assistantTexts) lines.push(`  - ${text}`);
	lines.push("");
	lines.push("Tool calls:");
	if (digest.toolCalls.length === 0) lines.push("  (none)");
	for (const call of digest.toolCalls) lines.push(`  - ${call.name}: ${call.summary}`);
	lines.push("");
	lines.push(`Files touched: ${digest.filesTouched.length ? digest.filesTouched.join(", ") : "(none)"}`);
	if (digest.errors.length) {
		lines.push("");
		lines.push("Tool errors:");
		for (const error of digest.errors) lines.push(`  - ${error}`);
	}
	lines.push("");
	lines.push("Now synchronise the ledger. If the run produced nothing verifiable, only call `apm sync`.");
	// Last line of defence: even if a digest field somehow contains a token, the
	// child (and its argv) never sees it. Keep the prompt's own text intact.
	return redactSecrets(lines.join("\n"));
}

// ---------------------------------------------------------------------------
// Child process output
// ---------------------------------------------------------------------------

export interface SideRunOutput {
	text: string;
	errors: string[];
	events: number;
}

/**
 * Parse the child's JSONL stream. Tolerant by design: the hook must produce a
 * diagnostic even when the child prints garbage.
 */
export function summarizeSideRunOutput(stdout: string): SideRunOutput {
	const errors: string[] = [];
	let text = "";
	let events = 0;
	for (const line of stdout.split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		let event: Record<string, unknown>;
		try {
			event = JSON.parse(trimmed) as Record<string, unknown>;
		} catch {
			continue;
		}
		events++;
		if (event.type === "error") {
			errors.push(truncate(JSON.stringify(event), 300));
			continue;
		}
		if (event.type !== "message_end") continue;
		const message = event.message as Record<string, unknown> | undefined;
		if (!message || message.role !== "assistant") continue;
		const parts = contentParts(message);
		const chunk = parts
			.filter((p) => p.type === "text")
			.map((p) => String(p.text ?? ""))
			.join("\n")
			.trim();
		if (chunk) text = chunk;
	}
	return { text: redactSecrets(text), errors: errors.map(redactSecrets), events };
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

export function nowIso(): string {
	return new Date().toISOString();
}

export function describeItems(claims: ClaimSnapshot[], limit = 6): string {
	const shown = claims.slice(0, limit).map((c) => `${c.identifier}(${c.itemStatus})`);
	const extra = claims.length > limit ? ` +${claims.length - limit} more` : "";
	return `${shown.join(", ")}${extra}`;
}

/** Session token file name used by the CLI (documented for diagnostics). */
export function sessionTokenFilePath(projectDir: string, _sessionId: string): string {
	return path.join(projectDir, ".apm", "sessions");
}

export function platformTempDir(): string {
	return os.tmpdir();
}

// ---------------------------------------------------------------------------
// Run-sync report formatting (TUI)
// ---------------------------------------------------------------------------

export type Tone = "plain" | "muted" | "accent" | "success" | "warning" | "error";

export interface StyledLine {
	text: string;
	tone: Tone;
}

export interface ReconciledEntry {
	identifier: string;
	action: string;
	ok: boolean;
	code?: string | null;
}

/// Shape of the `apm-run-sync` session entry written by the settle hook.
export interface RunSyncReportData {
	ran: boolean;
	mode: "side-run" | "abort-sync" | "skip";
	reason: string;
	runId?: string | null;
	sessionId?: string | null;
	items?: string[];
	calls?: number;
	childEvents?: number;
	reconciled?: ReconciledEntry[];
	output?: string;
	errors?: string[];
	durationMs?: number;
	at?: string;
}

function shortId(value: string | null | undefined): string {
	if (!value) return "-";
	return value.length > 18 ? `${value.slice(0, 18)}…` : value;
}

function duration(ms: number | undefined): string {
	if (!ms || ms < 0) return "";
	return ms >= 1000 ? `${(ms / 1000).toFixed(1)}s` : `${ms}ms`;
}

function compactItems(items: string[] | undefined, limit = 6): string {
	const list = items ?? [];
	if (list.length === 0) return "";
	const shown = list.slice(0, limit).join(", ");
	return list.length > limit ? `${shown} +${list.length - limit} more` : shown;
}

function outputLines(output: string | undefined, max: number, width: number): string[] {
	if (!output) return [];
	return output
		.split("\n")
		.map((line) => redactSecrets(line).trim())
		.filter(Boolean)
		.slice(0, max)
		.map((line) => truncate(line, width));
}

/**
 * Turn a run-sync report into display lines with semantic tones.
 *
 * Pure on purpose: the TUI renderer only applies theme colours, so this can be
 * tested without a terminal. Output and errors are redacted and truncated;
 * nothing here changes what the side-run wrote to the ledger.
 */
export function formatRunSyncReport(
	report: Partial<RunSyncReportData>,
	options: { expanded?: boolean } = {},
): StyledLine[] {
	if (!report.mode && report.ran === undefined) {
		return [{ text: "[apm] no run-sync data", tone: "muted" }];
	}
	const expanded = options.expanded === true;
	const errors = (report.errors ?? []).map((e) => redactSecrets(e));
	const items = report.items ?? [];
	const reconciled = report.reconciled ?? [];
	const lines: StyledLine[] = [];

	const label = (() => {
		if (report.mode === "skip") return "skipped";
		if (report.mode === "abort-sync") return "aborted run · liveness only";
		// A run that did not sync must never claim it did, even when the reason is
		// an error (a timed-out side-run used to render as "run synced with 1
		// issue(s)", which reads like success).
		if (report.ran === false) {
			return errors.length > 0 ? `run not synced with ${errors.length} issue(s)` : "run not synced";
		}
		if ((report.errors?.length ?? 0) > 0) {
			return `run synced with ${errors.length} issue(s)`;
		}
		return "run synced";
	})();
	const tone: Tone =
		report.mode === "skip"
			? "muted"
			: report.mode === "abort-sync"
				? "warning"
				: errors.length > 0 || report.ran === false
					? "warning"
					: "success";
	const meta = [duration(report.durationMs), items.length > 0 ? `${items.length} item(s)` : ""]
		.filter(Boolean)
		.join(" · ");
	lines.push({
		text: `[apm] ${label}${meta ? ` (${meta})` : ""}${report.runId ? ` · run ${shortId(report.runId)}` : ""}`,
		tone,
	});

	if (report.mode === "skip" || report.mode === "abort-sync") {
		if (report.reason) lines.push({ text: `  ${truncate(report.reason, 180)}`, tone: "muted" });
	}

	const itemText = compactItems(items);
	if (itemText && (expanded || report.mode !== "skip")) {
		lines.push({ text: `  items: ${itemText}`, tone: "muted" });
	}

	if (reconciled.length > 0 && report.mode !== "skip") {
		const shown = reconciled.slice(0, expanded ? 12 : 4);
		for (const entry of shown) {
			const icon = entry.ok ? (entry.action === "skip" ? "·" : "✓") : "✗";
			const suffix = entry.ok
				? entry.action === "skip"
					? "already recorded"
					: entry.action
				: `failed${entry.code ? ` (${entry.code})` : ""}`;
			lines.push({
				text: truncate(`  ${icon} ${entry.identifier} ${suffix}`, 160),
				tone: entry.ok ? (entry.action === "skip" ? "muted" : "success") : "error",
			});
		}
		if (reconciled.length > shown.length) {
			lines.push({
				text: `  +${reconciled.length - shown.length} more item(s)`,
				tone: "muted",
			});
		}
	}

	const shownErrors = errors.slice(0, expanded ? 6 : 1);
	for (const error of shownErrors) {
		lines.push({ text: truncate(`  ! ${error}`, 200), tone: "error" });
	}
	if (errors.length > shownErrors.length) {
		lines.push({ text: `  ! +${errors.length - shownErrors.length} more error(s)`, tone: "muted" });
	}

	const output = outputLines(report.output, expanded ? 6 : 1, 200);
	if (output.length > 0) {
		if (!expanded) {
			lines.push({ text: `  ${output[0]}`, tone: "muted" });
		} else {
			for (const line of output) lines.push({ text: `  ${line}`, tone: "muted" });
		}
	}

	if (expanded) {
		const metaLine = [
			report.at ? `at ${report.at}` : "",
			report.sessionId ? `session ${shortId(report.sessionId)}` : "",
			`run ${shortId(report.runId)}`,
		]
			.filter(Boolean)
			.join(" · ");
		lines.push({ text: `  ${metaLine}`, tone: "muted" });
	}

	return lines.length > 0 ? lines : [{ text: "[apm] no run-sync data", tone: "muted" }];
}
