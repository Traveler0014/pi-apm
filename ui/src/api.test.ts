import { describe, expect, it } from "vitest";
import {
	ApiError,
	TicketError,
	bootstrapTokenFromFragment,
	createApi,
	exchangeTicket,
	stripTicketFragment,
	ticketErrorMessage,
	ticketFromFragment,
} from "./api";
import type { Fetcher } from "./types";

interface Call {
	url: string;
	headers: Record<string, string>;
	credentials?: string;
}

function recordingFetcher(body: unknown, status = 200): { fetcher: Fetcher; calls: Call[] } {
	const calls: Call[] = [];
	const fetcher: Fetcher = async (url, init) => {
		calls.push({ url, headers: init.headers, credentials: init.credentials });
		return { ok: status >= 200 && status < 300, status, text: async () => JSON.stringify(body) };
	};
	return { fetcher, calls };
}

function fakeLocation(hash: string, pathname = "/ui/", search = "?mock=0"): Pick<Location, "hash" | "pathname" | "search"> {
	return { hash, pathname, search };
}

const okSession = { ok: true, token: null, token_type: "cookie", session: "cookie" };

describe("bootstrap ticket delivery via URL fragment", () => {

	it("exchanges #ticket=… for the real token and strips the fragment first", async () => {
		const replaced: string[] = [];
		const calls: Array<{
			url: string;
			method?: string;
			auth?: string;
			body?: string;
			cache?: string;
			credentials?: string;
		}> = [];
		const fetcher: Fetcher = async (url, init) => {
			calls.push({
				url,
				method: init.method,
				auth: init.headers.authorization,
				body: init.body,
				cache: init.cache,
				credentials: init.credentials,
			});
			return { ok: true, status: 200, text: async () => JSON.stringify(okSession) };
		};
		const found = await bootstrapTokenFromFragment(fakeLocation("#ticket=apm_boot_abc"), {
			replaceState: (_state, _title, url) => replaced.push(String(url)),
		}, { fetcher });
		expect(found).toBe(true);
		// Fragment removed before the network call: the ticket never survives a reload.
		expect(replaced).toEqual(["/ui/?mock=0"]);
		expect(calls).toHaveLength(1);
		expect(calls[0]).toMatchObject({
			url: "/api/bootstrap",
			method: "POST",
			auth: "Bearer apm_boot_abc",
			cache: "no-store",
			credentials: "same-origin",
		});
		expect(JSON.parse(calls[0]!.body!)).toEqual({ ticket: "apm_boot_abc" });
	});

	it("accepts ticket= anywhere in the fragment and keeps the other keys", async () => {
		const replaced: string[] = [];
		const fetcher: Fetcher = async () => ({ ok: true, status: 200, text: async () => JSON.stringify(okSession) });
		expect(
			await bootstrapTokenFromFragment(
				fakeLocation("#view=topology&item=APM-15&ticket=apm_boot_xyz"),
				{ replaceState: (_s, _t, url) => replaced.push(String(url)) },
				{ fetcher },
			),
		).toBe(true);
		// The ticket is gone; the view/item deep link survives so the page can use it.
		expect(replaced[0]).toBe("/ui/?mock=0#view=topology&item=APM-15");
		expect(replaced[0]).not.toContain("ticket");
	});

	it("leaves no fragment at all when the ticket was the only key", async () => {
		const replaced: string[] = [];
		const fetcher: Fetcher = async () => ({ ok: true, status: 200, text: async () => JSON.stringify(okSession) });
		await bootstrapTokenFromFragment(fakeLocation("#ticket=apm_boot_xyz"), { replaceState: (_s, _t, url) => replaced.push(String(url)) }, { fetcher });
		expect(replaced[0]).toBe("/ui/?mock=0");
	});

	it("does nothing without a ticket (and never treats a legacy token as one)", async () => {
		let called = 0;
		const fetcher: Fetcher = async () => {
			called += 1;
			return { ok: true, status: 200, text: async () => JSON.stringify(okSession) };
		};
		expect(await bootstrapTokenFromFragment(fakeLocation(""), { replaceState: () => {} }, { fetcher })).toBe(false);
		expect(await bootstrapTokenFromFragment(fakeLocation("#item=APM-15"), { replaceState: () => {} }, { fetcher })).toBe(false);
		expect(await bootstrapTokenFromFragment(fakeLocation("#token=apm_ui_legacy"), { replaceState: () => {} }, { fetcher })).toBe(false);
		expect(called).toBe(0);
	});

	it("maps each ticket failure code to its own message and never retries", async () => {
		const cases: Array<[number, string]> = [
			[401, "invalid_ticket"],
			[401, "ticket_expired"],
			[409, "ticket_consumed"],
		];
		for (const [status, code] of cases) {
			let calls = 0;
			const fetcher: Fetcher = async () => {
				calls += 1;
				return { ok: false, status, text: async () => JSON.stringify({ ok: false, error: { code, message: code } }) };
			};
			await expect(
				bootstrapTokenFromFragment(fakeLocation("#ticket=apm_boot_x"), { replaceState: () => {} }, { fetcher }),
			).rejects.toMatchObject({ name: "TicketError", code, status });
			expect(calls).toBe(1);
		}
		expect(ticketErrorMessage(new TicketError("ticket_consumed", 409, "x"))).toContain("already used");
		expect(ticketErrorMessage(new TicketError("ticket_expired", 401, "x"))).toContain("expired");
		expect(ticketErrorMessage(new TicketError("invalid_ticket", 401, "x"))).toContain("not valid");
		expect(ticketErrorMessage(new Error("network down"))).toContain("network down");
	});

	it("refuses a bootstrap response that leaks a token", async () => {
		// A cookie session must never hand the credential to JavaScript.
		const fetcher: Fetcher = async () => ({
			ok: true,
			status: 200,
			text: async () => JSON.stringify({ ok: true, token: "apm_ui_leak" }),
		});
		await expect(exchangeTicket("apm_boot_x", fetcher)).rejects.toMatchObject({ code: "invalid_ticket" });
	});

	it("accepts a cookie-session response with a null token", async () => {
		const fetcher: Fetcher = async () => ({
			ok: true,
			status: 200,
			text: async () => JSON.stringify(okSession),
		});
		await expect(exchangeTicket("apm_boot_x", fetcher)).resolves.toBe(true);
	});
});

describe("createApi", () => {
	it("never sends an Authorization header: the session is an HttpOnly cookie", async () => {
		const { fetcher, calls } = recordingFetcher({ ok: true, dashboard: {} });
		const api = createApi({ fetcher });
		await api.getDashboard();
		await api.getHealth();
		for (const call of calls) {
			expect(call.headers.authorization).toBeUndefined();
			expect(call.credentials).toBe("same-origin");
		}
	});

	it("uses absolute /api paths and encodes the item id", async () => {
		const { fetcher, calls } = recordingFetcher({ ok: true, item: {} });
		const api = createApi({ fetcher });
		await api.getItem("APM-15");
		await api.getEvents("APM 15/16", 5);
		expect(calls[0]!.url).toBe("/api/items/APM-15");
		expect(calls[1]!.url).toBe("/api/items/APM%2015%2F16/events?limit=5");
	});

	it("builds query strings and skips empty/false values", async () => {
		const { fetcher, calls } = recordingFetcher({ ok: true, items: [], count: 0 });
		const api = createApi({ fetcher });
		await api.getItems({ all: true, limit: 50, status: "", attention: false, keywords: undefined });
		expect(calls[0]!.url).toBe("/api/items?all=true&limit=50");
	});

	it("maps a domain error envelope to ApiError with the CLI code", async () => {
		const { fetcher } = recordingFetcher(
			{ ok: false, error: { code: "version_conflict", message: "expected 3, current 4", exit_code: 4 } },
			409,
		);
		const api = createApi({ fetcher });
		await expect(api.getItem("APM-15")).rejects.toMatchObject({
			name: "ApiError",
			status: 409,
			code: "version_conflict",
			message: "expected 3, current 4",
		});
	});

	it("maps a non-JSON response to invalid_response", async () => {
		const fetcher: Fetcher = async () => ({ ok: true, status: 200, text: async () => "<html>oops</html>" });
		const api = createApi({ fetcher });
		await expect(api.getDashboard()).rejects.toBeInstanceOf(ApiError);
		await expect(api.getDashboard()).rejects.toMatchObject({ code: "invalid_response" });
	});

	it("does not keep any token state after the exchange", async () => {
		const fetcher: Fetcher = async () => ({ ok: true, status: 200, text: async () => JSON.stringify(okSession) });
		await bootstrapTokenFromFragment(fakeLocation("#ticket=apm_boot_t"), { replaceState: () => {} }, { fetcher });
		// Nothing credential-shaped exists on the module any more.
		const api = createApi({ fetcher });
		expect(Object.keys(api).sort()).toEqual([
			"getDashboard",
			"getEvents",
			"getHealth",
			"getItem",
			"getItemWorkflow",
			"getItems",
			"getWorkflow",
		]);
	});
});

describe("ticket helpers (cookie sessions)", () => {
	it("reads the ticket without consuming it", () => {
		expect(ticketFromFragment({ hash: "#ticket=apm_boot_abc" })).toBe("apm_boot_abc");
		expect(ticketFromFragment({ hash: "#view=topology&ticket=apm_boot_abc" })).toBe("apm_boot_abc");
		expect(ticketFromFragment({ hash: "#view=topology" })).toBeNull();
		expect(ticketFromFragment({ hash: "" })).toBeNull();
	});

	it("strips only the ticket, keeping view/item deep links", () => {
		const urls: string[] = [];
		const stripped = stripTicketFragment(
			{ hash: "#view=topology&item=APM-15&ticket=apm_boot_abc", pathname: "/ui/", search: "?mock=0" },
			{ replaceState: (_s, _t, url) => urls.push(String(url)) },
		);
		expect(stripped).toBe(true);
		expect(urls[0]).toBe("/ui/?mock=0#view=topology&item=APM-15");
		expect(urls[0]).not.toContain("ticket");
	});

	it("does nothing when there is no ticket to strip", () => {
		const urls: string[] = [];
		expect(stripTicketFragment({ hash: "#item=APM-15", pathname: "/ui/", search: "" }, { replaceState: (_s, _t, url) => urls.push(String(url)) })).toBe(false);
		expect(urls).toEqual([]);
	});
});
