/**
 * API client for the embedded dashboard.
 *
 * Same-origin, absolute `/api/*` paths (the page is served from `/ui/`).
 *
 * Authentication is a two-step hand-off: `apm ui` prints a URL whose fragment
 * carries a **one-time bootstrap ticket** (short TTL), the page erases the
 * fragment immediately and swaps the ticket for the real API token with one
 * `POST /api/bootstrap`. The token then lives only in this module's memory — never
 * in localStorage, sessionStorage, a cookie, the DOM, the URL or a log line.
 */

import type {
	DashboardResponse,
	EventsResponse,
	Fetcher,
	HealthResponse,
	ItemDetailResponse,
	ItemsResponse,
} from "./types";

export interface ApiOptions {
	basePath?: string;
	fetcher?: Fetcher;
	onToken?: (present: boolean) => void;
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

let token: string | null = null;
let tokenListener: ((present: boolean) => void) | null = null;

/** In-memory only: no storage, no DOM. */
export function setToken(value: string | null): void {
	token = value && value.trim() ? value.trim() : null;
	tokenListener?.(token !== null);
}

export function hasToken(): boolean {
	return token !== null;
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
): Promise<string> {
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
			authorization: `Bearer ${value}`,
		},
		body: JSON.stringify({ ticket: value }),
		cache: "no-store",
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
	const tokenValue = (body as { token?: unknown } | null)?.token;
	if (typeof tokenValue !== "string" || !tokenValue.trim()) {
		throw new TicketError("invalid_ticket", response.status, "bootstrap response had no token");
	}
	setToken(tokenValue);
	return tokenValue;
}

/**
 * Read `#ticket=…` (also `…&ticket=…`) from the fragment, erase the fragment so
 * the ticket never reaches history/referrers, then exchange it for the token.
 *
 * Returns true when a ticket was present and exchanged.
 */
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
	// Erase first: even a failed exchange must not leave the ticket in the URL bar.
	history.replaceState(null, "", `${location.pathname}${location.search}`);
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
		credentials: "omit",
	});

export interface Api {
	getDashboard(signal?: AbortSignal): Promise<DashboardResponse>;
	getItems(query?: Record<string, string | number | boolean | undefined>, signal?: AbortSignal): Promise<ItemsResponse>;
	getItem(id: string, signal?: AbortSignal): Promise<ItemDetailResponse>;
	getEvents(id: string, limit?: number, signal?: AbortSignal): Promise<EventsResponse>;
	getHealth(signal?: AbortSignal): Promise<HealthResponse>;
	hasToken(): boolean;
}

export function createApi(options: ApiOptions = {}): Api {
	const base = (options.basePath ?? "/api").replace(/\/$/, "");
	const fetcher = options.fetcher ?? defaultFetcher;
	tokenListener = options.onToken ?? null;
	if (options.onToken) options.onToken(token !== null);

	async function request<T>(path: string, query?: Record<string, string | number | boolean | undefined>, signal?: AbortSignal): Promise<T> {
		const search = new URLSearchParams();
		for (const [key, value] of Object.entries(query ?? {})) {
			if (value === undefined || value === "" || value === false) continue;
			search.set(key, String(value));
		}
		const suffix = search.size > 0 ? `?${search.toString()}` : "";
		const headers: Record<string, string> = { accept: "application/json" };
		if (token && path !== "/health") {
			headers.authorization = `Bearer ${token}`;
		}
		const response = await fetcher(`${base}${path}${suffix}`, { method: "GET", headers, signal });
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
		getHealth: (signal) => request<HealthResponse>("/health", undefined, signal),
		hasToken,
	};
}
