// @vitest-environment happy-dom
/**
 * Focus restoration must survive arbitrary ledger data (APM-55): the selector is
 * built from `data-*` values, so an id containing `"` or `\` has to be escaped
 * instead of producing an invalid selector or matching the wrong element.
 */

import { describe, expect, it } from "vitest";
import { escapeAttributeValueFallback, focusSelector } from "./focus";

describe("focusSelector", () => {
	it("returns null when nothing was focused", () => {
		expect(focusSelector(null)).toBeNull();
		expect(focusSelector({})).toBeNull();
	});

	it("prefers the same precedence the old inline builder had", () => {
		expect(focusSelector({ bucket: "ready_unclaimed", view: "list" })).toBe('[data-bucket="ready_unclaimed"]');
		expect(focusSelector({ view: "topology" })).toBe('[data-view="topology"]');
		expect(focusSelector({ relation: "dependencies" })).toBe('.dag-panel button[data-relation="dependencies"]');
		expect(focusSelector({ scope: "upstream" })).toBe('.dag-panel .dag-scope[data-scope="upstream"]');
		expect(focusSelector({ dagControl: "zoom-in" })).toBe(".dag-panel .dag-zoom-in");
		expect(focusSelector({ itemId: "id-1" })).toBe('[data-item-id="id-1"]');
		expect(focusSelector({ itemId: "id-1", action: "expand" })).toBe('[data-item-id="id-1"] [data-action="expand"]');
	});

	it("routes every attribute value through the injected escaper", () => {
		const escape = (value: string): string => `<<${value}>>`;
		expect(focusSelector({ itemId: "a", action: "b" }, escape)).toBe('[data-item-id="<<a>>"] [data-action="<<b>>"]');
		expect(focusSelector({ bucket: "bucket" }, escape)).toBe('[data-bucket="<<bucket>>"]');
	});

	it("still finds the element when the id contains selector-hostile characters", () => {
		document.body.replaceChildren();
		const row = document.createElement("div");
		row.dataset.itemId = "weird]id";
		const button = document.createElement("button");
		button.dataset.action = "do]it";
		row.append(button);
		document.body.append(row);

		const selector = focusSelector({ itemId: row.dataset.itemId, action: button.dataset.action });
		expect(selector).not.toBeNull();
		expect(document.querySelector(selector!)).toBe(button);
	});
});

describe("escapeAttributeValueFallback", () => {
	it("escapes only the characters that need it inside quoted selectors", () => {
		expect(escapeAttributeValueFallback('a"b')).toBe('a\\"b');
		expect(escapeAttributeValueFallback("a\\b")).toBe("a\\\\b");
		expect(escapeAttributeValueFallback("line\nbreak")).toBe("line\\\nbreak");
		expect(escapeAttributeValueFallback("plain-id_1.2")).toBe("plain-id_1.2");
	});
});
