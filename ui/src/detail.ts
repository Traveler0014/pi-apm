/**
 * Read-only detail drawer.
 *
 * One dialog per page: open it from a row (click, Enter or Space), it loads
 * `/api/items/{id}` and `/api/items/{id}/events`, renders every handover field
 * the ledger stores, and offers copyable CLI commands so the human can continue
 * in a terminal.
 *
 * Accessibility contract (acceptance 2 of APM-16):
 *   - `role="dialog" aria-modal="true"`, labelled by the item title;
 *   - focus moves into the dialog on open and returns to the invoking row on
 *     close; Tab cycles inside the dialog; Escape closes;
 *   - every status is a symbol **and** a word, never colour alone;
 *   - text is rendered with `textContent` only, long values wrap and collapsible
 *     blocks are clamped, so a hostile ledger entry cannot inject markup or
 *     stretch the layout.
 */

import { type Api, ApiError } from "./api";
import { cliCommands } from "./commands";
import { clockTime, relativeTime } from "./transform";
import type { EventView, ItemDetail, WorkItem } from "./types";

export interface DrawerOptions {
	api: Api;
	/** Clock override for tests. */
	now?: () => number;
	/** Notified when the drawer opens/closes (main.ts uses it for the URL hash). */
	onOpenChange?: (identifier: string | null) => void;
	/** Clipboard bridge; defaults to `navigator.clipboard` with a DOM fallback. */
	copyText?: (text: string) => Promise<boolean>;
}

export interface Drawer {
	open(id: string): Promise<void>;
	close(): void;
	isOpen(): boolean;
	currentIdentifier(): string | null;
	/** Re-fetch the open item (called after a dashboard refresh). */
	refreshIfOpen(): Promise<void>;
	element: HTMLElement;
}

const ACCEPTANCE_MARK: Record<string, string> = {
	met: "✓ met",
	waived: "– waived",
	open: "○ open",
};

function el<K extends keyof HTMLElementTagNameMap>(
	tag: K,
	props: { className?: string; text?: string; attrs?: Record<string, string> } = {},
): HTMLElementTagNameMap[K] {
	const node = document.createElement(tag);
	if (props.className) node.className = props.className;
	if (props.text !== undefined) node.textContent = props.text;
	for (const [key, value] of Object.entries(props.attrs ?? {})) node.setAttribute(key, value);
	return node;
}

/** Section with a heading and an optional count, so long lists stay scannable. */
function section(title: string, count?: number): { root: HTMLElement; body: HTMLElement; heading: HTMLElement } {
	const root = el("section", { className: "detail-section" });
	const heading = el("h3", { className: "detail-heading" });
	heading.append(el("span", { text: title }));
	if (count !== undefined) heading.append(el("span", { className: "count", text: String(count) }));
	const body = el("div", { className: "detail-body" });
	root.append(heading, body);
	return { root, body, heading };
}

function field(body: HTMLElement, label: string, value: string | null | undefined, options: { pre?: boolean } = {}): void {
	if (value === null || value === undefined || value === "") return;
	const row = el("div", { className: "field" });
	row.append(el("span", { className: "field-label", text: label }));
	const text = el(options.pre ? "pre" : "p", { className: "field-value", text: value });
	row.append(text);
	body.append(row);
}

function list(body: HTMLElement, values: string[], emptyText = "none"): void {
	if (values.length === 0) {
		body.append(el("p", { className: "muted", text: emptyText }));
		return;
	}
	const ul = el("ul", { className: "bullets" });
	for (const value of values) ul.append(el("li", { text: value }));
	body.append(ul);
}

/**
 * Long text: clamp to a few lines with a native disclosure, so a 10 KB progress
 * summary cannot push the rest of the drawer off the screen.
 */
function clamped(body: HTMLElement, label: string, value: string | null | undefined, limit = 280): void {
	if (!value) return;
	const row = el("div", { className: "field" });
	row.append(el("span", { className: "field-label", text: label }));
	if (value.length <= limit) {
		row.append(el("p", { className: "field-value", text: value }));
	} else {
		const details = el("details", { className: "clamped" });
		details.append(el("summary", { text: `${value.slice(0, limit).trimEnd()}…` }));
		details.append(el("pre", { className: "field-value", text: value }));
		row.append(details);
	}
	body.append(row);
}

function defaultCopy(): (text: string) => Promise<boolean> {
	return async (text: string) => {
		try {
			if (navigator.clipboard?.writeText) {
				await navigator.clipboard.writeText(text);
				return true;
			}
		} catch {
			// fall through to the legacy path
		}
		try {
			const area = document.createElement("textarea");
			area.value = text;
			area.setAttribute("readonly", "");
			area.style.position = "fixed";
			area.style.opacity = "0";
			document.body.append(area);
			area.select();
			const ok = document.execCommand?.("copy") ?? false;
			area.remove();
			return ok;
		} catch {
			return false;
		}
	};
}

export function createDrawer(options: DrawerOptions): Drawer {
	const now = options.now ?? (() => Date.now());
	const copyText = options.copyText ?? defaultCopy();

	const backdrop = el("div", { className: "drawer-backdrop", attrs: { hidden: "" } });
	const dialog = el("aside", {
		className: "drawer",
		attrs: { role: "dialog", "aria-modal": "true", "aria-labelledby": "drawer-title", tabindex: "-1" },
	});
	const header = el("header", { className: "drawer-head" });
	const title = el("h2", { className: "drawer-title", attrs: { id: "drawer-title" } });
	const subtitle = el("p", { className: "drawer-subtitle" });
	const actions = el("div", { className: "drawer-actions" });
	const refreshButton = el("button", { className: "ghost", text: "Refresh", attrs: { type: "button" } });
	const closeButton = el("button", { className: "ghost", text: "Close ✕", attrs: { type: "button", "aria-label": "Close details (Escape)" } });
	actions.append(refreshButton, closeButton);
	header.append(title, subtitle, actions);
	const status = el("p", { className: "drawer-status", attrs: { role: "status", "aria-live": "polite" } });
	const content = el("div", { className: "drawer-content" });
	dialog.append(header, status, content);
	backdrop.append(dialog);

	let openId: string | null = null;
	let identifier: string | null = null;
	let detail: ItemDetail | null = null;
	let events: EventView[] = [];
	let eventsAvailable = true;
	let eventsError: string | null = null;
	let inFlight: AbortController | null = null;
	let lastFocused: HTMLElement | null = null;
	let copyAnnouncement = "";

	function announce(text: string): void {
		copyAnnouncement = text;
		status.textContent = text;
	}

	function visible(): boolean {
		return !backdrop.hasAttribute("hidden");
	}

	function close(): void {
		if (!visible()) return;
		inFlight?.abort();
		backdrop.setAttribute("hidden", "");
		document.body.classList.remove("drawer-open");
		identifier = null;
		const restore = lastFocused;
		const fallbackId = openId;
		lastFocused = null;
		options.onOpenChange?.(null);
		// The row may have been re-rendered by a dashboard refresh while the drawer
		// was open, so fall back to looking it up by id before giving up on focus.
		const target = restore?.isConnected ? restore : fallbackId ? document.querySelector<HTMLElement>(`[data-item-id="${fallbackId}"]`) : null;
		target?.focus?.({ preventScroll: true });
		openId = null;
	}

	async function load(id: string, showSpinner: boolean): Promise<void> {
		inFlight?.abort();
		const controller = new AbortController();
		inFlight = controller;
		eventsAvailable = true;
		eventsError = null;
		if (showSpinner) {
			status.textContent = "Loading item…";
			content.replaceChildren(el("p", { className: "muted", text: "Loading…" }));
		}
		try {
			const [detailResponse, eventsResponse] = await Promise.all([
				options.api.getItem(id, controller.signal),
				options.api.getEvents(id, 20, controller.signal).catch((cause: unknown) => {
					// The events feed is optional (older servers may not expose it); the
					// drawer must still render the handover data, but it should say why
					// the list is empty instead of pretending there are no events.
					eventsAvailable = false;
					eventsError = cause instanceof ApiError ? `${cause.code}: ${cause.message}` : String(cause);
					return { ok: true as const, events: [] as EventView[] };
				}),
			]);
			detail = detailResponse.item;
			events = eventsResponse.events;
			identifier = detail.item.identifier;
			status.textContent = copyAnnouncement;
			renderDetail();
		} catch (cause) {
			if (cause instanceof Error && (cause.name === "AbortError" || /abort/i.test(cause.message))) return;
			const message =
				cause instanceof ApiError ? `${cause.code}: ${cause.message}` : cause instanceof Error ? cause.message : String(cause);
			status.textContent = `Could not load the item — ${message}`;
			content.replaceChildren(
				el("p", { className: "muted", text: `Could not load this item (${message}).` }),
				(() => {
					const retry = el("button", { className: "ghost", text: "Retry", attrs: { type: "button" } });
					retry.addEventListener("click", () => void load(id, true));
					return retry;
				})(),
			);
		} finally {
			if (inFlight === controller) inFlight = null;
		}
	}

	async function open(id: string): Promise<void> {
		lastFocused = document.activeElement as HTMLElement | null;
		openId = id;
		backdrop.removeAttribute("hidden");
		document.body.classList.add("drawer-open");
		title.textContent = "Loading…";
		subtitle.textContent = "";
		status.textContent = "";
		copyAnnouncement = "";
		content.replaceChildren();
		dialog.focus({ preventScroll: true });
		await load(id, true);
		options.onOpenChange?.(identifier);
		// Move focus to Close so the first Tab lands inside the dialog.
		closeButton.focus({ preventScroll: true });
	}

	function renderDetail(): void {
		if (!detail) return;
		const item = detail.item;
		title.textContent = `${item.identifier} · ${item.title}`;
		subtitle.textContent = [
			item.status,
			`health ${item.health}`,
			`priority ${item.priority}`,
			item.item_type,
			item.workflow_id ? `workflow ${item.workflow_id} v${item.workflow_version}` : null,
			item.parent_identifier ? `parent ${item.parent_identifier}` : null,
		]
			.filter(Boolean)
			.join(" · ");

		const nodes: HTMLElement[] = [];

		// --- handover context -------------------------------------------------
		const context = section("Handover context", detail.context.version);
		const doc = detail.context.doc;
		field(context.body, "Goal", doc.goal);
		field(context.body, "Current state", doc.current_state);
		if (doc.constraints.length > 0) {
			context.body.append(el("span", { className: "field-label", text: `Constraints (${doc.constraints.length})` }));
			list(context.body, doc.constraints, "none");
		}
		if (doc.references.length > 0) {
			context.body.append(el("span", { className: "field-label", text: `References (${doc.references.length})` }));
			list(context.body, doc.references, "none");
		}
		clamped(context.body, "Notes", doc.notes);
		context.body.append(
			el("p", {
				className: "muted",
				text: `context v${detail.context.version} · ${detail.context.author} · ${relativeTime(detail.context.created_at, now())} · reason: ${detail.context.reason}`,
			}),
		);
		nodes.push(context.root);

		// --- acceptance -------------------------------------------------------
		const acceptance = section("Acceptance", detail.acceptance.length);
		if (detail.acceptance.length === 0) {
			acceptance.body.append(el("p", { className: "muted", text: "No acceptance criteria defined." }));
		} else {
			const ul = el("ul", { className: "checks" });
			for (const criterion of detail.acceptance) {
				const li = el("li", { className: "check" });
				li.dataset.status = criterion.status;
				li.append(el("span", { className: "check-mark", text: ACCEPTANCE_MARK[criterion.status] ?? criterion.status }));
				li.append(el("span", { className: "check-text", text: `${criterion.seq}. ${criterion.text}` }));
				if (criterion.note) li.append(el("span", { className: "check-note", text: criterion.note }));
				ul.append(li);
			}
			acceptance.body.append(ul);
		}
		nodes.push(acceptance.root);

		// --- evidence ---------------------------------------------------------
		const evidence = section("Evidence", detail.evidence.length);
		if (detail.evidence.length === 0) {
			evidence.body.append(el("p", { className: "muted", text: "No evidence attached." }));
		} else {
			for (const row of detail.evidence) {
				const block = el("div", { className: "evidence" });
				block.append(el("span", { className: "badge", text: row.kind }));
				clamped(block, "", row.value, 400);
				if (row.note) block.append(el("p", { className: "muted", text: row.note }));
				if (row.created_by || row.created_at) {
					block.append(
						el("p", {
							className: "muted",
							text: [row.created_by, row.created_at ? relativeTime(row.created_at, now()) : null].filter(Boolean).join(" · "),
						}),
					);
				}
				evidence.body.append(block);
			}
		}
		nodes.push(evidence.root);

		// --- dependencies -----------------------------------------------------
		const deps = section("Dependencies", item.blocked_by.length + item.blocks.length);
		if (item.blocked_by.length === 0 && item.blocks.length === 0) {
			deps.body.append(el("p", { className: "muted", text: "No dependency edges." }));
		} else {
			const render = (label: string, rows: WorkItem[] | ItemDetail["item"]["blocked_by"]): void => {
				if (rows.length === 0) return;
				deps.body.append(el("span", { className: "field-label", text: label }));
				const ul = el("ul", { className: "bullets" });
				for (const row of rows) ul.append(el("li", { text: `${row.identifier} · ${row.status} · ${row.title}` }));
				deps.body.append(ul);
			};
			render("Blocked by", item.blocked_by);
			render("Blocks", item.blocks);
		}
		nodes.push(deps.root);

		// --- claim ------------------------------------------------------------
		const claim = section("Claim");
		if (!item.claim) {
			claim.body.append(el("p", { className: "muted", text: "No claim: the item is free to take." }));
		} else {
			const c = item.claim;
			field(claim.body, "Holder", `${c.holder} (${c.holder_kind})`);
			field(claim.body, "Status", c.expired ? `${c.status} · lease expired` : c.status);
			if (c.expires_at) field(claim.body, "Expires", `${clockTime(c.expires_at)} (${relativeTime(c.expires_at, now())})`);
			if (c.lease_seconds) field(claim.body, "Lease", `${c.lease_seconds}s`);
			if (c.leased_at) field(claim.body, "Leased", `${clockTime(c.leased_at)} (${relativeTime(c.leased_at, now())})`);
			if (c.session_id) field(claim.body, "Session", c.session_id);
			if (c.run_id) field(claim.body, "Run", c.run_id);
		}
		nodes.push(claim.root);

		// --- three clocks -----------------------------------------------------
		const clocks = section("Clocks");
		const clockRow = (label: string, iso: string | null | undefined, extra?: string): void => {
			const row = el("div", { className: "clock" });
			row.append(el("span", { className: "field-label", text: label }));
			row.append(el("span", { className: "field-value", text: iso ? `${clockTime(iso)} · ${relativeTime(iso, now())}${extra ? ` · ${extra}` : ""}` : "never" }));
			clocks.body.append(row);
		};
		clockRow("Heartbeat", item.last_heartbeat_at);
		clockRow("Progress", item.last_progress_at, item.seconds_since_progress ? `${item.seconds_since_progress}s without progress` : undefined);
		clockRow("Run sync", item.last_run_sync_at);
		clocks.body.append(
			el("p", {
				className: "muted",
				text: `created ${clockTime(item.created_at)} · updated ${clockTime(item.updated_at)}${item.done_at ? ` · done ${clockTime(item.done_at)}` : ""}${
					item.cancelled_at ? ` · cancelled ${clockTime(item.cancelled_at)}` : ""
				} · version ${item.version} (context v${item.context_version})`,
			}),
		);
		nodes.push(clocks.root);

		// --- attention --------------------------------------------------------
		if (detail.attention.length > 0) {
			const attention = section("Attention", detail.attention.length);
			for (const request of detail.attention) {
				const block = el("div", { className: "attention" });
				block.append(el("p", { className: "question", text: `${request.blocking ? "⏸ blocking" : "· non-blocking"} · ${request.status} · ${request.question}` }));
				if (request.context) block.append(el("p", { className: "muted", text: request.context }));
				if (request.options && request.options.length > 0) list(block, request.options, "");
				if (request.recommendation) block.append(el("p", { className: "muted", text: `recommendation: ${request.recommendation}` }));
				if (request.response) block.append(el("p", { text: `answer: ${request.response}` }));
				attention.body.append(block);
			}
			nodes.push(attention.root);
		}

		// --- recent events ----------------------------------------------------
		const recent = section("Recent events", events.length);
		if (events.length === 0) {
			recent.body.append(
				el("p", {
					className: "muted",
					text: eventsAvailable
						? "No events recorded for this item yet."
						: `Event feed unavailable from this server (${eventsError ?? "unknown error"}).`,
				}),
			);
		} else {
			const ul = el("ul", { className: "events" });
			for (const event of events.slice(0, 20)) {
				const li = el("li", { className: "event" });
				li.append(el("span", { className: "event-kind", text: event.kind }));
				li.append(
					el("span", {
						className: "event-meta",
						text: `${event.actor} · ${clockTime(event.at)} · ${relativeTime(event.at, now())}${
							event.new_version !== null && event.new_version !== undefined
								? event.prev_version === null || event.prev_version === undefined
									? ` · →v${event.new_version}`
									: event.prev_version === event.new_version
										? ""
										: ` · v${event.prev_version}→v${event.new_version}`
								: ""
						}`,
					}),
				);
				if (event.payload && typeof event.payload === "object" && Object.keys(event.payload as object).length > 0) {
					const payload = JSON.stringify(event.payload);
					const details = el("details", { className: "clamped" });
					details.append(el("summary", { text: payload.slice(0, 140) + (payload.length > 140 ? "…" : "") }));
					if (payload.length > 140) details.append(el("pre", { text: payload }));
					li.append(details);
				}
				ul.append(li);
			}
			recent.body.append(ul);
		}
		nodes.push(recent.root);

		// --- CLI commands -----------------------------------------------------
		const commands = section("CLI commands");
		commands.body.append(el("p", { className: "muted", text: "The dashboard is read-only; run these in a terminal to act." }));
		for (const spec of cliCommands(item)) {
			const row = el("div", { className: "command" });
			row.append(el("code", { className: "command-text", text: spec.command }));
			const copy = el("button", { className: "ghost copy", text: "Copy", attrs: { type: "button", "aria-label": `Copy: ${spec.label}` } });
			copy.dataset.command = spec.command;
			copy.addEventListener("click", async () => {
				const ok = await copyText(spec.command);
				announce(ok ? `Copied: ${spec.command}` : `Copy failed — select the text manually: ${spec.command}`);
			});
			row.append(copy);
			if (spec.hint) row.append(el("p", { className: "muted", text: spec.hint }));
			commands.body.append(row);
		}
		nodes.push(commands.root);

		content.replaceChildren(...nodes);
	}

	// --- wiring -------------------------------------------------------------

	closeButton.addEventListener("click", () => close());
	refreshButton.addEventListener("click", () => {
		if (openId) void load(openId, true);
	});
	backdrop.addEventListener("mousedown", (event) => {
		if (event.target === backdrop) close();
	});

	dialog.addEventListener("keydown", (event) => {
		if (event.key === "Escape") {
			event.preventDefault();
			close();
			return;
		}
		if (event.key !== "Tab") return;
		// Focus trap: keep Tab inside the dialog.
		const focusable = [...dialog.querySelectorAll<HTMLElement>('button, [href], input, select, textarea, summary, [tabindex]:not([tabindex="-1"])')].filter(
			(node) => !node.hasAttribute("disabled") && node.offsetParent !== null,
		);
		if (focusable.length === 0) return;
		const first = focusable[0]!;
		const last = focusable[focusable.length - 1]!;
		const active = document.activeElement;
		if (event.shiftKey && (active === first || active === dialog)) {
			event.preventDefault();
			last.focus();
		} else if (!event.shiftKey && active === last) {
			event.preventDefault();
			first.focus();
		}
	});

	document.body.append(backdrop);

	return {
		element: backdrop,
		async open(id: string) {
			await open(id);
		},
		close,
		isOpen: visible,
		currentIdentifier: () => identifier,
		async refreshIfOpen() {
			if (openId) await load(openId, false);
		},
	};
}
