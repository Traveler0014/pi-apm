/**
 * API client for the embedded dashboard.
 *
 * Same-origin, absolute `/api/*` paths (the page is served from `/ui/`).
 *
 * Authentication is a two-step hand-off: `apm ui` prints a URL whose fragment
 * carries a **one-time bootstrap ticket** (short TTL), the page erases the
 * fragment immediately and exchanges the ticket at `POST /api/bootstrap`. The
 * server answers with an **HttpOnly, SameSite=Strict session cookie scoped to
 * `/api`** — the bearer token itself is never returned to JavaScript. That is why
 * a page reload keeps working (the browser replays the cookie) while the ticket
 * stays single-use, and why nothing credential-shaped can reach Web Storage, the
 * DOM, the URL or a log line.
 */

import type {
	DashboardResponse,
	EventsResponse,
	Fetcher,
	HealthResponse,
	ItemDetailResponse,
	ItemWorkflowResponse,
	ItemsResponse,
	WorkflowDefinitionResponse,
} from "./types";

export interface ApiOptions {
	basePath?: string;
	fetcher?: Fetcher;
}

interface ApiErrorShape {
	status: number;
	code: string;
	message: string;
}

export class ApiError extends Error {
	readonly status: number;
	readonly code: string;

	constructor({ status, code, message }: ApiErrorShape) {
		super(message);
		this.name = "ApiError";
		this.status = status;
		this.code = code;
	}
}

/** Error codes `/api/bootstrap` may return for a one-time ticket. */
export type TicketErrorCode = "ticket_consumed" | "ticket_expired" | "invalid_ticket";

export class TicketError extends Error {
	readonly code: string;
	readonly status: number;

	constructor(code: string, status: number, message: string) {
		super(message);
		this.name = "TicketError";
		this.code = code;
		this.status = status;
	}
}

/**
 * Swap a one-time bootstrap ticket for the real API token.
 *
 * The ticket arrives in the URL fragment, is single-use with a short TTL, and is
 * sent in the `Authorization` header of one `POST /api/bootstrap` request. The
 * returned token stays in module memory; it is never printed, stored or put back
 * into the URL. A failure is final: the caller must not retry, because the ticket
 * is consumed (or expired) — the user has to re-run `apm ui`.
 */
export async function exchangeTicket(
	ticket: string,
	fetcher: Fetcher = defaultFetcher,
	basePath = "/api",
): Promise<boolean> {
	const base = basePath.replace(/\/$/, "");
	const value = ticket.trim();
	if (!value) throw new TicketError("invalid_ticket", 0, "empty bootstrap ticket");
	// The ticket travels twice on purpose: the backend accepts either the bearer
	// header or the JSON body (curl vs. fetch), and sending both keeps the
	// contract symmetric without weakening it.
	const response = await fetcher(`${base}/bootstrap`, {
		method: "POST",
		headers: {
			accept: "application/json",
			"content-type": "application/json",
			// The ticket travels in the body; the header form is kept for curl-style
			// callers but is not needed by the browser.
			authorization: `Bearer ${value}`,
		},
		body: JSON.stringify({ ticket: value }),
		cache: "no-store",
		credentials: "same-origin",
	});
	const text = await response.text();
	let body: unknown = null;
	try {
		body = text ? JSON.parse(text) : null;
	} catch {
		throw new TicketError("invalid_ticket", response.status, "bootstrap response was not JSON");
	}
	if (!response.ok) {
		const failure = body as { error?: { code?: string; message?: string } } | null;
		throw new TicketError(
			failure?.error?.code ?? `http_${response.status}`,
			response.status,
			failure?.error?.message ?? `bootstrap failed with status ${response.status}`,
		);
	}
	// The session lives in the HttpOnly cookie the response set; the body must not
	// contain the credential. If a future server did return one, it is ignored on
	// purpose rather than stored.
	const leaked = (body as { token?: unknown } | null)?.token;
	if (typeof leaked === "string" && leaked.length > 0) {
		throw new TicketError(
			"invalid_ticket",
			response.status,
			"bootstrap returned a token; this UI only accepts an HttpOnly cookie session",
		);
	}
	return true;
}

/**
 * Read `#ticket=…` (also `…&ticket=…`) from the fragment, erase the fragment so
 * the ticket never reaches history/referrers, then exchange it for the token.
 *
 * Returns true when a ticket was present and exchanged.
 */
/** Read `#ticket=…` without consuming it. */
export function ticketFromFragment(location: Pick<Location, "hash">): string | null {
	const raw = location.hash.startsWith("#") ? location.hash.slice(1) : location.hash;
	if (!raw) return null;
	return new URLSearchParams(raw).get("ticket");
}

/**
 * Remove the ticket from the address bar, keeping any other fragment keys (view,
 * item). Safe to call when there is no ticket.
 */
export function stripTicketFragment(
	location: Pick<Location, "hash" | "pathname" | "search">,
	history: Pick<History, "replaceState">,
): boolean {
	const raw = location.hash.startsWith("#") ? location.hash.slice(1) : location.hash;
	if (!raw) return false;
	const params = new URLSearchParams(raw);
	if (!params.has("ticket")) return false;
	params.delete("ticket");
	const rest = params.toString();
	history.replaceState(null, "", `${location.pathname}${location.search}${rest ? `#${rest}` : ""}`);
	return true;
}

export async function bootstrapTokenFromFragment(
	location: Pick<Location, "hash" | "pathname" | "search">,
	history: Pick<History, "replaceState">,
	options: { fetcher?: Fetcher; basePath?: string } = {},
): Promise<boolean> {
	const raw = location.hash.startsWith("#") ? location.hash.slice(1) : location.hash;
	if (!raw) return false;
	const params = new URLSearchParams(raw);
	const ticket = params.get("ticket");
	if (!ticket) return false;
	// Erase the ticket first: even a failed exchange must not leave it in the URL
	// bar (it is a bearer secret for one request). Other fragment keys — the UI
	// view and the selected item — are kept so a merged launch URL still deep-links.
	params.delete("ticket");
	const rest = params.toString();
	history.replaceState(null, "", `${location.pathname}${location.search}${rest ? `#${rest}` : ""}`);
	await exchangeTicket(ticket, options.fetcher, options.basePath);
	return true;
}

/** Human-readable message for the "re-run apm ui" state. */
export function ticketErrorMessage(error: unknown): string {
	if (error instanceof TicketError) {
		switch (error.code) {
			case "ticket_consumed":
				return "This dashboard link was already used (the ticket is single-use). Run `apm ui` again to get a new one.";
			case "ticket_expired":
				return "This dashboard link expired. Run `apm ui` again to get a new one.";
			case "invalid_ticket":
				return "This dashboard link is not valid. Run `apm ui` again to get a new one.";
			default:
				return `Could not start a dashboard session (${error.code}). Run \`apm ui\` again.`;
		}
	}
	return error instanceof Error && error.message ? `Could not start a dashboard session: ${error.message}` : "Could not start a dashboard session.";
}

const defaultFetcher: Fetcher = (url, init) =>
	fetch(url, {
		method: init.method ?? "GET",
		headers: init.headers,
		body: init.body,
		cache: init.cache,
		signal: init.signal,
		credentials: init.credentials ?? "same-origin",
	});

export interface Api {
	getDashboard(signal?: AbortSignal): Promise<DashboardResponse>;
	getItems(query?: Record<string, string | number | boolean | undefined>, signal?: AbortSignal): Promise<ItemsResponse>;
	getItem(id: string, signal?: AbortSignal): Promise<ItemDetailResponse>;
	getEvents(id: string, limit?: number, signal?: AbortSignal): Promise<EventsResponse>;
	/** The item's snapshot workflow definition (404 when it has none). */
	getItemWorkflow(id: string, signal?: AbortSignal): Promise<ItemWorkflowResponse>;
	getWorkflow(id: string, version: number, signal?: AbortSignal): Promise<WorkflowDefinitionResponse>;
	getHealth(signal?: AbortSignal): Promise<HealthResponse>;
}

export function createApi(options: ApiOptions = {}): Api {
	const base = (options.basePath ?? "/api").replace(/\/$/, "");
	const fetcher = options.fetcher ?? defaultFetcher;

	async function request<T>(path: string, query?: Record<string, string | number | boolean | undefined>, signal?: AbortSignal): Promise<T> {
		const search = new URLSearchParams();
		for (const [key, value] of Object.entries(query ?? {})) {
			if (value === undefined || value === "" || value === false) continue;
			search.set(key, String(value));
		}
		const suffix = search.size > 0 ? `?${search.toString()}` : "";
		// No Authorization header: the browser sends the HttpOnly session cookie for
		// same-origin `/api/*` requests, and JavaScript never sees the credential.
		const response = await fetcher(`${base}${path}${suffix}`, {
			method: "GET",
			headers: { accept: "application/json" },
			credentials: "same-origin",
			signal,
		});
		const text = await response.text();
		let body: unknown = null;
		try {
			body = text ? JSON.parse(text) : null;
		} catch {
			throw new ApiError({
				status: response.status,
				code: "invalid_response",
				message: `expected JSON from ${path}`,
			});
		}
		if (!response.ok || (body && typeof body === "object" && "ok" in body && body.ok === false)) {
			const failure = body as { error?: { code?: string; message?: string } } | null;
			throw new ApiError({
				status: response.status,
				code: failure?.error?.code ?? `http_${response.status}`,
				message: failure?.error?.message ?? `request failed with status ${response.status}`,
			});
		}
		return body as T;
	}

	return {
		getDashboard: (signal) => request<DashboardResponse>("/dashboard", undefined, signal),
		getItems: (query, signal) => request<ItemsResponse>("/items", query, signal),
		getItem: (id, signal) => request<ItemDetailResponse>(`/items/${encodeURIComponent(id)}`, undefined, signal),
		getEvents: (id, limit = 20, signal) =>
			request<EventsResponse>(`/items/${encodeURIComponent(id)}/events`, { limit }, signal),
		getItemWorkflow: (id, signal) =>
			request<ItemWorkflowResponse>(`/items/${encodeURIComponent(id)}/workflow`, undefined, signal),
		getWorkflow: (id, version, signal) =>
			request<WorkflowDefinitionResponse>(`/workflows/${encodeURIComponent(id)}`, { version }, signal),
		getHealth: (signal) => request<HealthResponse>("/health", undefined, signal),
	};
}
