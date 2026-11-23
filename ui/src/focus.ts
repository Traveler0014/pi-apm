/**
 * Focus restoration across re-renders.
 *
 * The dashboard rebuilds its DOM on every 4-second refresh, so a keyboard user's
 * focus has to be re-applied by data attribute. Attribute values come from the
 * ledger (item ids are UUIDs, but action/bucket/view values are only stable by
 * convention), so they must be escaped before they are spliced into a selector:
 * an id containing `"` or `\` would otherwise produce an invalid selector (a
 * `DOMException`) or match something unintended.
 */

/** Fit / zoom / expand live inside the graph, which is rebuilt each render. */
export type DagControl = "fit" | "zoom-in" | "zoom-out" | "reset" | "expand";

export interface FocusSnapshot {
	itemId?: string;
	action?: string;
	bucket?: string;
	view?: string;
	relation?: string;
	scope?: string;
	dagControl?: DagControl;
}

export type AttributeEscaper = (value: string) => string;

/**
 * Escape one attribute value for a double-quoted selector.
 *
 * `CSS.escape` is the correct tool and is used whenever the platform provides it
 * (every real browser). The fallback covers older engines and non-DOM test
 * environments: inside quotes only a backslash, a double quote and line breaks
 * need escaping.
 */
export function escapeAttributeValue(value: string): string {
	const css = (globalThis as { CSS?: { escape?: (input: string) => string } }).CSS;
	if (css && typeof css.escape === "function") return css.escape(value);
	return escapeAttributeValueFallback(value);
}

/** Platform-free escaping used when `CSS.escape` is unavailable. */
export function escapeAttributeValueFallback(value: string): string {
	return value.replace(/[\\"\n\r\f]/g, (char) => `\\${char}`);
}

/** Build the selector for a saved focus target, or `null` when nothing was focused. */
export function focusSelector(saved: FocusSnapshot | null, escape: AttributeEscaper = escapeAttributeValue): string | null {
	if (!saved) return null;
	const value = (raw: string): string => `"${escape(raw)}"`;
	if (saved.bucket) return `[data-bucket=${value(saved.bucket)}]`;
	if (saved.view) return `[data-view=${value(saved.view)}]`;
	if (saved.relation) return `.dag-panel button[data-relation=${value(saved.relation)}]`;
	if (saved.scope) return `.dag-panel .dag-scope[data-scope=${value(saved.scope)}]`;
	if (saved.dagControl) return `.dag-panel .dag-${saved.dagControl}`;
	if (saved.itemId) {
		const action = saved.action ? ` [data-action=${value(saved.action)}]` : "";
		return `[data-item-id=${value(saved.itemId)}]${action}`;
	}
	return null;
}
