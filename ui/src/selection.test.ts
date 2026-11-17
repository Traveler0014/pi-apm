import { describe, expect, it } from "vitest";
import { cacheIsFresh, findByReference, pruneCache, reconcileSelection } from "./selection";

const items = [{ id: "a" }, { id: "b" }];

describe("reconcileSelection", () => {
	it("keeps the selection across a refresh, even when ordering changes", () => {
		expect(reconcileSelection("b", [items[1]!, items[0]!])).toEqual({ selectedId: "b", dropped: false });
	});

	it("keeps a selection that the current filters hide", () => {
		// The snapshot is the whole ledger; filtering happens later, so a hidden
		// item is still selected.
		expect(reconcileSelection("a", items)).toEqual({ selectedId: "a", dropped: false });
	});

	it("drops the selection when the item really disappears", () => {
		expect(reconcileSelection("gone", items)).toEqual({ selectedId: null, dropped: true });
	});

	it("is a no-op without a selection", () => {
		expect(reconcileSelection(null, items)).toEqual({ selectedId: null, dropped: false });
	});
});

describe("pruneCache", () => {
	it("drops cached details for vanished items only", () => {
		const cache = new Map([["a", 1], ["gone", 2]]);
		expect(pruneCache(cache, items)).toEqual(["gone"]);
		expect([...cache.keys()]).toEqual(["a"]);
	});
});

describe("cacheIsFresh", () => {
	it("invalidates a cached detail when the item version moves", () => {
		expect(cacheIsFresh({ version: 3 }, { version: 3 })).toBe(true);
		expect(cacheIsFresh({ version: 3 }, { version: 4 })).toBe(false);
		expect(cacheIsFresh(undefined, { version: 3 })).toBe(false);
	});
});

describe("findByReference", () => {
	const items = [
		{ id: "uuid-1", identifier: "APM-1" },
		{ id: "uuid-2", identifier: "APM-2" },
	];

	it("resolves identifiers and ids", () => {
		expect(findByReference(items, "APM-2")).toEqual(items[1]);
		expect(findByReference(items, "uuid-1")).toEqual(items[0]);
	});

	it("returns null for unknown or empty references", () => {
		expect(findByReference(items, "APM-404")).toBeNull();
		expect(findByReference(items, "   ")).toBeNull();
	});
});
