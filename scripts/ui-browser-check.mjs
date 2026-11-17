#!/usr/bin/env node
// Real-browser verification for `apm ui` (APM-17).
//
// Spawns the release binary against a throwaway SQLite ledger, drives Chromium
// via playwright-core, and asserts the dashboard behaves like the docs promise:
// ticket exchange, live data, filters, detail drawer, auto/manual refresh,
// offline cache, XSS safety, no token in storage/HTML and a clean console.
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

	const dashboard = apmJson(["dashboard"], env);

	const server = spawn(BIN, ["ui", "--db", db, "--no-open", "--port", "0"], {
		env: { ...process.env, ...env },
		stdio: ["ignore", "pipe", "pipe"],
	});
	let serverOut = "";
	let serverErr = "";
	server.stdout.on("data", (chunk) => (serverOut += chunk));
	server.stderr.on("data", (chunk) => (serverErr += chunk));

	const cleanup = async () => {
		await browser?.close().catch(() => {});
		server.kill("SIGKILL");
		fs.rmSync(tmp, { recursive: true, force: true });
	};
	let browser;
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
		if (message.type() === "error") consoleErrors.push(text);
		if (/content security policy/i.test(text)) cspViolations.push(text);
	});
	page.on("pageerror", (error) => consoleErrors.push(String(error)));

	await page.goto(launchUrl, { waitUntil: "networkidle" });
	await page.waitForSelector('section[aria-label="Summary"]');
	await page.waitForFunction(() => /MAIN QUEUE · \d+/.test(document.body.innerText));

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
		[...document.querySelectorAll('[aria-label*=": "]')]
			.map((el) => el.getAttribute("aria-label"))
			.filter((label) => /^BRW-\d+: /.test(label)),
	);
	const identifiers = rows.map((label) => label.split(":")[0]);
	assert(new Set(identifiers).size === identifiers.length, "each item appears exactly once (no duplicate rows)");
	const multiBadge = await page.evaluate(() => {
		const article = [...document.querySelectorAll("article")].find((el) =>
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
		document.querySelector('[aria-label*=": "]')?.getAttribute("aria-label"),
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
		rows: [...document.querySelectorAll('[aria-label*=": "]')].filter((el) =>
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

	// 11. Console hygiene.
	assert(cspViolations.length === 0, "no CSP violations in the console");
	assert(consoleErrors.length === 0, `console has no errors (${consoleErrors.join(" | ")})`);

	await cleanup();
	pass("browser check complete");
}

main().catch(async (error) => {
	console.error(`FAIL ${error.message}`);
	process.exitCode = 1;
});
