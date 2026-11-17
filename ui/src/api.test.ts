import { describe, expect, it } from "vitest";
import {
	ApiError,
	TicketError,
	bootstrapTokenFromFragment,
	createApi,
	exchangeTicket,
	hasToken,
	setToken,
	ticketErrorMessage,
} from "./api";
import type { Fetcher } from "./types";

interface Call {
	url: string;
	headers: Record<string, string>;
}

function recordingFetcher(body: unknown, status = 200): { fetcher: Fetcher; calls: Call[] } {
	const calls: Call[] = [];
	const fetcher: Fetcher = async (url, init) => {
		calls.push({ url, headers: init.headers });
		return { ok: status >= 200 && status < 300, status, text: async () => JSON.stringify(body) };
	};
	return { fetcher, calls };
}

function fakeLocation(hash: string, pathname = "/ui/", search = "?mock=0"): Pick<Location, "hash" | "pathname" | "search"> {
	return { hash, pathname, search };
}

describe("bootstrap ticket delivery via URL fragment", () => {
	const okToken = { ok: true, token: "apm_ui_real", token_type: "Bearer" };

	it("exchanges #ticket=… for the real token and strips the fragment first", async () => {
		setToken(null);
		const replaced: string[] = [];
		const calls: Array<{ url: string; method?: string; auth?: string; body?: string; cache?: string }> = [];
		const fetcher: Fetcher = async (url, init) => {
			calls.push({ url, method: init.method, auth: init.headers.authorization, body: init.body, cache: init.cache });
			return { ok: true, status: 200, text: async () => JSON.stringify(okToken) };
		};
		const found = await bootstrapTokenFromFragment(fakeLocation("#ticket=apm_boot_abc"), {
			replaceState: (_state, _title, url) => replaced.push(String(url)),
		}, { fetcher });
		expect(found).toBe(true);
		expect(hasToken()).toBe(true);
		// Fragment removed before the network call: the ticket never survives a reload.
		expect(replaced).toEqual(["/ui/?mock=0"]);
		expect(calls).toHaveLength(1);
		expect(calls[0]).toMatchObject({
			url: "/api/bootstrap",
			method: "POST",
			auth: "Bearer apm_boot_abc",
			cache: "no-store",
		});
		expect(JSON.parse(calls[0]!.body!)).toEqual({ ticket: "apm_boot_abc" });
	});

	it("accepts ticket= anywhere in the fragment and keeps the other keys", async () => {
		setToken(null);
		const replaced: string[] = [];
		const fetcher: Fetcher = async () => ({ ok: true, status: 200, text: async () => JSON.stringify(okToken) });
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
		setToken(null);
		const replaced: string[] = [];
		const fetcher: Fetcher = async () => ({ ok: true, status: 200, text: async () => JSON.stringify(okToken) });
		await bootstrapTokenFromFragment(fakeLocation("#ticket=apm_boot_xyz"), { replaceState: (_s, _t, url) => replaced.push(String(url)) }, { fetcher });
		expect(replaced[0]).toBe("/ui/?mock=0");
	});

	it("does nothing without a ticket (and never treats a legacy token as one)", async () => {
		setToken(null);
		let called = 0;
		const fetcher: Fetcher = async () => {
			called += 1;
			return { ok: true, status: 200, text: async () => JSON.stringify(okToken) };
		};
		expect(await bootstrapTokenFromFragment(fakeLocation(""), { replaceState: () => {} }, { fetcher })).toBe(false);
		expect(await bootstrapTokenFromFragment(fakeLocation("#item=APM-15"), { replaceState: () => {} }, { fetcher })).toBe(false);
		expect(await bootstrapTokenFromFragment(fakeLocation("#token=apm_ui_legacy"), { replaceState: () => {} }, { fetcher })).toBe(false);
		expect(called).toBe(0);
		expect(hasToken()).toBe(false);
	});

	it("maps each ticket failure code to its own message and never retries", async () => {
		const cases: Array<[number, string]> = [
			[401, "invalid_ticket"],
			[401, "ticket_expired"],
			[409, "ticket_consumed"],
		];
		for (const [status, code] of cases) {
			setToken(null);
			let calls = 0;
			const fetcher: Fetcher = async () => {
				calls += 1;
				return { ok: false, status, text: async () => JSON.stringify({ ok: false, error: { code, message: code } }) };
			};
			await expect(
				bootstrapTokenFromFragment(fakeLocation("#ticket=apm_boot_x"), { replaceState: () => {} }, { fetcher }),
			).rejects.toMatchObject({ name: "TicketError", code, status });
			expect(calls).toBe(1);
			expect(hasToken()).toBe(false);
		}
		expect(ticketErrorMessage(new TicketError("ticket_consumed", 409, "x"))).toContain("already used");
		expect(ticketErrorMessage(new TicketError("ticket_expired", 401, "x"))).toContain("expired");
		expect(ticketErrorMessage(new TicketError("invalid_ticket", 401, "x"))).toContain("not valid");
		expect(ticketErrorMessage(new Error("network down"))).toContain("network down");
	});

	it("rejects a bootstrap response without a token", async () => {
		setToken(null);
		const fetcher: Fetcher = async () => ({ ok: true, status: 200, text: async () => JSON.stringify({ ok: true }) });
		await expect(exchangeTicket("apm_boot_x", fetcher)).rejects.toMatchObject({ code: "invalid_ticket" });
	});
});

describe("createApi", () => {
	it("sends the bearer token on API calls but not on /health", async () => {
		setToken("apm_ui_secret");
		const { fetcher, calls } = recordingFetcher({ ok: true, dashboard: {} });
		const api = createApi({ fetcher });
		await api.getDashboard();
		await api.getHealth();
		expect(calls[0]!.headers.authorization).toBe("Bearer apm_ui_secret");
		expect(calls[1]!.headers.authorization).toBeUndefined();
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

	it("notifies a token listener after the ticket is exchanged", async () => {
		setToken(null);
		const seen: boolean[] = [];
		createApi({ fetcher: recordingFetcher({ ok: true }).fetcher, onToken: (present) => seen.push(present) });
		const fetcher: Fetcher = async () => ({ ok: true, status: 200, text: async () => JSON.stringify({ ok: true, token: "apm_ui_t" }) });
		await bootstrapTokenFromFragment(fakeLocation("#ticket=apm_boot_t"), { replaceState: () => {} }, { fetcher });
		expect(seen).toEqual([false, true]);
	});
});
