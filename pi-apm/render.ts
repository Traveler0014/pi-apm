/**
 * TUI rendering for the durable `apm-run-sync` entries written after every
 * settled run.
 *
 * `formatRunSyncReport` is pure (no pi-tui, no theme) and unit tested; the
 * renderer only maps its styled lines onto Pi's components. Long output is
 * truncated and every line is scrubbed of claim tokens.
 */

import { Box, Text } from "@earendil-works/pi-tui";
import {
	type RunSyncReportData,
	type StyledLine,
	formatRunSyncReport,
} from "./lib.ts";

/** Minimal theme surface the renderer needs (Pi's theme is structurally larger). */
export interface RenderTheme {
	fg(token: string, text: string): string;
	bg(token: string, text: string): string;
}

export interface EntryLike {
	data?: unknown;
}

/**
 * Render one `apm-run-sync` entry. Returns a Pi component (Box with Text lines),
 * never a bare string, and never throws on malformed data.
 */
export function renderRunSyncEntry(
	entry: EntryLike,
	options: { expanded?: boolean },
	theme: RenderTheme,
): Box {
	const report = (entry.data ?? {}) as Partial<RunSyncReportData>;
	let lines: StyledLine[];
	try {
		lines = formatRunSyncReport(report, { expanded: options?.expanded === true });
	} catch (error) {
		lines = [
			{ text: `[apm] run sync report could not be rendered: ${String(error)}`, tone: "error" },
		];
	}
	const box = new Box(1, 1, (text) => theme.bg("customMessageBg", text));
	for (const line of lines) {
		box.addChild(new Text(styleLine(line, theme), 0, 0));
	}
	return box;
}

function styleLine(line: StyledLine, theme: RenderTheme): string {
	switch (line.tone) {
		case "accent":
			return theme.fg("accent", line.text);
		case "success":
			return theme.fg("success", line.text);
		case "warning":
			return theme.fg("warning", line.text);
		case "error":
			return theme.fg("error", line.text);
		case "muted":
			return theme.fg("muted", line.text);
		default:
			return line.text;
	}
}
