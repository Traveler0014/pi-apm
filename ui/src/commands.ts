/**
 * CLI commands offered by the read-only detail view.
 *
 * The dashboard never writes: it shows what to type so a human can continue in a
 * terminal. Commands are pure strings built from the item identifier — no token,
 * no session id, no run id ever appears in them (the CLI resolves those from the
 * 0600 session file).
 */

import type { WorkItem } from "./types";

export interface CommandSpec {
	/** What the command is for. */
	label: string;
	/** The exact text that lands on the clipboard. */
	command: string;
	/** One line of context (why it is safe / what it does). */
	hint: string;
}

export function cliCommands(item: Pick<WorkItem, "identifier">): CommandSpec[] {
	const id = shellQuote(item.identifier);
	return [
		{
			label: "Read the item",
			command: `apm get ${id} --json`,
			hint: "Same payload this drawer renders.",
		},
		{
			label: "Claim it",
			command: `apm claim ${id}`,
			hint: "Takes the lease before you write; fails if another agent holds it.",
		},
		{
			label: "Record progress",
			command: `apm progress ${id} --summary "<what changed>" --next-action "<next step>"`,
			hint: "Replace the placeholders; one write per meaningful unit of work.",
		},
		{
			label: "Change status",
			command: `apm status ${id} --to doing`,
			hint: "The legal target states come from the item's workflow.",
		},
		{
			label: "Read the handover",
			command: `apm context show ${id}`,
			hint: "Text form of the goal/current-state block above.",
		},
		{
			label: "Recent events",
			command: `apm events ${id} --limit 20 --json`,
			hint: "Audit trail: claims, progress, status changes.",
		},
	];
}

/** Quote an identifier only when it needs it (identifiers are slugs in practice). */
export function shellQuote(value: string): string {
	if (/^[A-Za-z0-9._:-]+$/.test(value)) return value;
	return `'${value.replace(/'/g, `'\\''`)}'`;
}
