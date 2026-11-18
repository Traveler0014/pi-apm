/**
 * Title measurement and wrapping shared by the layout and the rendered nodes.
 *
 * Titles come from a bilingual ledger, so width cannot be `length * k` and the
 * line break cannot be "split on spaces": a Chinese title has no spaces at all,
 * and one CJK glyph is roughly twice as wide as a Latin letter. Both functions
 * below therefore work in *width units* (Latin/grapheme ≈ 1, CJK/fullwidth ≈ 2)
 * and break CJK text between grapheme clusters (word boundaries are optional in
 * Chinese, Japanese and Korean), while Latin text keeps whole words.
 */

/** Extra width budget for the node padding inside NODE_W (see layout.ts). */
const ELLIPSIS = "…";

/**
 * Width budget of one title line, in Unicode width units (a CJK grapheme is 2).
 * Kept in sync with NODE_W in layout.ts: 208px − 24px padding leaves 184px, and
 * a Latin letter is measured at ~7px per unit.
 */
export const MAX_LINE_UNITS = 26;
/** Pixels per width unit used by the width estimate. */
export const UNIT_PX = 7;
/** Horizontal padding inside a node box (border box − content box). */
export const NODE_PADDING_X = 24;

/** True for characters that occupy roughly two Latin advance widths. */
function isWide(codePoint: number): boolean {
	return (
		(codePoint >= 0x1100 && codePoint <= 0x115f) || // Hangul Jamo
		(codePoint >= 0x2e80 && codePoint <= 0x303e) || // CJK radicals, punctuation
		(codePoint >= 0x3041 && codePoint <= 0x33ff) || // Hiragana, Katakana, CJK compat
		(codePoint >= 0x3400 && codePoint <= 0x4dbf) || // CJK ext A
		(codePoint >= 0x4e00 && codePoint <= 0x9fff) || // CJK unified
		(codePoint >= 0xa000 && codePoint <= 0xa4cf) || // Yi
		(codePoint >= 0xac00 && codePoint <= 0xd7a3) || // Hangul syllables
		(codePoint >= 0xf900 && codePoint <= 0xfaff) || // CJK compat ideographs
		(codePoint >= 0xfe30 && codePoint <= 0xfe6f) || // CJK compat forms
		(codePoint >= 0xff00 && codePoint <= 0xff60) || // Fullwidth forms
		(codePoint >= 0xffe0 && codePoint <= 0xffe6) ||
		(codePoint >= 0x20000 && codePoint <= 0x3fffd) // CJK ext B+
	);
}

/** Split into user-perceived characters, using Intl.Segmenter when present. */
export function graphemes(text: string): string[] {
	const Segmenter = (Intl as unknown as { Segmenter?: new (locale?: string, options?: { granularity: string }) => { segment: (input: string) => Iterable<{ segment: string }> } }).Segmenter;
	if (Segmenter) {
		try {
			const segmenter = new Segmenter(undefined, { granularity: "grapheme" });
			return [...segmenter.segment(text)].map((part) => part.segment);
		} catch {
			// Fall through to the code-point split.
		}
	}
	return [...text];
}

/** Width of one grapheme in Latin-letter units (CJK and fullwidth are 2). */
export function graphemeWidth(grapheme: string): number {
	const codePoint = grapheme.codePointAt(0) ?? 0;
	return isWide(codePoint) ? 2 : 1;
}

/** Estimated width of a title in Latin-letter units. */
export function titleUnits(text: string): number {
	let units = 0;
	for (const grapheme of graphemes(text)) units += graphemeWidth(grapheme);
	return units;
}

/** Is this grapheme a break-anywhere character (CJK, no spaces needed)? */
function isBreakable(grapheme: string): boolean {
	const codePoint = grapheme.codePointAt(0) ?? 0;
	return isWide(codePoint) && !/^[\u3000-\u303e\uff00-\uff60]$/.test(grapheme);
}

/** Tokens to lay out: Latin words stay whole, CJK breaks per character. */
function tokens(text: string): string[] {
	const parts: string[] = [];
	let word = "";
	const flush = (): void => {
		if (word) parts.push(word);
		word = "";
	};
	for (const grapheme of graphemes(text)) {
		if (/\s/.test(grapheme)) {
			flush();
			continue;
		}
		if (isBreakable(grapheme)) {
			flush();
			parts.push(grapheme);
			continue;
		}
		word += grapheme;
	}
	flush();
	return parts;
}

/**
 * Split a title into at most `maxLines` lines of at most `maxUnits` width units.
 *
 * - Latin words are never cut mid-word (they only overflow when a single word is
 *   wider than the line, which the CSS ellipsis then handles);
 * - CJK text breaks between grapheme clusters, so a Chinese title uses both
 *   lines instead of one overflowing one;
 * - a truncated title ends with `…`.
 */
export function wrapTitle(title: string, maxChars = 26, maxLines = 2): string[] {
	const trimmed = title.trim();
	if (!trimmed) return [""];
	const maxUnits = maxChars;
	const lines: string[] = [];
	let current = "";
	let truncated = false;
	const push = (): void => {
		lines.push(current);
		current = "";
	};
	for (const token of tokens(trimmed)) {
		const separator = current && !isBreakable(token) ? " " : "";
		const candidate = `${current}${separator}${token}`;
		if (titleUnits(candidate) <= maxUnits || current === "") {
			current = candidate;
			continue;
		}
		push();
		if (lines.length === maxLines) {
			truncated = true;
			break;
		}
		current = token;
	}
	if (!truncated && current) {
		if (lines.length === maxLines) truncated = true;
		else lines.push(current);
	}
	if (truncated) {
		const last = lines[maxLines - 1] ?? "";
		lines[maxLines - 1] = shorten(last, maxUnits);
	}
	return lines.slice(0, maxLines);
}

/** Fit `text` inside `maxUnits`, ending with an ellipsis when something is cut. */
function shorten(text: string, maxUnits: number): string {
	if (titleUnits(text) <= maxUnits) return text;
	const budget = maxUnits - 1; // room for the ellipsis
	let out = "";
	for (const grapheme of graphemes(text)) {
		if (titleUnits(out + grapheme) > budget) break;
		out += grapheme;
	}
	return `${out.trimEnd()}${ELLIPSIS}`;
}
