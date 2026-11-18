#!/usr/bin/env node
// Real-browser verification for `apm ui` (APM-22).
//
// Spawns the release binary against a throwaway SQLite ledger, drives Chromium
// via playwright-core, and asserts the dashboard behaves like the docs promise:
// ticket exchange, live data, filters, detail drawer, auto/manual refresh,
// offline cache, XSS safety, no token in storage/HTML and a clean console.
//
// Coverage added for the visualisation work (APM-20/21): the topology view
// (ready vs waiting, direct/transitive blockers, unlock counts), stage bars from
// each item's *snapshot* workflow (including a custom four-state flow and its
// acceptance progress), the local dependency graph (dependency vs parent edges,
// scope switches, text equivalent) and closed-category reachability (done /
// cancelled items open the full drawer from the fold and from a deep link).
//
// Requirements: a Chromium/Chrome binary. Set APM_CHROME or CHROME_PATH, or let
// the script find one under ~/.cache/ms-playwright (Playwright's cache) or PATH.
// Without a browser it prints SKIP and exits 0, so `just verify` stays usable.
//
// Usage: node scripts/ui-browser-check.mjs [--headed]

import { execFileSync, spawn } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

const ROOT = path.resolve(new URL("..", import.meta.url).pathname);
const BIN = process.env.APM_BIN || path.join(ROOT, "target/release/apm");
const HEADED = process.argv.includes("--headed");

function log(kind, message) {
	console.log(`${kind} ${message}`);
}
function pass(message) {
	log("PASS", message);
}
function fail(message) {
	log("FAIL", message);
	throw new Error(message);
}
function assert(condition, message) {
	if (!condition) fail(message);
	pass(message);
}

function findChromium() {
	const candidates = [];
	if (process.env.APM_CHROME) candidates.push(process.env.APM_CHROME);
	if (process.env.CHROME_PATH) candidates.push(process.env.CHROME_PATH);
	const cache = path.join(os.homedir(), ".cache/ms-playwright");
	if (fs.existsSync(cache)) {
		const dirs = fs
			.readdirSync(cache)
			.filter((name) => name.startsWith("chromium-"))
			.sort()
			.reverse();
		for (const dir of dirs) {
			for (const sub of ["chrome-linux64/chrome", "chrome-linux/chrome"]) {
				candidates.push(path.join(cache, dir, sub));
			}
		}
	}
	for (const name of ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable"]) {
		try {
			const found = execFileSync("which", [name], { encoding: "utf8" }).trim();
			if (found) candidates.push(found);
		} catch {
			// not installed
		}
	}
	return candidates.find((candidate) => candidate && fs.existsSync(candidate));
}

function run(bin, args, env = {}) {
	return execFileSync(bin, args, {
		encoding: "utf8",
		env: { ...process.env, ...env, APM_SESSION_ID: "", APM_RUN_ID: "", APM_SIDE_RUN: "" },
	});
}

function apmJson(args, env) {
	const stdout = run(BIN, [...args, "--json"], env);
	const parsed = JSON.parse(stdout);
	if (parsed.ok === false) throw new Error(`apm ${args.join(" ")} failed: ${stdout}`);
	return parsed;
}

function metric(text, label) {
	const lines = text.split("\n").map((line) => line.trim());
	const index = lines.findIndex((line) => line === label);
	if (index <= 0) return null;
	const raw = lines
		.slice(0, index)
		.reverse()
		.find((line) => /^\d+$/.test(line));
	return raw === undefined ? null : Number(raw);
}

async function main() {
	if (!fs.existsSync(BIN)) fail(`missing binary ${BIN}; run \`just build\` first`);
	const executablePath = findChromium();
	if (!executablePath) {
		log("SKIP", "no Chromium/Chrome found (set APM_CHROME); browser checks skipped");
		return;
	}
	const { chromium } = await import("playwright-core");

	const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "apm-ui-browser-"));
	const db = path.join(tmp, ".apm", "apm.db");
	const env = { APM_DB: db };
	run(BIN, ["init", "--name", "Browser Check", "--prefix", "BRW"], env);

	const identifier = (args) => apmJson(args, env).item.identifier;
	const blocked = identifier([
		"create", "--title", "Parser crashes on empty page", "--goal", "verify the UI",
		"--current-state", "fixture ready", "--priority", "highest", "--label", "parser",
	]);
	const waiting = identifier([
		"create", "--title", "Cache layer for dashboard", "--goal", "verify the UI",
		"--current-state", "fixture ready", "--label", "perf",
	]);
	const dependent = identifier([
		"create", "--title", "Depends on parser fix", "--goal", "verify the UI",
		"--current-state", "fixture ready",
	]);
	const ready = identifier([
		"create", "--title", "Polish the docs", "--goal", "verify the UI",
		"--current-state", "fixture ready",
	]);
	const done = identifier([
		"create", "--title", "Ship the old feature", "--goal", "verify the UI",
		"--current-state", "fixture ready",
	]);

	run(BIN, ["claim", blocked, "--session", "browser", "--json"], env);
	run(BIN, ["status", blocked, "--session", "browser", "--to", "doing", "--json"], env);
	run(BIN, ["claim", waiting, "--session", "browser", "--json"], env);
	run(BIN, ["status", waiting, "--session", "browser", "--to", "doing", "--json"], env);
	run(
		BIN,
		["attention", "request", "--item", waiting, "--session", "browser",
			"--question", "Which cache backend?", "--context", "browser fixture"],
		env,
	);
	run(BIN, ["claim", dependent, "--session", "browser", "--json"], env);
	run(BIN, ["dep", "add", dependent, blocked, "--session", "browser", "--json"], env);
	run(BIN, ["release", dependent, "--session", "browser", "--json"], env);
	run(BIN, ["claim", done, "--session", "browser", "--json"], env);
	run(BIN, ["status", done, "--session", "browser", "--to", "doing", "--json"], env);
	run(BIN, ["progress", done, "--session", "browser", "--summary", "shipped", "--json"], env);
	run(
		BIN,
		["evidence", "add", done, "--session", "browser", "--kind", "note", "--value", "released", "--json"],
		env,
	);
	run(BIN, ["status", done, "--session", "browser", "--to", "done", "--note", "released", "--json"], env);

	// Visualisation fixtures: a custom workflow with its own states, a spike that
	// uses it, a story for the parent edge and a cancelled item (every closed
	// category must be reachable, not just "done").
	const workflowFile = path.join(tmp, "review-flow.json");
	fs.writeFileSync(
		workflowFile,
		JSON.stringify({
			id: "review-flow",
			version: 1,
			name: "Review flow",
			initial: "triage",
			states: [
				{ key: "triage", label: "Triage", category: "open", terminal: false },
				{ key: "reviewing", label: "Reviewing", category: "review", terminal: false },
				{ key: "shipped", label: "Shipped", category: "closed", terminal: true },
				{ key: "dropped", label: "Dropped", category: "closed", terminal: true, cancel: true },
			],
			transitions: [
				{ from: "triage", to: "reviewing" },
				{ from: "reviewing", to: "shipped", requires: ["evidence_or_acceptance", "progress_or_note"] },
				{ from: "reviewing", to: "dropped", requires: ["reason"] },
			],
			guards: {
				reason: { kind: "reason", description: "needs a reason" },
				evidence_or_acceptance: { kind: "evidence_or_acceptance", description: "needs evidence" },
				progress_or_note: { kind: "progress_or_note", description: "needs progress" },
			},
		}),
	);
	run(BIN, ["workflow", "set", "--file", workflowFile, "--bind", "spike", "--json"], env);
	const spike = identifier([
		"create", "--title", "Spike with a custom flow", "--goal", "verify the UI",
		"--current-state", "fixture ready", "--type", "spike",
	]);
	run(BIN, ["claim", spike, "--session", "browser", "--json"], env);
	run(BIN, ["status", spike, "--session", "browser", "--to", "reviewing", "--json"], env);
	run(BIN, ["accept", "add", spike, "--session", "browser", "--text", "custom stage renders", "--json"], env);
	run(BIN, ["accept", "add", spike, "--session", "browser", "--text", "guards are shown", "--json"], env);
	run(BIN, ["accept", "set", spike, "--session", "browser", "--seq", "1", "--status", "met", "--json"], env);

	const epic = identifier([
		"create", "--title", "UI epic", "--goal", "verify the UI", "--current-state", "fixture ready", "--type", "story",
	]);
	run(BIN, ["--human", "update", dependent, "--parent", epic, "--json"], env);

	const cancelled = identifier([
		"create", "--title", "Abandoned idea", "--goal", "verify the UI", "--current-state", "fixture ready",
	]);
	run(BIN, ["--human", "status", cancelled, "--to", "cancelled", "--reason", "duplicate", "--json"], env);
	// A closed item with a long title: the closed group must wrap, not overflow.
	const longClosed = identifier([
		"create", "--title",
		"Superseded by the parser rewrite that landed after the cluster upgrade but before the cache migration and therefore never shipped",
		"--goal", "verify the UI", "--current-state", "fixture ready",
	]);
	run(BIN, ["--human", "status", longClosed, "--to", "cancelled", "--reason", "superseded", "--json"], env);

	// APM-33: one open item with a long title (must wrap), a two-level dependency
	// chain (transitive blocker count), and a custom closed state (not just
	// done/cancelled) so the closed group is exercised across workflows.
	const longOpen = identifier([
		"create", "--title",
		"Refactor the incremental snapshot loader so it survives partial migrations from the legacy cluster without dropping subscriber state",
		"--goal", "verify long open titles wrap", "--current-state", "fixture ready",
	]);
	const chain = identifier([
		"create", "--title", "Third item behind a chain", "--goal", "verify transitive blockers",
		"--current-state", "fixture ready",
	]);
	// A long open list is a precondition for the master/detail checks (APM-46):
	// the left column must scroll on its own.
	for (const title of [
		"Filler item one",
		"Filler item two",
		"Filler item three",
		"Filler item four",
		"Filler item five",
		"Filler item six",
	]) {
		run(
			BIN,
			["--human", "create", "--title", title, "--goal", "verify the long list", "--current-state", "filler", "--json"],
			env,
		);
	}
	// CJK titles (APM-46 review): a short two-node chain must fit at a readable
	// zoom, and a long Chinese title must wrap inside its box instead of overflowing.
	const cjkBlocker = identifier([
		"create", "--title", "分片回收", "--goal", "验证中文标题", "--current-state", "fixture ready",
	]);
	const cjkChain = identifier([
		"create", "--title", "映射表修复", "--goal", "验证中文标题", "--current-state", "fixture ready",
	]);
	run(BIN, ["claim", cjkChain, "--session", "browser", "--json"], env);
	run(BIN, ["dep", "add", cjkChain, cjkBlocker, "--session", "browser", "--json"], env);
	run(BIN, ["release", cjkChain, "--session", "browser", "--json"], env);
	const cjkLong = identifier([
		"create", "--title", "修复分片映射表在并发回收阶段偶发死锁并补充长时间回归用例",
		"--goal", "验证中文长标题换行", "--current-state", "fixture ready",
	]);
	run(BIN, ["claim", chain, "--session", "browser", "--json"], env);
	run(BIN, ["dep", "add", chain, dependent, "--session", "browser", "--json"], env);
	run(BIN, ["release", chain, "--session", "browser", "--json"], env);
	const customClosed = identifier([
		"create", "--title", "Custom flow that was dropped", "--goal", "verify custom closed states",
		"--current-state", "fixture ready", "--type", "spike",
	]);
	run(BIN, ["claim", customClosed, "--session", "browser", "--json"], env);
	run(BIN, ["status", customClosed, "--session", "browser", "--to", "reviewing", "--json"], env);
	run(
		BIN,
		["status", customClosed, "--session", "browser", "--to", "dropped", "--reason", "not needed", "--json"],
		env,
	);

	const dashboard = apmJson(["dashboard"], env);

	const server = spawn(BIN, ["ui", "--db", db, "--no-open", "--port", "0"], {
		env: { ...process.env, ...env },
		stdio: ["ignore", "pipe", "pipe"],
	});
	let expectReplayConflict = false;
	let serverOut = "";
	let serverErr = "";
	server.stdout.on("data", (chunk) => (serverOut += chunk));
	server.stderr.on("data", (chunk) => (serverErr += chunk));

	const cleanup = async () => {
		globalThis.__apmUiCleanup = null;
		await browser?.close().catch(() => {});
		server.kill("SIGKILL");
		fs.rmSync(tmp, { recursive: true, force: true });
	};
	let browser;
	globalThis.__apmUiCleanup = cleanup;
	process.on("exit", () => {
		server.kill("SIGKILL");
		fs.rmSync(tmp, { recursive: true, force: true });
	});

	const deadline = Date.now() + 20_000;
	let launchUrl = null;
	while (Date.now() < deadline && !launchUrl) {
		const match = serverOut.match(/ui: (http:\/\/\S+#ticket=\S+)/);
		if (match) launchUrl = match[1];
		else await new Promise((resolve) => setTimeout(resolve, 50));
	}
	if (!launchUrl) {
		server.kill("SIGKILL");
		fail(`apm ui did not print a launch URL. stdout=${serverOut} stderr=${serverErr}`);
	}

	browser = await chromium.launch({
		executablePath,
		headless: !HEADED,
		args: ["--no-sandbox", "--disable-dev-shm-usage"],
	});
	const context = await browser.newContext({ viewport: { width: 1280, height: 900 } });
	const page = await context.newPage();
	const consoleErrors = [];
	const cspViolations = [];
	page.on("console", (message) => {
		const text = message.text();
		if (expectReplayConflict && /409 \(Conflict\)/.test(text)) return; // expected replay
		if (message.type() === "error") consoleErrors.push(text);
		if (/content security policy/i.test(text)) cspViolations.push(text);
	});
	page.on("pageerror", (error) => consoleErrors.push(String(error)));

	const pageTicket = new URL(launchUrl).hash.replace(/^#/, "").split("&").find((part) => part.startsWith("ticket="))?.slice("ticket=".length) ?? "";
	await page.goto(launchUrl, { waitUntil: "networkidle" });
	await page.waitForSelector('section[aria-label="Summary"]');
	await page.waitForFunction(() => /MAIN QUEUE · \d+/.test(document.body.innerText));
	const initialGroups = await page.evaluate(() =>
		[...document.querySelectorAll(".queue-group")].map((group) => ({
			key: group.dataset.group,
			rows: [...group.querySelectorAll("[data-item-id]")].map(
				(node) => node.querySelector(".identifier")?.textContent,
			),
		})),
	);
	// Long open titles must render in full (wrapping is checked visually below).
	const longTitleState = await page.evaluate((id) => {
		const row = [...document.querySelectorAll(".row")].find(
			(node) => node.querySelector(".identifier")?.textContent === id,
		);
		return { found: Boolean(row), rowText: row?.querySelector(".title")?.textContent ?? "" };
	}, longOpen);

	// 1. Ticket exchange and credential hygiene.
	const url = page.url();
	assert(!url.includes("#") && !url.includes("?"), `URL is clean after bootstrap (${url})`);
	const secrets = await page.evaluate(() => ({
		ls: localStorage.length,
		ss: sessionStorage.length,
		cookie: document.cookie,
		html: /apm_(ui|boot)_[0-9a-f]{32}/.test(document.documentElement.outerHTML),
		remote: performance
			.getEntriesByType("resource")
			.map((entry) => entry.name)
			.filter((name) => !name.startsWith(location.origin)),
		overflow: document.documentElement.scrollWidth - document.documentElement.clientWidth,
	}));
	assert(secrets.ls === 0 && secrets.ss === 0 && secrets.cookie === "", "no browser storage holds credentials");
	assert(!secrets.html, "HTML contains no token or ticket");
	assert(secrets.remote.length === 0, "no remote resources are loaded");
	assert(secrets.overflow === 0, "no horizontal overflow at 1280px");
	assert(!serverOut.includes("apm_ui_"), "server stdout never contains the bearer token");
	assert(serverOut.includes("#ticket=apm_boot_"), "server stdout carries the one-time ticket");

	// 2. First paint matches the ledger JSON.
	let text = await page.evaluate(() => document.body.innerText);
	const expected = {
		OPEN: dashboard.totals.open_items,
		"IN PROGRESS": dashboard.buckets.in_progress.length,
		READY: dashboard.buckets.ready_unclaimed.length,
		"WAITING HUMAN": dashboard.totals.waiting_human,
		"DONE": dashboard.totals["status:done"],
	};
	for (const [label, value] of Object.entries(expected)) {
		assert(metric(text, label) === value, `metric ${label}=${metric(text, label)} matches ledger ${value}`);
	}

	// 3. Deduplication: one row per item, multiple badges on one row.
	const rows = await page.evaluate(() =>
		[...document.querySelectorAll('.list-view [aria-label*=": "]')]
			.map((el) => el.getAttribute("aria-label"))
			.filter((label) => /^BRW-\d+: /.test(label)),
	);
	const identifiers = rows.map((label) => label.split(":")[0]);
	assert(new Set(identifiers).size === identifiers.length, "each item appears exactly once (no duplicate rows)");
	const multiBadge = await page.evaluate(() => {
		const article = [...document.querySelectorAll(".list-view article")].find((el) =>
			/BRW-\d+/.test(el.getAttribute("aria-label") || ""),
		);
		return article ? [...article.querySelectorAll(".badge, [class*=badge]")].length : 0;
	});
	assert(multiBadge >= 2, "items in several queues show multiple badges on one row");

	// 4. Filters, search and sort.
	const clickChip = async (label) => {
		await page.click(`button:has-text("${label}")`);
		await page.waitForTimeout(150);
	};
	await clickChip("waiting human");
	text = await page.evaluate(() => document.body.innerText);
	assert(/MAIN QUEUE · \d+ OF \d+/.test(text), "queue filter narrows the list and says how many were filtered out");
	const startable = initialGroups.find((group) => group.key === "startable");
	assert(
		Boolean(startable) && !startable.rows.includes(blocked) && !startable.rows.includes(waiting),
		`list Startable holds only startable work (${startable?.rows.join(",")})`,
	);
	assert(
		Boolean(startable?.rows.includes(longOpen)),
		`a long-titled open item is still startable (${longOpen})`,
	);
	assert(
		longTitleState.found && longTitleState.rowText.startsWith("Refactor the incremental snapshot loader so it survives"),
		`long open titles render in full (${JSON.stringify(longTitleState)})`,
	);

	await clickChip("waiting human");

	const search = page.locator('input[aria-label="Search work items"]');
	await search.fill("parser");
	await page.waitForTimeout(150);
	text = await page.evaluate(() => document.body.innerText);
	assert(text.includes("Depends on parser fix") && !text.includes("Polish the docs"), "search filters by title and label");
	await search.fill("");

	await page.selectOption('select[aria-label="Sort work items"]', "priority");
	await page.waitForTimeout(150);
	const firstRow = await page.evaluate(() =>
		document.querySelector('.list-view [aria-label*=": "]')?.getAttribute("aria-label"),
	);
	assert(firstRow?.startsWith(blocked), `priority sort puts ${blocked} (highest) first`);

	// 5. Recently done fold.
	const fold = page.locator("details summary");
	assert(!(await page.evaluate(() => document.querySelector("details")?.open ?? false)), "recently done starts folded");
	await fold.click();
	await page.waitForTimeout(400);
	const foldText = await page.evaluate(() => document.querySelector("details")?.innerText ?? "");
	assert(
		foldText.includes(done),
		`expanding recently done reveals ${done} (fold: ${JSON.stringify(foldText.slice(0, 200))})`,
	);

	// 5b. Topology view (APM-20): ready vs waiting, blockers, stages, graph.
	await page.click('[data-view="topology"]');
	await page.waitForSelector('[data-group="ready"]');
	const topo = await page.evaluate(() => ({
		ready: [...document.querySelectorAll('[data-group="ready"] [data-item-id]')].map(
			(node) => node.querySelector(".identifier")?.textContent,
		),
		inProgress: [
			...new Set(
				[...document.querySelectorAll('[data-group="in_progress"] [data-item-id]')].map(
					(node) => node.querySelector(".identifier")?.textContent,
				),
			),
		],
		waiting: [...document.querySelectorAll('[data-group="waiting"] [data-item-id]')].map((node) => ({
			id: node.querySelector(".identifier")?.textContent,
			facts: node.querySelector(".topo-facts")?.textContent ?? "",
			blockers: node.querySelector(".topo-blockers")?.textContent ?? "",
		})),
		hash: location.hash,
	}));
	assert(topo.ready.includes(ready), `topology lists unblocked items under Ready to start (${topo.ready.join(",")})`);
	// APM-32: active/claimed work must never leak into Ready to start.
	assert(
		!topo.ready.includes(blocked) && !topo.ready.includes(waiting),
		`doing items stay out of Ready to start (${topo.ready.join(",")})`,
	);
	assert(
		topo.inProgress.includes(blocked),
		`${blocked} is listed under In progress (${topo.inProgress.join(",")})`,
	);
	const dependentCard = topo.waiting.find((entry) => entry.id === dependent);
	assert(
		Boolean(dependentCard) && dependentCard.blockers.includes(blocked),
		`${dependent} is listed as waiting on ${blocked}`,
	);
	assert(/\d+ upstream blocker/.test(dependentCard?.facts ?? ""), `waiting card shows the transitive blocker count (${dependentCard?.facts})`);
	const chainCard = topo.waiting.find((entry) => entry.id === chain);
	assert(
		Boolean(chainCard) && /2 upstream blocker/.test(chainCard.facts),
		`a two-level chain reports both transitive blockers (${chainCard?.facts})`,
	);
	const unlockCard = await page.evaluate((id) => {
		const card = [...document.querySelectorAll('.topology-view [data-item-id]')].find(
			(node) => node.querySelector(".identifier")?.textContent === id,
		);
		return card?.querySelector(".topo-facts")?.textContent ?? "";
	}, blocked);
	assert(/unlocks [1-9]\d*/.test(unlockCard), `a blocker shows how many items it would unlock (${unlockCard})`);
	assert(topo.hash.includes("view=topology"), "view mode is reflected in the URL");

	// Stage bar from the item's own workflow snapshot, plus acceptance progress.
	// Acceptance comes from the item detail, which the UI loads for the selected
	// item only, so select it first.
	await page.evaluate((id) => {
		[...document.querySelectorAll('.topology-view [data-item-id]')]
			.find((node) => node.querySelector(".identifier")?.textContent === id)
			?.click();
	}, spike);
	await page.waitForFunction(
		(id) => {
			const card = [...document.querySelectorAll('.topology-view [data-item-id]')].find(
				(node) => node.querySelector(".identifier")?.textContent === id,
			);
			return /acceptance \d+\/\d+/.test(card?.querySelector(".topo-facts")?.textContent ?? "");
		},
		spike,
		{ timeout: 5000 },
	);
	const stageInfo = await page.evaluate((id) => {
		const card = [...document.querySelectorAll('.topology-view [data-item-id]')].find(
			(node) => node.querySelector(".identifier")?.textContent === id,
		);
		return {
			workflow: card?.querySelector(".stage-bar")?.getAttribute("data-workflow") ?? "",
			labels: [...(card?.querySelectorAll(".stage-label") ?? [])].map((node) => node.textContent),
			current: card?.querySelector('.stage[data-state="current"]')?.textContent ?? "",
			facts: card?.querySelector(".topo-facts")?.textContent ?? "",
		};
	}, spike);
	assert(
		stageInfo.workflow === "review-flow@1" && stageInfo.labels.join(",") === "Triage,Reviewing,Shipped,Dropped",
		`custom workflow stages come from the item snapshot (${stageInfo.workflow}: ${stageInfo.labels.join(",")})`,
	);
	assert(stageInfo.current.includes("Reviewing"), "the current custom stage is marked");
	assert(/acceptance 1\/2 done/.test(stageInfo.facts), `acceptance progress is shown (${stageInfo.facts})`);
	const branches = await page.evaluate(() =>
		[...document.querySelectorAll(".selection-summary .branch")].map((node) => node.textContent ?? ""),
	);
	assert(
		branches.some((line) => line.includes("Shipped") && line.includes("needs evidence")) &&
			branches.some((line) => line.includes("Dropped") && line.includes("needs reason")),
		`custom workflow branches show their guards (${branches.join(" | ")})`,
	);

	// Local graph (APM-46): dependencies is the product default; hierarchy is the
	// other sub-view. The legacy combined picture is never exposed.
	await page.evaluate((id) => {
		[...document.querySelectorAll('.topology-view [data-item-id]')]
			.find((node) => node.querySelector(".identifier")?.textContent === id)
			?.click();
	}, dependent);
	await page.waitForSelector(".dag .dag-node");
	const graph = await page.evaluate(() => {
		const dag = document.querySelector(".dag");
		return {
			relation: dag.dataset.relation,
			roles: [...dag.querySelectorAll(".dag-node")].map((node) => node.dataset.role),
			kinds: [...new Set([...dag.querySelectorAll(".dag-edge")].map((edge) => edge.dataset.kind))],
			arrow: [...dag.querySelectorAll(".dag-edge-dependency")].every((edge) => edge.getAttribute("marker-end")),
			parentEdges: dag.querySelectorAll(".dag-edge-parent").length,
			text: document.querySelector(".dag-text")?.textContent ?? "",
			hash: location.hash,
			relations: [...document.querySelectorAll(".dag-panel button[data-relation]")].map(
				(button) => `${button.dataset.relation}:${button.getAttribute("aria-pressed")}`,
			),
			combined: document.querySelectorAll('[data-relation="combined"], button[data-relation="combined"]').length,
		};
	});
	assert(
		graph.relation === "dependencies" && graph.relations.join(",") === "dependencies:true,hierarchy:false",
		`dependencies is the default relation (${graph.relations.join(",")})`,
	);
	assert(graph.combined === 0, "the legacy combined relation is not exposed");
	assert(
		["upstream", "selected", "downstream"].every((role) => graph.roles.includes(role)),
		`dependency roles: ${graph.roles.join(",")}`,
	);
	assert(
		graph.kinds.length === 1 && graph.kinds[0] === "dependency" && graph.arrow && graph.parentEdges === 0,
		`dependency edges are solid+arrowed and no hierarchy edge leaks in (${graph.kinds.join(",")})`,
	);
	assert(
		/← blocks/.test(graph.text) && /● selected/.test(graph.text) && graph.hash.includes(`item=${dependent}`),
		"the text view mirrors the relations and the selection is in the URL",
	);

	// Scope narrows a dependency chain (and is hidden in the hierarchy view).
	const allNodes = graph.roles.length;
	await page.click('.dag-panel [data-scope="blocking"]');
	await page.waitForTimeout(250);
	const blocking = await page.evaluate(() => ({
		nodes: document.querySelectorAll(".dag-panel .dag .dag-node").length,
		parentEdges: document.querySelectorAll(".dag-panel .dag-edge-parent").length,
		hash: location.hash,
	}));
	assert(
		blocking.parentEdges === 0 && blocking.nodes <= allNodes,
		`the blocking-chain scope keeps only dependencies (${blocking.nodes}/${allNodes})`,
	);
	assert(blocking.hash.includes("scope=blocking"), "graph scope is reflected in the URL");
	await page.click('.dag-panel [data-scope="all"]');
	await page.waitForTimeout(200);

	// Hierarchy sub-view: parent/children only, no dependency edge, no scopes.
	await page.click('.dag-panel button[data-relation="hierarchy"]');
	await page.waitForSelector('.dag[data-relation="hierarchy"] .dag-node');
	const hierarchy = await page.evaluate(() => {
		const dag = document.querySelector('.dag[data-relation="hierarchy"]');
		return {
			roles: [...dag.querySelectorAll(".dag-node")].map((node) => node.dataset.role),
			kinds: [...new Set([...dag.querySelectorAll(".dag-edge")].map((edge) => edge.dataset.kind))],
			dashed: [...dag.querySelectorAll(".dag-edge-parent")].every((edge) => !edge.getAttribute("marker-end")),
			scopesHidden: document.querySelector(".dag-panel .dag-scopes")?.hidden,
			scopesDisplay: (() => {
				const scopes = document.querySelector(".dag-panel .dag-scopes");
				return scopes ? getComputedStyle(scopes).display : null;
			})(),
			scopesArea: (() => {
				const scopes = document.querySelector(".dag-panel .dag-scopes");
				if (!scopes) return null;
				const rect = scopes.getBoundingClientRect();
				return Math.round(rect.width * rect.height);
			})(),
			scopeButtonsVisible: [...document.querySelectorAll(".dag-panel .dag-scope")].filter((button) => {
				const rect = button.getBoundingClientRect();
				return rect.width > 0 && rect.height > 0;
			}).length,
			hash: location.hash,
		};
	});
	assert(
		hierarchy.roles.includes("selected") && hierarchy.roles.some((role) => role === "parent" || role === "child"),
		`hierarchy roles: ${hierarchy.roles.join(",")}`,
	);
	assert(hierarchy.kinds.length === 1 && hierarchy.kinds[0] === "parent", "hierarchy shows parent edges only");
	assert(hierarchy.dashed, "parent edges stay dashed without an arrow");
	assert(
		hierarchy.scopesHidden === true &&
			hierarchy.scopesDisplay === "none" &&
			hierarchy.scopesArea === 0 &&
			hierarchy.scopeButtonsVisible === 0,
		`the dependency scopes take no space in the hierarchy view (display ${hierarchy.scopesDisplay}, area ${hierarchy.scopesArea}, visible buttons ${hierarchy.scopeButtonsVisible})`,
	);
	assert(hierarchy.hash.includes("relation=hierarchy"), "the relation is reflected in the URL");
	await page.click('.dag-panel button[data-relation="dependencies"]');
	await page.waitForSelector('.dag[data-relation="dependencies"] .dag-node');
	await page.waitForTimeout(200);
	// Coming back, the scopes are visible and clickable again — the guard must not
	// hide them permanently.
	const scopesBack = await page.evaluate(() => {
		const scopes = document.querySelector(".dag-panel .dag-scopes");
		const rect = scopes.getBoundingClientRect();
		const first = scopes.querySelector(".dag-scope").getBoundingClientRect();
		return {
			hidden: scopes.hidden,
			display: getComputedStyle(scopes).display,
			area: Math.round(rect.width * rect.height),
			buttonArea: Math.round(first.width * first.height),
		};
	});
	assert(
		scopesBack.hidden === false && scopesBack.display === "flex" && scopesBack.area > 0 && scopesBack.buttonArea > 0,
		`the scopes come back in the dependencies view (display ${scopesBack.display}, area ${scopesBack.area}, button ${scopesBack.buttonArea})`,
	);
	await page.click('.dag-panel [data-scope="upstream"]');
	await page.waitForTimeout(200);
	assert(
		(await page.getAttribute('.dag-panel [data-scope="upstream"]', "aria-pressed")) === "true",
		"a restored scope button is clickable",
	);
	await page.click('.dag-panel [data-scope="all"]');
	await page.waitForTimeout(200);

	// Expanded graph: same picture at a larger size, Esc restores focus, and the
	// scope/relation controls stay in sync with the panel.
	const expandTrigger = page.locator(".dag-expand");
	await expandTrigger.click();
	await page.waitForSelector('.dag-dialog[open] .dag .dag-node');
	const overlay = await page.evaluate(() => {
		const dialog = document.querySelector(".dag-dialog");
		const dag = dialog.querySelector(".dag");
		const rect = dialog.getBoundingClientRect();
		const inline = document.querySelector(".dag-panel .dag");
		return {
			open: dialog.hasAttribute("open") || dialog.open,
			nodes: dag.querySelectorAll(".dag-node").length,
			inlineNodes: inline ? inline.querySelectorAll(".dag-node").length : 0,
			relation: dag.dataset.relation,
			text: dialog.querySelector(".dag-text")?.textContent ?? "",
			viewport: innerWidth,
			width: Math.round(rect.width),
			focusedInside: dialog.contains(document.activeElement),
			overflowX: Math.max(0, document.documentElement.scrollWidth - document.documentElement.clientWidth),
		};
	});
	assert(overlay.open && overlay.focusedInside, "the expanded graph opens as a focused dialog");
	assert(overlay.nodes === overlay.inlineNodes, `the overlay is the same picture (${overlay.nodes}/${overlay.inlineNodes})`);
	assert(overlay.relation === "dependencies", "the overlay keeps the current relation");
	assert(overlay.width <= overlay.viewport, `the overlay fits the viewport (${overlay.width}/${overlay.viewport})`);
	assert(/blocks/.test(overlay.text), "the overlay keeps the text view");
	assert(overlay.overflowX === 0, "the expanded graph adds no horizontal page overflow");
	await page.click('.dag-dialog [data-scope="upstream"]');
	await page.waitForTimeout(250);
	const overlayScope = await page.evaluate(() => ({
		pressed: document.querySelector('.dag-dialog [data-scope="upstream"]')?.getAttribute("aria-pressed"),
		panelPressed: document.querySelector('.dag-panel [data-scope="upstream"]')?.getAttribute("aria-pressed"),
		open: document.querySelector(".dag-dialog")?.hasAttribute("open") ?? false,
	}));
	assert(
		overlayScope.pressed === "true" && overlayScope.panelPressed === "true",
		`scope switches stay in sync between panel and overlay (${JSON.stringify(overlayScope)})`,
	);
	assert(overlayScope.open, "changing scope keeps the overlay open");
	await page.keyboard.press("Escape");
	await page.waitForTimeout(200);
	const afterGraphEscape = await page.evaluate(() => ({
		open: document.querySelector(".dag-dialog")?.hasAttribute("open") ?? false,
		focus: document.activeElement?.getAttribute("aria-label"),
	}));
	assert(!afterGraphEscape.open, "Escape closes the expanded graph");
	assert(afterGraphEscape.focus === "Expand local dependency graph", "focus returns to the Expand trigger");
	await page.click('.dag-panel [data-scope="all"]');
	await page.waitForTimeout(200);

	// A single-node neighbourhood is still legible and expandable (not a tiny blob).
	await page.evaluate((id) => {
		[...document.querySelectorAll('.topology-view [data-item-id]')]
			.find((node) => node.querySelector(".identifier")?.textContent === id)
			?.click();
	}, ready);
	await page.waitForFunction(() => document.querySelectorAll(".dag-panel .dag .dag-node").length === 1);
	const single = await page.evaluate(() => {
		const node = document.querySelector(".dag-panel .dag .dag-node");
		const rect = node.getBoundingClientRect();
		const root = document.querySelector(".dag-panel .dag");
		const canvas = root.querySelector(".dag-canvas");
		return {
			width: Math.round(rect.width),
			height: Math.round(rect.height),
			title: node.querySelector(".dag-node-title")?.textContent ?? "",
			root: [root.clientWidth, root.clientHeight],
			transform: canvas.style.transform,
			level: root.querySelector(".dag-zoom-level")?.textContent,
			aside: document.querySelector(".topo-aside")?.getBoundingClientRect().width,
		};
	});

	assert(
		single.width >= 180 && single.height >= 70,
		`a single node keeps a readable box (${single.width}x${single.height})`,
	);
	assert(single.title.length > 0, "a single node still shows its title");
	await page.click(".dag-expand");
	await page.waitForSelector(".dag-dialog[open] .dag .dag-node");
	await page.keyboard.press("Escape");
	await page.waitForTimeout(150);

	// Restore the multi-node selection the following checks describe.
	await page.evaluate((id) => {
		[...document.querySelectorAll('.topology-view [data-item-id]')]
			.find((node) => node.querySelector(".identifier")?.textContent === id)
			?.click();
	}, dependent);
	await page.waitForFunction(() => document.querySelectorAll(".dag-panel .dag .dag-node").length > 1);

	// Selected summary must not carry duplicate/dead controls (APM-26): the
	// entry point lives on the card, the graph node and the list row.
	const summaryControls = await page.evaluate(() => {
		const summary = document.querySelector('.selection-summary');
		const dead = [...document.querySelectorAll('[data-action="open-detail"]')].filter(
			(button) => !button.closest("[data-item-id]"),
		);
		return {
			summaryButtons: [...summary.querySelectorAll("button")].map((button) => button.textContent?.trim()),
			deadButtons: dead.length,
			summaryHead: summary.querySelector(".summary-head")?.textContent ?? "",
			summaryHasStages: Boolean(summary.querySelector(".stage-bar")),
			cardDetail: Boolean(document.querySelector('.topology-view [data-item-id] [data-action="open-detail"]')),
		};
	});
	assert(
		!summaryControls.summaryButtons.includes("Details"),
		`the selection summary has no duplicate Details button (${summaryControls.summaryButtons.join(",")})`,
	);
	assert(summaryControls.deadButtons === 0, "no Details button exists outside an item container (no dead controls)");
	assert(
		summaryControls.summaryHead.includes(dependent) && summaryControls.summaryHasStages,
		"the summary still shows the identifier, title and stage bar",
	);

	// The real entry point still works by click...
	await page.evaluate((id) => {
		const card = [...document.querySelectorAll('.topology-view [data-item-id]')].find(
			(node) => node.querySelector(".identifier")?.textContent === id,
		);
		card?.querySelector('[data-action="open-detail"]')?.click();
	}, dependent);
	await page.waitForSelector('aside[role="dialog"]:visible', { timeout: 5000 });
	assert(true, "the card Details button still opens the drawer by click");
	await page.keyboard.press("Escape");
	await page.waitForTimeout(200);

	// ...and by keyboard (focus the button, press Enter).
	const openedByKeyboard = await page.evaluate(async (id) => {
		const card = [...document.querySelectorAll('.topology-view [data-item-id]')].find(
			(node) => node.querySelector(".identifier")?.textContent === id,
		);
		const button = card?.querySelector('[data-action="open-detail"]');
		button?.focus();
		const focused = document.activeElement === button;
		button?.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
		return focused;
	}, dependent);
	await page.waitForTimeout(600);
	const keyboardDrawer = await page.evaluate(
		() => !document.querySelector(".drawer-backdrop")?.hasAttribute("hidden"),
	);
	if (keyboardDrawer) await page.keyboard.press("Escape");
	assert(openedByKeyboard, "the card Details button is focusable (keyboard reachable)");

	// Every closed category opens the full drawer — here via an in-session deep
	// link to a cancelled item (the fold check above already covered "done").
	await page.evaluate((id) => {
		location.hash = `#view=topology&item=${id}`;
	}, cancelled);
	await page.waitForSelector('aside[role="dialog"]:visible');
	const closedDrawer = await page.evaluate(() => ({
		title: document.querySelector('aside[role="dialog"] .drawer-title')?.textContent ?? "",
		subtitle: document.querySelector('aside[role="dialog"] .drawer-subtitle')?.textContent ?? "",
		sections: [...document.querySelectorAll('aside[role="dialog"] .detail-heading')].map((node) => node.textContent),
	}));
	assert(closedDrawer.subtitle.includes("cancelled"), `a cancelled item opens from a deep link (${closedDrawer.subtitle})`);
	assert(closedDrawer.sections.length >= 8, "the closed item shows the full drawer, not a reduced view");
	await page.keyboard.press("Escape");
	await page.waitForTimeout(200);

	// Back to the list view for the remaining checks.
	await page.click('[data-view="list"]');
	await page.waitForTimeout(200);

	// 6. Detail drawer: sections, events, copy feedback, Esc, focus return.
	await page.click(`button[aria-label="Open details for ${dependent}"]`);
	await page.waitForSelector('aside[role="dialog"]:visible');
	await page.waitForFunction(
		() => /HANDOVER CONTEXT/i.test(document.querySelector('aside[role="dialog"]')?.innerText ?? ""),
		{ timeout: 5000 },
	);
	const drawer = await page.evaluate(() => {
		const dialog = document.querySelector('aside[role="dialog"]');
		return {
			headings: [...dialog.querySelectorAll("h2, h3")].map((h) => h.textContent.trim()),
			text: dialog.innerText,
			url: location.href,
		};
	});
	const sections = ["HANDOVER CONTEXT", "ACCEPTANCE", "EVIDENCE", "DEPENDENCIES", "CLAIM", "CLOCKS", "RECENT EVENTS", "CLI COMMANDS"];
	for (const section of sections) {
		assert(drawer.text.toUpperCase().includes(section), `detail drawer shows ${section}`);
	}
	assert(drawer.text.includes(blocked), "dependencies section names the blocking item");
	assert(drawer.url.endsWith(`#item=${dependent}`), "drawer stores a #item= deep link");
	await page.click(`button[aria-label="Copy: Read the item"]`);
	await page.waitForTimeout(150);
	const copied = await page.evaluate(() =>
		[...document.querySelectorAll("[aria-live]")].map((el) => el.textContent.trim()).join(" | "),
	);
	assert(copied.includes(`Copied: apm get ${dependent} --json`), "copy announces the command via aria-live");
	assert(!/--token|apm_ui_|--session/.test(copied), "copied commands contain no credentials");
	await page.keyboard.press("Escape");
	await page.waitForTimeout(200);
	const afterEscape = await page.evaluate(() => ({
		visible: !!document.querySelector('aside[role="dialog"]')?.getClientRects().length,
		url: location.href,
		focus: document.activeElement?.getAttribute("aria-label"),
	}));
	assert(!afterEscape.visible, "Escape closes the drawer");
	assert(!afterEscape.url.includes("#"), "closing the drawer clears the deep link");
	assert(
		afterEscape.focus === `Open details for ${dependent}` ||
			afterEscape.focus?.startsWith(dependent),
		`focus returns to the originating row (active: ${JSON.stringify(afterEscape)})`,
	);

	// 7. Automatic refresh (≤5s) and manual refresh.
	const probe = identifier([
		"create", "--title", "Live refresh probe", "--goal", "verify the UI", "--current-state", "created",
	]);
	await page.waitForFunction(
		(identifier) => document.body.innerText.includes(identifier),
		probe,
		{ timeout: 6000 },
	);
	pass(`automatic refresh picked up ${probe} within 5s`);
	await page.click('button:has-text("Refresh")');
	await page.waitForFunction(
		() => /live · updated 0s ago|live · updated 1s ago/.test(document.body.innerText),
		{ timeout: 5000 },
	);
	pass("manual refresh resets the freshness clock");

	// 8. Offline banner keeps the cache, then recovers; filters survive.
	await page.evaluate(() => {
		window.__realFetch = window.fetch.bind(window);
		window.fetch = () => Promise.reject(new TypeError("Failed to fetch"));
	});
	await page.waitForFunction(() => /offline · cached/i.test(document.body.innerText), { timeout: 9000 });
	const offline = await page.evaluate(() => ({
		text: document.body.innerText,
		rows: [...document.querySelectorAll('.list-view [aria-label*=": "]')].filter((el) =>
			/^BRW-\d+: /.test(el.getAttribute("aria-label") || ""),
		).length,
		sort: document.querySelector('select[aria-label="Sort work items"]')?.value,
		fold: document.querySelector("details")?.open,
	}));
	assert(offline.rows >= 5, "offline state keeps the cached rows");
	assert(offline.sort === "priority" && offline.fold === true, "offline keeps filters, sort and the open fold");
	await page.evaluate(() => {
		window.fetch = window.__realFetch;
	});
	await page.waitForFunction(() => !/offline · cached/i.test(document.body.innerText), { timeout: 9000 });
	pass("banner clears when the connection recovers");

	// 8b. The HttpOnly session cookie survives a reload; the ticket stays single-use.
	const beforeReload = await page.evaluate(() => ({
		connection: document.querySelector(".connection")?.textContent ?? "",
		search: document.querySelector('input[aria-label="Search work items"]')?.value ?? "",
	}));
	await page.reload({ waitUntil: "networkidle" });
	await page.waitForFunction(() => /MAIN QUEUE · \d+/.test(document.body.innerText), { timeout: 10_000 });
	expectReplayConflict = true;
	const afterReload = await page.evaluate(async (ticket) => {
		const state = {
			connection: document.querySelector(".connection")?.textContent ?? "",
			search: document.querySelector('input[aria-label="Search work items"]')?.value ?? "",
			url: location.href,
			cookieVisible: document.cookie,
			storage: localStorage.length + sessionStorage.length,
			rows: document.querySelectorAll(".list-view .row").length,
		};
		// Replaying the consumed ticket must fail even though the cookie works.
		window.__expectReplay = true;
		const replay = await fetch("/api/bootstrap", {
			method: "POST",
			headers: { "content-type": "application/json" },
			body: JSON.stringify({ ticket }),
		});
		const replayBody = await replay.text();
		return {
			...state,
			replayStatus: replay.status,
			replayCode: JSON.parse(replayBody || "{}")?.error?.code ?? "",
		};
	}, pageTicket);
	assert(afterReload.connection.startsWith("live"), `a page reload keeps the session via the cookie (${afterReload.connection})`);
	assert(afterReload.rows >= 5, "the reload renders the queue without asking for a ticket");
	assert(afterReload.url === new URL(launchUrl).origin + "/ui/", `the reload URL carries no ticket (${afterReload.url})`);
	assert(afterReload.cookieVisible === "", "the session cookie is HttpOnly (invisible to JavaScript)");
	assert(afterReload.storage === 0, "no Web Storage was used by the reload flow");
	assert(
		afterReload.replayStatus === 409 && afterReload.replayCode === "ticket_consumed",
		`replaying the consumed ticket still fails (${afterReload.replayStatus} ${afterReload.replayCode})`,
	);
	assert(beforeReload.connection.startsWith("live"), "pre-reload session was live");
	expectReplayConflict = false;

	// 8c. Status scope + closed group (APM-30): done/cancelled/aborted are
	// searchable and open the full drawer from the closed group.
	await page.click('[data-view="list"]');
	await page.click('.status-scope [data-status-scope="closed"]');
	await page.waitForTimeout(300);
	const closedScope = await page.evaluate(() => ({
		hash: location.hash,
		rows: [...document.querySelectorAll(".list-view .queue-group .row .identifier")].map((node) => node.textContent),
		foldHidden: document.querySelector(".done-fold")?.hidden ?? false,
		pressed: document.querySelector('.status-scope [data-status-scope="closed"]')?.getAttribute("aria-pressed"),
	}));
	assert(closedScope.pressed === "true" && closedScope.hash.includes("status=closed"), "the Closed scope is active and reflected in the URL");
	assert(
		closedScope.rows.includes(done) && closedScope.rows.includes(cancelled) && closedScope.rows.includes(customClosed),
		`the closed scope lists every closed category incl. custom states (${closedScope.rows.join(",")})`,
	);
	assert(closedScope.foldHidden, "the closed fold is hidden when closed work is a real group");
	await page.fill('input[aria-label="Search work items"]', "Superseded");
	await page.waitForTimeout(300);
	const longClosedSearch = await page.evaluate(() => ({
		rows: [...document.querySelectorAll(".list-view .queue-group .row .identifier")].map((node) => node.textContent),
		overflow: document.documentElement.scrollWidth - document.documentElement.clientWidth,
	}));
	assert(
		longClosedSearch.rows.length === 1 && longClosedSearch.rows[0] === longClosed,
		`searching a long closed title returns it (${longClosedSearch.rows.join(",")})`,
	);
	assert(longClosedSearch.overflow === 0, "a long closed title wraps instead of overflowing");
	await page.fill('input[aria-label="Search work items"]', "");

	// Search reaches a done item directly, and its drawer opens.
	await page.fill('input[aria-label="Search work items"]', done);
	await page.waitForTimeout(300);
	const doneSearch = await page.evaluate(() => ({
		rows: [...document.querySelectorAll(".list-view .queue-group .row .identifier")].map((node) => node.textContent),
	}));
	assert(doneSearch.rows.length === 1 && doneSearch.rows[0] === done, `searching a done item returns exactly it (${doneSearch.rows.join(",")})`);
	await page.click(`button[aria-label="Open details for ${done}"]`);
	await page.waitForSelector('aside[role="dialog"]:visible');
	await page.waitForFunction(() => {
		const subtitle = document.querySelector('aside[role="dialog"] .drawer-subtitle')?.textContent ?? "";
		return subtitle.length > 0 && !subtitle.includes("Loading");
	}, { timeout: 5000 });
	const doneDrawer = await page.evaluate(() => ({
		title: document.querySelector('aside[role="dialog"] .drawer-title')?.textContent ?? "",
		subtitle: document.querySelector('aside[role="dialog"] .drawer-subtitle')?.textContent ?? "",
	}));
	assert(doneDrawer.title.includes(done) && doneDrawer.subtitle.includes("done"), `a done item opens the full drawer (${doneDrawer.title} · ${doneDrawer.subtitle})`);
	await page.keyboard.press("Escape");
	await page.waitForSelector('aside[role="dialog"]', { state: "hidden" });
	await page.waitForTimeout(150);

	// Topology keeps the same closed rows as a group.
	await page.click('[data-view="topology"]');
	await page.waitForSelector('[data-group="closed"]', { state: "attached" });
	const topoClosed = await page.evaluate(() => ({
		rows: [...document.querySelectorAll('[data-group="closed"] [data-item-id] .identifier')].map((node) => node.textContent),
		title: document.querySelector('[data-group="closed"] .topo-title')?.textContent ?? "",
	}));
	assert(topoClosed.rows.length >= 2 && topoClosed.title.includes("Closed"), `topology has an accessible closed group (${topoClosed.title})`);

	// The status scope survives a refresh like the other view state.
	await page.click('.status-scope [data-status-scope="all"]');
	await page.fill('input[aria-label="Search work items"]', "");
	await page.waitForTimeout(300);
	const scopeBefore = await page.evaluate(() => location.hash);
	await page.waitForTimeout(5000);
	const scopeAfter = await page.evaluate(() => ({
		hash: location.hash,
		pressed: document.querySelector('.status-scope [data-status-scope="all"]')?.getAttribute("aria-pressed"),
	}));
	assert(scopeAfter.hash === scopeBefore && scopeAfter.pressed === "true", "the status scope survives an automatic refresh");

	// 390 px with the closed group visible: still no horizontal overflow.
	await page.setViewportSize({ width: 390, height: 844 });
	await page.waitForTimeout(300);
	const narrowClosed = await page.evaluate(
		() => document.documentElement.scrollWidth - document.documentElement.clientWidth,
	);
	assert(narrowClosed === 0, "no horizontal overflow at 390px with the closed group visible");
	await page.setViewportSize({ width: 1280, height: 900 });
	await page.click('.status-scope [data-status-scope="open"]');
	await page.waitForTimeout(200);

	// 8d. Master/detail workspace (APM-46): the list scrolls on its own, the graph
	// and summary stay pinned beside it.
	await page.click('[data-view="topology"]');
	await page.waitForSelector(".topo-list .topo-card");
	const workspace = await page.evaluate(() => {
		const list = document.querySelector(".topo-list");
		const aside = document.querySelector(".topo-aside");
		const panel = document.querySelector(".dag-panel");
		const summary = document.querySelector(".selection-summary");
		return {
			listScrolls: list.scrollHeight > list.clientHeight + 20,
			pageScrolls: document.documentElement.scrollHeight - document.documentElement.clientHeight > 2,
			asideRightOfList: aside.getBoundingClientRect().left >= list.getBoundingClientRect().right - 1,
			asideOrder: [...aside.children].map((node) => node.className).join(","),
			asideHoldsPanel: aside.contains(panel) && aside.contains(summary),
			inlineGraph: Boolean(panel.querySelector(".dag.dag-viewport")),
			overflow: document.documentElement.scrollHeight - document.documentElement.clientHeight,
			listH: Math.round(list.getBoundingClientRect().height),
			clientH: document.documentElement.clientHeight,
			topoTop: document.querySelector(".topology-view").style.getPropertyValue("--topo-top"),
			bottom: Math.round(document.querySelector(".content").getBoundingClientRect().bottom),
		};
	});
	assert(workspace.listScrolls, "the item list scrolls without the page scrolling");
	assert(!workspace.pageScrolls, `the topology workspace fits the viewport height (overflow ${workspace.overflow}px)`);
	assert(workspace.asideRightOfList, "the graph column sits to the right of the list");
	assert(
		workspace.asideHoldsPanel && workspace.asideOrder === "dag-panel,selection-summary",
		`the sticky column is graph-on-top, summary-below (${workspace.asideOrder})`,
	);
	assert(workspace.inlineGraph, "the graph is rendered inline on a desktop viewport");

	// Scroll the list to the end: the page must not move and the graph must stay
	// pinned, which is what makes a long queue usable.
	const asideBefore = await page.evaluate(() =>
		Math.round(document.querySelector(".topo-aside").getBoundingClientRect().top),
	);
	await page.evaluate(() => {
		const list = document.querySelector(".topo-list");
		list.scrollTop = list.scrollHeight;
	});
	await page.waitForTimeout(150);
	const pinned = await page.evaluate(() => ({
		listScroll: document.querySelector(".topo-list").scrollTop,
		pageY: window.scrollY,
		asideTop: Math.round(document.querySelector(".topo-aside").getBoundingClientRect().top),
		asideBottom: Math.round(document.querySelector(".topo-aside").getBoundingClientRect().bottom),
		viewport: innerHeight,
	}));
	assert(pinned.listScroll > 100, `the list scrolled on its own (${pinned.listScroll})`);
	assert(pinned.pageY === 0, "scrolling the list never scrolls the page");
	assert(
		pinned.asideTop === asideBefore && pinned.asideTop >= 0 && pinned.asideBottom <= pinned.viewport + 1,
		`the graph stays put and fully visible while the list scrolls (${pinned.asideTop}px, bottom ${pinned.asideBottom}/${pinned.viewport})`,
	);

	// Selecting the last card needs no page scroll to reach the picture.
	await page.evaluate(() => {
		const cards = [...document.querySelectorAll(".topo-list .topo-card")];
		cards[cards.length - 1].click();
	});
	await page.waitForTimeout(300);
	const lastPick = await page.evaluate(() => {
		const graph = document.querySelector(".dag-panel .dag").getBoundingClientRect();
		return {
			pageY: window.scrollY,
			top: Math.round(graph.top),
			bottom: Math.round(graph.bottom),
			viewport: innerHeight,
			nodes: document.querySelectorAll(".dag-panel .dag .dag-node").length,
			selectedInList: Boolean(document.querySelector(".topo-list .topo-card[aria-current='true'], .topo-list .topo-card[data-selected='true']")),
			listScroll: document.querySelector(".topo-list").scrollTop,
		};
	});
	assert(lastPick.pageY === 0, "selecting the last item does not scroll the page");
	assert(
		lastPick.top >= 0 && lastPick.bottom <= lastPick.viewport + 1,
		`the graph is fully visible without a page scroll (${lastPick.top}-${lastPick.bottom}/${lastPick.viewport})`,
	);
	assert(lastPick.nodes >= 1, "the graph renders the new selection");

	// Column budget (APM-46 review): the graph column gets ~55% of the workspace and
	// the inline picture is shorter, so a simple Chinese chain is readable.
	const columns = await page.evaluate(() => {
		const view = document.querySelector(".topology-view").getBoundingClientRect();
		const aside = document.querySelector(".topo-aside").getBoundingClientRect();
		const graph = document.querySelector(".dag-panel .dag.dag-viewport").getBoundingClientRect();
		return {
			ratio: aside.width / view.width,
			graphHeight: Math.round(graph.height),
			viewportHeight: innerHeight,
		};
	});
	assert(
		columns.ratio >= 0.5 && columns.ratio <= 0.6,
		`the graph column takes ~55% of the workspace (${Math.round(columns.ratio * 100)}%)`,
	);
	assert(
		columns.graphHeight < Math.round(columns.viewportHeight * 0.4),
		`the inline graph is shorter than 40vh (${columns.graphHeight}px of ${columns.viewportHeight})`,
	);

	// A two-node Chinese chain must fit at >= 85% zoom, not be shrunk to ~74%.
	await page.evaluate((id) => {
		[...document.querySelectorAll('.topology-view [data-item-id]')]
			.find((node) => node.querySelector(".identifier")?.textContent === id)
			?.click();
	}, cjkChain);
	await page.waitForFunction(
		(id) =>
			document.querySelectorAll(".dag-panel .dag .dag-node").length === 2 &&
			[...document.querySelectorAll(".dag-panel .dag .dag-node")].some(
				(node) => node.querySelector(".dag-node-id")?.textContent === id,
			),
		cjkChain,
	);
	await page.waitForTimeout(200);
	const cjkFit = await page.evaluate(() => ({
		level: Number.parseInt(document.querySelector(".dag-panel .dag-zoom-level")?.textContent ?? "0", 10),
		nodes: document.querySelectorAll(".dag-panel .dag .dag-node").length,
	}));
	assert(cjkFit.nodes === 2, `the Chinese chain renders both nodes (${cjkFit.nodes})`);
	assert(cjkFit.level >= 85, `a two-node Chinese chain fits at >= 85% (${cjkFit.level}%)`);

	// Vertical geometry (APM-48): the two title lines must not be squeezed by the
	// flex column. Measured at the default fit, at ~150% and at ~174%.
	const verticalGeometry = async (label) =>
		page.evaluate((name) => {
			const node = document.querySelector(".dag-panel .dag .dag-node[data-selected='true']")
				?? document.querySelector(".dag-panel .dag .dag-node");
			const lines = [...node.querySelectorAll(".dag-node-title-line")];
			const title = node.querySelector(".dag-node-title");
			const id = node.querySelector(".dag-node-id");
			const meta = node.querySelector(".dag-node-meta");
			const level = document.querySelector(".dag-panel .dag-zoom-level")?.textContent ?? "";
			const overflow = (element) => (element ? element.scrollHeight - element.clientHeight : 0);
			return {
				label: name,
				level,
				lines: lines.map((line) => ({
					text: line.textContent ?? "",
					client: line.clientHeight,
					scroll: line.scrollHeight,
				})),
				titleOverflow: overflow(title),
				idOverflow: overflow(id),
				metaOverflow: overflow(meta),
				nodeOverflow: overflow(node),
				nodeWidthOverflow: node.scrollWidth - node.clientWidth,
				idVisible: Boolean(id && id.getBoundingClientRect().height > 0),
				metaVisible: Boolean(meta && meta.getBoundingClientRect().height > 0),
				box: [Math.round(node.getBoundingClientRect().width), Math.round(node.getBoundingClientRect().height)],
				tooltip: title?.getAttribute("title") ?? "",
				aria: node.getAttribute("aria-label") ?? "",
			};
		}, label);

	const geometryChecks = [];
	geometryChecks.push(await verticalGeometry("default"));

	// Zoom to >= 150% (each + step is 1.2x from the fit scale).
	for (let i = 0; i < 6; i += 1) {
		const level = await page.evaluate(() =>
			Number.parseInt(document.querySelector(".dag-panel .dag-zoom-level")?.textContent ?? "0", 10),
		);
		if (level >= 150) break;
		await page.click(".dag-panel .dag-zoom-in");
		await page.waitForTimeout(80);
	}
	await page.waitForTimeout(150);
	geometryChecks.push(await verticalGeometry("150%"));

	for (let i = 0; i < 4; i += 1) {
		const level = await page.evaluate(() =>
			Number.parseInt(document.querySelector(".dag-panel .dag-zoom-level")?.textContent ?? "0", 10),
		);
		if (level >= 174) break;
		await page.click(".dag-panel .dag-zoom-in");
		await page.waitForTimeout(80);
	}
	await page.waitForTimeout(150);
	geometryChecks.push(await verticalGeometry("174%"));

	const highScale = geometryChecks[2].level;
	assert(
		Number.parseInt(geometryChecks[1].level, 10) >= 150 && Number.parseInt(highScale, 10) >= 174,
		`zoom reaches the measured scales (${geometryChecks[1].level}, ${highScale})`,
	);
	for (const check of geometryChecks) {
		for (const line of check.lines) {
			assert(
				line.scroll <= line.client,
				`${check.label}: the title line is not clipped vertically ("${line.text}" ${line.scroll}/${line.client}, zoom ${check.level})`,
			);
		}
		assert(
			check.titleOverflow <= 0 && check.nodeOverflow <= 0 && check.idOverflow <= 0 && check.metaOverflow <= 0,
			`${check.label}: title block, id, meta and node have no vertical overflow (${check.titleOverflow}/${check.idOverflow}/${check.metaOverflow}/${check.nodeOverflow}, zoom ${check.level})`,
		);
		assert(
			check.nodeWidthOverflow <= 1,
			`${check.label}: no horizontal overflow at ${check.level} (${check.nodeWidthOverflow})`,
		);
		assert(
			check.idVisible && check.metaVisible && check.tooltip.length > 0 && check.aria.length > 0,
			`${check.label}: id, meta, title tooltip and aria-label all survive (${check.level})`,
		);
		assert(check.lines.length >= 1, `${check.label}: the title lines are rendered (${check.lines.length})`);
	}
	// Back to the fit view for the checks that follow.
	await page.click(".dag-panel .dag-fit");
	await page.waitForTimeout(200);

	// A long Chinese title wraps into two lines inside the box: no overflow, and the
	// full text stays available through the title/aria attributes.
	await page.evaluate((id) => {
		[...document.querySelectorAll('.topology-view [data-item-id]')]
			.find((node) => node.querySelector(".identifier")?.textContent === id)
			?.click();
	}, cjkLong);
	await page.waitForFunction(
		(id) =>
			[...document.querySelectorAll(".dag-panel .dag .dag-node")].some(
				(node) =>
					node.querySelector(".dag-node-id")?.textContent === id &&
					node.querySelectorAll(".dag-node-title-line").length >= 2,
			),
		cjkLong,
	);
	await page.waitForTimeout(200);
	const cjkWrap = await page.evaluate((id) => {
		const node = [...document.querySelectorAll(".dag-panel .dag .dag-node")].find(
			(candidate) => candidate.querySelector(".dag-node-id")?.textContent === id,
		);
		const lines = [...node.querySelectorAll(".dag-node-title-line")];
		const title = node.querySelector(".dag-node-title");
		return {
			lines: lines.map((line) => line.textContent ?? ""),
			overflow: lines.map((line) => line.scrollWidth - line.clientWidth),
			nodeOverflow: node.scrollWidth - node.clientWidth,
			tooltip: title?.getAttribute("title") ?? "",
			aria: node.getAttribute("aria-label") ?? "",
			box: [Math.round(node.getBoundingClientRect().width), Math.round(node.getBoundingClientRect().height)],
		};
	}, cjkLong);
	assert(cjkWrap.lines.length >= 2, `a long Chinese title uses both lines (${cjkWrap.lines.join(" | ")})`);
	assert(
		cjkWrap.overflow.every((gap) => gap <= 1) && cjkWrap.nodeOverflow <= 1,
		`the Chinese title does not overflow its box (${cjkWrap.overflow.join(",")} / ${cjkWrap.nodeOverflow})`,
	);
	assert(cjkWrap.tooltip.length > 5 && cjkWrap.aria.includes("修复分片映射表"), "the full Chinese title stays available");
	assert(cjkWrap.box[0] >= 180 && cjkWrap.box[1] >= 70, `the Chinese node keeps a readable box (${cjkWrap.box.join("x")})`);

	// The empty state must be really hidden on a non-empty graph (APM-46 review): an
	// author `display: grid` used to override `[hidden]`, painting the hint text on
	// top of the edges. Check both directions: non-empty hides it, and a genuinely
	// empty relation shows it.
	const emptyState = await page.evaluate(() => {
		const root = document.querySelector(".dag-panel .dag");
		const empty = root.querySelector(".dag-empty");
		const style = getComputedStyle(empty);
		const rect = empty.getBoundingClientRect();
		return {
			nodes: root.querySelectorAll(".dag-node").length,
			hiddenAttr: empty.hasAttribute("hidden"),
			display: style.display,
			visibility: style.visibility,
			area: Math.round(rect.width * rect.height),
		};
	});
	assert(emptyState.nodes > 0, "the graph has nodes for the empty-state check");
	assert(
		emptyState.display === "none" && emptyState.area === 0,
		`the empty state is not painted on a non-empty graph (display ${emptyState.display}, area ${emptyState.area})`,
	);

	// The rule itself, in both directions: inside a real viewport, a hidden hint is
	// display:none and an unhidden one is painted. (A selected item always keeps one
	// node, so `data-empty="true"` is not reachable from the product UI — this
	// asserts the cascade that the bug report was about.)
	const emptyCascade = await page.evaluate(() => {
		const root = document.querySelector(".dag-panel .dag");
		const probe = document.createElement("p");
		probe.className = "muted dag-empty";
		probe.textContent = "probe";
		probe.hidden = true;
		root.append(probe);
		const hiddenDisplay = getComputedStyle(probe).display;
		probe.hidden = false;
		const shownDisplay = getComputedStyle(probe).display;
		probe.remove();
		const real = root.querySelector(".dag-empty");
		return {
			hiddenDisplay,
			shownDisplay,
			realHidden: real?.hasAttribute("hidden") ?? null,
			rootEmpty: root.dataset.empty,
			canvasHidden: root.querySelector(".dag-canvas")?.hidden ?? null,
		};
	});
	assert(
		emptyCascade.hiddenDisplay === "none",
		`a hidden empty state is display:none, not grid (${emptyCascade.hiddenDisplay})`,
	);
	assert(
		emptyCascade.shownDisplay !== "none",
		`an unhidden empty state is still painted (${emptyCascade.shownDisplay})`,
	);
	assert(
		emptyCascade.realHidden === (emptyCascade.rootEmpty === "false") &&
			emptyCascade.canvasHidden === (emptyCascade.rootEmpty === "true"),
		`the empty hint and canvas visibility follow data-empty (${emptyCascade.rootEmpty})`,
	);

	// A 4s poll refresh keeps the list scroll, the zoom, the search and focus.
	await page.evaluate(() => {
		document.querySelector(".topo-list").scrollTop = 320;
	});
	await page.click(".dag-panel .dag-zoom-in");
	await page.focus(".dag-panel button[data-relation='dependencies']");
	await page.waitForTimeout(200);
	const beforeRefresh = await page.evaluate(() => ({
		scroll: document.querySelector(".topo-list").scrollTop,
		level: document.querySelector(".dag-panel .dag-zoom-level")?.textContent,
		search: document.querySelector('input[aria-label="Search work items"]').value,
		focusRelation: document.activeElement?.getAttribute("data-relation"),
		selected: document.querySelector('.dag-panel .dag .dag-node[data-selected="true"]')?.dataset.itemId,
	}));
	await page.waitForTimeout(5000);
	const afterRefresh = await page.evaluate(() => ({
		scroll: document.querySelector(".topo-list").scrollTop,
		level: document.querySelector(".dag-panel .dag-zoom-level")?.textContent,
		search: document.querySelector('input[aria-label="Search work items"]').value,
		focusRelation: document.activeElement?.getAttribute("data-relation"),
		selected: document.querySelector('.dag-panel .dag .dag-node[data-selected="true"]')?.dataset.itemId,
	}));
	assert(afterRefresh.scroll === beforeRefresh.scroll, `the list keeps its scroll across a refresh (${afterRefresh.scroll})`);
	assert(afterRefresh.level === beforeRefresh.level, `the graph keeps its zoom across a refresh (${afterRefresh.level})`);
	assert(afterRefresh.search === beforeRefresh.search, "the search text survives a refresh");
	assert(afterRefresh.selected === beforeRefresh.selected, "the selection survives a refresh");
	assert(
		afterRefresh.focusRelation === beforeRefresh.focusRelation && beforeRefresh.focusRelation === "dependencies",
		`focus stays on the relation control across a refresh (${afterRefresh.focusRelation})`,
	);

	// Narrow screen: the inline picture collapses into the full-screen overlay.
	await page.setViewportSize({ width: 390, height: 844 });
	await page.waitForTimeout(400);
	const narrowGraph = await page.evaluate(() => ({
		inlineHidden: getComputedStyle(document.querySelector(".dag-panel .dag.dag-viewport")).display === "none",
		expandVisible: (() => {
			const rect = document.querySelector(".dag-expand").getBoundingClientRect();
			return rect.width > 0 && rect.height > 0;
		})(),
		overflow: Math.max(0, document.documentElement.scrollWidth - document.documentElement.clientWidth),
	}));
	assert(narrowGraph.inlineHidden, "the inline graph collapses on a narrow screen");
	assert(narrowGraph.expandVisible, "the Expand control stays reachable at 390px");
	assert(narrowGraph.overflow === 0, "the narrow topology workspace has no horizontal overflow");
	await page.click(".dag-expand");
	await page.waitForSelector(".dag-dialog[open] .dag .dag-node", { state: "visible" });
	const fullscreen = await page.evaluate(() => {
		const rect = document.querySelector(".dag-dialog").getBoundingClientRect();
		return {
			width: Math.round(rect.width),
			height: Math.round(rect.height),
			viewportWidth: innerWidth,
			viewportHeight: innerHeight,
			nodes: document.querySelectorAll(".dag-dialog .dag .dag-node").length,
			overflow: Math.max(0, document.documentElement.scrollWidth - document.documentElement.clientWidth),
		};
	});
	assert(
		fullscreen.width >= fullscreen.viewportWidth - 1 && fullscreen.height >= fullscreen.viewportHeight - 1,
		`the graph fills the viewport at 390px (${fullscreen.width}x${fullscreen.height})`,
	);
	assert(fullscreen.nodes >= 1, "the full-screen graph renders the neighbourhood");
	assert(fullscreen.overflow === 0, "the full-screen graph adds no horizontal overflow");
	await page.keyboard.press("Escape");
	await page.waitForTimeout(250);
	const narrowFocus = await page.evaluate(() => ({
		open: document.querySelector(".dag-dialog")?.hasAttribute("open") ?? false,
		focus: document.activeElement?.getAttribute("aria-label"),
	}));
	assert(!narrowFocus.open, "Escape closes the full-screen graph");
	assert(narrowFocus.focus === "Expand local dependency graph", "focus returns to Expand after the full-screen graph closes");
	await page.setViewportSize({ width: 1280, height: 900 });
	await page.waitForTimeout(300);

	// The plain list view is untouched by the workspace work.
	await page.click('[data-view="list"]');
	const plainList = await page.evaluate(() => ({
		rows: document.querySelectorAll(".list-view .queue-group .row").length,
		nested: document.querySelectorAll(".list-view .topo-list, .list-view .topo-aside").length,
		listScrolls: document.querySelector(".list-view").scrollHeight > document.documentElement.clientHeight,
	}));
	assert(plainList.rows > 0 && plainList.nested === 0, "the plain list view keeps its own layout");
	assert(plainList.listScrolls, "the list view still scrolls the page as before");
	await page.click('[data-view="topology"]');
	await page.waitForSelector(".topo-list .topo-card");

	// 9. XSS payloads stay text.
	const xss = identifier([
		"create", "--title", '<img src=x onerror="window.__xss=1"> <script>window.__xss2=1</script>',
		"--goal", "verify escaping", "--current-state", "created",
	]);
	await page.waitForFunction((identifier) => document.body.innerText.includes(identifier), xss, {
		timeout: 6000,
	});
	const xssState = await page.evaluate(() => ({
		flag: window.__xss ?? null,
		flag2: window.__xss2 ?? null,
		html: document.body.innerHTML.includes("<img src=x onerror="),
	}));
	assert(xssState.flag === null && xssState.flag2 === null && !xssState.html, "XSS payload renders as text, not markup");

	// 10. Narrow viewport keeps everything inside the viewport.
	await page.setViewportSize({ width: 390, height: 844 });
	await page.waitForTimeout(200);
	const narrow = await page.evaluate(
		() => document.documentElement.scrollWidth - document.documentElement.clientWidth,
	);
	assert(narrow === 0, "no horizontal overflow at 390px");

	// The expanded graph stays usable on a phone-sized viewport: it fills the
	// screen, keeps the text view reachable and still closes with Escape.
	await page.click('[data-view="topology"]');
	await page.evaluate(() => {
		document.querySelector(".topology-view .topo-card[data-item-id]")?.click();
	});
	await page.waitForSelector(".dag-panel .dag-expand");
	const beforeOverlay = await page.evaluate(() => ({
		scroll: document.documentElement.scrollWidth - document.documentElement.clientWidth,
		offenders: [...document.querySelectorAll("*")]
			.map((element) => ({ element, right: element.getBoundingClientRect().right }))
			.filter((entry) => entry.right > innerWidth + 1)
			.sort((a, b) => b.right - a.right)
			.slice(0, 4)
			.map((entry) => `${entry.element.tagName}.${entry.element.className} right=${Math.round(entry.right)}`),
	}));
	assert(
		beforeOverlay.scroll === 0,
		`topology view adds no overflow at 390px (${JSON.stringify(beforeOverlay)})`,
	);
	await page.click(".dag-expand");
	await page.waitForSelector(".dag-dialog[open] .dag .dag-node");
	const narrowDialog = await page.evaluate(() => {
		const dialog = document.querySelector(".dag-dialog");
		const rect = dialog.getBoundingClientRect();
		return {
			width: Math.round(rect.width),
			height: Math.round(rect.height),
			left: Math.round(rect.left),
			right: Math.round(rect.right),
			viewport: innerWidth,
			scrollX: document.documentElement.scrollWidth - document.documentElement.clientWidth,
			hasText: Boolean(dialog.querySelector(".dag-text")),
			offenders: [...document.querySelectorAll("*")]
				.map((element) => ({ element, right: element.getBoundingClientRect().right }))
				.filter((entry) => entry.right > innerWidth + 1)
				.sort((a, b) => b.right - a.right)
				.slice(0, 5)
				.map((entry) => `${entry.element.tagName}.${entry.element.className} right=${Math.round(entry.right)}`),
		};
	});
	assert(narrowDialog.width <= narrowDialog.viewport, `expanded graph fits a 390px screen (${narrowDialog.width}/${narrowDialog.viewport})`);
	assert(narrowDialog.height <= 844, `expanded graph fits the phone height (${narrowDialog.height})`);
	assert(narrowDialog.hasText, "the phone overlay keeps the text view");
	assert(
		narrowDialog.scrollX <= beforeOverlay.scroll,
		`the phone overlay adds no page overflow (before=${JSON.stringify(beforeOverlay)}, ${JSON.stringify(narrowDialog)})`,
	);
	await page.keyboard.press("Escape");
	await page.waitForTimeout(150);

	// 11. Console hygiene.
	assert(cspViolations.length === 0, "no CSP violations in the console");
	assert(consoleErrors.length === 0, `console has no errors (${consoleErrors.join(" | ")})`);

	// 12. A restarted process is a different session: it mints a new token, so the
	// old HttpOnly cookie cannot authenticate against it. The cookie-level proof
	// lives in tests/ui.rs (the old cookie gets 401 from the new process); here we
	// assert the observable half — a fresh one-time ticket, never the old one.
	server.kill("SIGKILL");
	const second = spawn(BIN, ["ui", "--db", db, "--no-open", "--port", "0"], {
		env: { ...process.env, ...env },
		stdio: ["ignore", "pipe", "pipe"],
	});
	let secondOut = "";
	const secondUrl = await new Promise((resolve, reject) => {
		const timer = setTimeout(() => reject(new Error(`second apm ui did not start: ${secondOut}`)), 20_000);
		second.stdout.on("data", (chunk) => {
			secondOut += String(chunk);
			const match = secondOut.match(/ui: (http:\/\/\S+#ticket=\S+)/);
			if (match) {
				clearTimeout(timer);
				resolve(match[1]);
			}
		});
	});
	const secondTicket =
		new URL(secondUrl)
			.hash.replace(/^#/, "")
			.split("&")
			.find((part) => part.startsWith("ticket="))
			?.slice("ticket=".length) ?? "";
	assert(Boolean(secondTicket) && secondTicket !== pageTicket, "a restarted apm ui issues a fresh one-time ticket");
	second.kill("SIGKILL");

	await cleanup();
	pass("browser check complete");
}

main().catch(async (error) => {
	console.error(`FAIL ${error.message}`);
	// A thrown check leaves a Chromium and an `apm ui` behind; without an explicit
	// exit the event loop keeps the process alive and a caller's timeout kills it.
	await globalThis.__apmUiCleanup?.().catch(() => {});
	process.exit(1);
});
