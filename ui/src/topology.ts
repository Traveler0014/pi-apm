/**
 * Dependency topology for the read-only UI.
 *
 * Two edge kinds are kept apart on purpose (a hard product constraint):
 *
 *   - `dependency` (`blocked_by`): work order — B cannot start before A closes;
 *   - `parent` (`parent_id`): hierarchy — context, not order.
 *
 * Everything here is pure. The graph is defensive about cycles: a ledger may
 * contain a dependency loop (created before the CLI learned to reject it), and
 * the views must still render, without infinite recursion.
 */

import { groupItems, indexItems, type GroupKey } from "./groups";
import type { WorkItem } from "./types";

export type EdgeKind = "dependency" | "parent";

export interface GraphNode {
	item: WorkItem;
	/** Mutually exclusive headline group (groups.ts); drives the summary lists. */
	group: GroupKey;
	/** Why the node is in that group (tooltip / accessibility text). */
	explanation: string;
	/** Open = not a closed category (done/cancelled/aborted/custom closed). */
	open: boolean;
	/** Ids this node waits for, restricted to nodes present in the snapshot. */
	dependsOn: string[];
	/** Ids that wait for this node. */
	blockedDependents: string[];
	parentId: string | null;
	childIds: string[];
	/** True when the node takes part in a dependency cycle. */
	inCycle: boolean;
}

export interface GraphEdge {
	from: string;
	to: string;
	kind: EdgeKind;
}

export interface TopologyGraph {
	nodes: Map<string, GraphNode>;
	byIdentifier: Map<string, string>;
	edges: GraphEdge[];
	/** Ids that belong to at least one dependency cycle. */
	cycles: string[];
	/** Ids of *startable* nodes (open, unclaimed, healthy, no open blockers). */
	readyIds: string[];
	/** Ids of open nodes with at least one open blocker. */
	waitingIds: string[];
	/** Active/review or validly claimed. */
	inProgressIds: string[];
	/** Waiting for a human answer. */
	needsHumanIds: string[];
	/** Stale lease, failed recovery or an unexplained waiting state. */
	atRiskIds: string[];
}

const CLOSED = "closed";

export function isOpenItem(item: Pick<WorkItem, "status_category">): boolean {
	return item.status_category !== CLOSED;
}

/**
 * Build the graph from a full item snapshot (`/api/items?all=true`).
 *
 * Unknown references (an id not present in the snapshot, e.g. a different
 * project) are dropped rather than guessed at, so `dependsOn` only contains
 * nodes the UI can actually show.
 */
export function buildGraph(items: WorkItem[], now: number = Date.now()): TopologyGraph {
	const nodes = new Map<string, GraphNode>();
	const byIdentifier = new Map<string, string>();
	const grouped = groupItems(items, now, indexItems(items));
	for (const item of items) {
		const classification = grouped.classification.get(item.id);
		nodes.set(item.id, {
			item,
			group: classification?.group ?? "startable",
			explanation: classification?.explanation ?? "",
			open: isOpenItem(item),
			dependsOn: [],
			blockedDependents: [],
			parentId: item.parent_id ?? null,
			childIds: [],
			inCycle: false,
		});
		byIdentifier.set(item.identifier, item.id);
	}

	for (const node of nodes.values()) {
		const blockers = (node.item.blocked_by ?? [])
			.map((ref) => ref.id)
			.filter((id) => id !== node.item.id && nodes.has(id));
		node.dependsOn = [...new Set(blockers)];
		for (const blocker of node.dependsOn) nodes.get(blocker)!.blockedDependents.push(node.item.id);
		const parent = node.parentId;
		if (parent && parent !== node.item.id && nodes.has(parent)) nodes.get(parent)!.childIds.push(node.item.id);
	}

	const edges: GraphEdge[] = [];
	for (const node of nodes.values()) {
		for (const to of node.blockedDependents) edges.push({ from: node.item.id, to, kind: "dependency" });
		for (const child of node.childIds) edges.push({ from: node.item.id, to: child, kind: "parent" });
	}

	const cycles = findCycles(nodes);
	for (const id of cycles) {
		const node = nodes.get(id);
		if (node) node.inCycle = true;
	}

	const readyIds: string[] = [];
	const waitingIds: string[] = [];
	const inProgressIds: string[] = [];
	const needsHumanIds: string[] = [];
	const atRiskIds: string[] = [];
	for (const node of nodes.values()) {
		if (!node.open) continue;
		const openBlockerIds = node.dependsOn.filter((id) => nodes.get(id)!.open);
		const hasCycle = node.inCycle;
		switch (node.group) {
			case "in_progress":
				inProgressIds.push(node.item.id);
				break;
			case "needs_human":
				needsHumanIds.push(node.item.id);
				break;
			case "at_risk":
				atRiskIds.push(node.item.id);
				break;
			case "waiting_upstream":
				waitingIds.push(node.item.id);
				break;
			case "startable":
				// A node inside a cycle is never "ready": nothing can unblock it first.
				if (openBlockerIds.length === 0 && !hasCycle) readyIds.push(node.item.id);
				else waitingIds.push(node.item.id);
				break;
			default:
				break;
		}
	}

	return {
		nodes,
		byIdentifier,
		edges,
		cycles,
		readyIds,
		waitingIds,
		inProgressIds,
		needsHumanIds,
		atRiskIds,
	};
}

/**
 * Cycle detection over dependency edges.
 *
 * For each node, ask "can I reach myself?" with a bounded walk. A node on a loop
 * always can, and the `seen` set makes the walk terminate. O(V·(V+E)) is fine for
 * a local ledger (hundreds of items) and is much easier to verify than colour
 * marking with an explicit stack.
 */
export function findCycles(nodes: Map<string, GraphNode>): string[] {
	const cycles: string[] = [];
	for (const start of nodes.keys()) {
		const seen = new Set<string>();
		const stack = [...(nodes.get(start)?.dependsOn ?? [])];
		let onCycle = false;
		while (stack.length > 0) {
			const id = stack.pop()!;
			if (id === start) {
				onCycle = true;
				break;
			}
			if (seen.has(id)) continue;
			seen.add(id);
			stack.push(...(nodes.get(id)?.dependsOn ?? []));
		}
		if (onCycle) cycles.push(start);
	}
	return cycles.sort();
}

/** Direct blockers that are still open (what the user has to finish first). */
export function openBlockers(graph: TopologyGraph, id: string): WorkItem[] {
	const node = graph.nodes.get(id);
	if (!node) return [];
	return node.dependsOn.map((dep) => graph.nodes.get(dep)!).filter((dep) => dep.open).map((dep) => dep.item);
}

/** Direct dependents of `id` that are still open. */
export function openDependents(graph: TopologyGraph, id: string): WorkItem[] {
	const node = graph.nodes.get(id);
	if (!node) return [];
	return node.blockedDependents
		.map((dep) => graph.nodes.get(dep)!)
		.filter((dep) => dep.open)
		.map((dep) => dep.item);
}

/** Transitive closure over `next`, excluding the starting node, cycle-safe. */
function closure(start: string, next: (id: string) => string[]): Set<string> {
	const seen = new Set<string>();
	const stack = [...next(start)];
	while (stack.length > 0) {
		const id = stack.pop()!;
		if (id === start || seen.has(id)) continue;
		seen.add(id);
		for (const further of next(id)) if (!seen.has(further)) stack.push(further);
	}
	return seen;
}

/**
 * How many items would become unblocked if `id` closed: transitive dependents
 * that are currently waiting, directly or indirectly, on this node.
 */
export function unlockCount(graph: TopologyGraph, id: string): number {
	const dependents = closure(id, (nodeId) => graph.nodes.get(nodeId)?.blockedDependents ?? []);
	let count = 0;
	for (const dependent of dependents) {
		const node = graph.nodes.get(dependent);
		// A node inside a dependency cycle cannot be unlocked by closing one item,
		// so counting it would over-promise; the cycle itself is reported separately.
		if (!node?.open || node.inCycle) continue;
		if (node.dependsOn.includes(id) || node.dependsOn.some((dep) => dependents.has(dep))) count += 1;
	}
	return count;
}

/** Transitive blockers: everything upstream that is still open. */
export function upstreamChain(graph: TopologyGraph, id: string): { ids: Set<string>; open: WorkItem[] } {
	const ids = closure(id, (nodeId) => graph.nodes.get(nodeId)?.dependsOn ?? []);
	const open = [...ids]
		.map((nodeId) => graph.nodes.get(nodeId)!)
		.filter((node) => node.open)
		.map((node) => node.item)
		.sort(byIdentifier);
	return { ids, open };
}

function byIdentifier(a: WorkItem, b: WorkItem): number {
	return a.number - b.number;
}

export interface LocalDag {
	selectedId: string;
	nodes: Array<{
		item: WorkItem;
		depth: number;
		role: "upstream" | "selected" | "downstream" | "parent" | "child";
	}>;
	edges: GraphEdge[];
	upstream: WorkItem[];
	downstream: WorkItem[];
	parents: WorkItem[];
	children: WorkItem[];
}

/**
 * Neighbourhood of one item, limited to `hops` levels (default 2) so the view
 * stays local. Roles drive layout: upstream on the left, dependents on the
 * right, the selected item in the middle.
 *
 * Scoping (blocking chain / upstream / downstream) is applied afterwards by
 * `scopeDag` in `dag.ts`, so this function keeps a single responsibility.
 */
export type DagScope = "all" | "blocking" | "upstream" | "downstream";

export function localDag(graph: TopologyGraph, id: string, hops = 2): LocalDag {
	const node = graph.nodes.get(id);
	if (!node) return { selectedId: id, nodes: [], edges: [], upstream: [], downstream: [], parents: [], children: [] };

	const upstream: WorkItem[] = [];
	const downstream: WorkItem[] = [];
	const depths = new Map<string, { up: number; down: number }>();
	depths.set(id, { up: 0, down: 0 });

	const walk = (direction: "up" | "down"): void => {
		let frontier = [id];
		for (let depth = 1; depth <= hops; depth += 1) {
			const next: string[] = [];
			for (const current of frontier) {
				const currentNode = graph.nodes.get(current);
				if (!currentNode) continue;
				const neighbours = direction === "up" ? currentNode.dependsOn : currentNode.blockedDependents;
				for (const neighbour of neighbours) {
					const entry = depths.get(neighbour) ?? { up: Number.POSITIVE_INFINITY, down: Number.POSITIVE_INFINITY };
					const known = direction === "up" ? entry.up : entry.down;
					if (known <= depth) continue;
					if (direction === "up") entry.up = depth;
					else entry.down = depth;
					depths.set(neighbour, entry);
					next.push(neighbour);
					const item = graph.nodes.get(neighbour)!.item;
					const bucket = direction === "up" ? upstream : downstream;
					if (!bucket.some((row) => row.id === item.id)) bucket.push(item);
				}
			}
			frontier = next;
			if (frontier.length === 0) break;
		}
	};
	walk("up");
	walk("down");

	const parents = node.parentId && graph.nodes.has(node.parentId) ? [graph.nodes.get(node.parentId)!.item] : [];
	const children = node.childIds.map((childId) => graph.nodes.get(childId)!.item);

	const nodes: LocalDag["nodes"] = [];
	for (const [nodeId, depth] of depths) {
		const item = graph.nodes.get(nodeId)!.item;
		const role: LocalDag["nodes"][number]["role"] =
			nodeId === id
				? "selected"
				: parents.some((row) => row.id === nodeId)
					? "parent"
					: children.some((row) => row.id === nodeId)
						? "child"
						: upstream.some((row) => row.id === nodeId)
							? "upstream"
							: "downstream";
		nodes.push({ item, depth: role === "upstream" ? -depth.up : role === "downstream" ? depth.down : 0, role });
	}
	// Hierarchy neighbours are part of the local picture even when they are not on
	// a dependency path (parents above, children below).
	for (const parent of parents) {
		if (!nodes.some((entry) => entry.item.id === parent.id)) nodes.push({ item: parent, depth: 0, role: "parent" });
	}
	for (const child of children) {
		if (!nodes.some((entry) => entry.item.id === child.id)) nodes.push({ item: child, depth: 0, role: "child" });
	}
	nodes.sort((a, b) => a.depth - b.depth || byIdentifier(a.item, b.item));

	const included = new Set(nodes.map((entry) => entry.item.id));
	const edges = graph.edges.filter((edge) => included.has(edge.from) && included.has(edge.to));

	return { selectedId: id, nodes, edges, upstream, downstream, parents, children };
}

/**
 * Text equivalent of the local graph: one line per relation, ordered so a screen
 * reader hears "upstream → selected → downstream" like the picture reads.
 */
export function localDagText(dag: LocalDag): string[] {
	const lines: string[] = [];
	const describe = (item: WorkItem): string => `${item.identifier} (${item.status}, ${item.health})`;
	if (dag.upstream.length === 0) lines.push("Upstream: none (nothing blocks this item)");
	else lines.push(`Upstream: ${dag.upstream.map(describe).join("; ")}`);
	if (dag.downstream.length === 0) lines.push("Downstream: none (unlocking it unblocks nothing)");
	else lines.push(`Downstream: ${dag.downstream.map(describe).join("; ")}`);
	if (dag.parents.length > 0) lines.push(`Parent: ${dag.parents.map((item) => item.identifier).join(", ")}`);
	if (dag.children.length > 0) lines.push(`Children: ${dag.children.map(describe).join("; ")}`);
	return lines;
}

export interface TopologySummary {
	/** Aliases "startable": open, unclaimed, unblocked, healthy. */
	ready: WorkItem[];
	/** Active/review/claimed work, kept out of Ready to start. */
	inProgress: WorkItem[];
	/** Waiting on a human answer. */
	needsHuman: WorkItem[];
	/** Stale lease, failed recovery or an unexplained waiting state. */
	atRisk: WorkItem[];
	/** Unlock count per open item (how many transitive dependents it frees). */
	unlocks: Map<string, number>;
	waiting: Array<{ item: WorkItem; directBlockers: WorkItem[]; transitiveBlockers: number; unlocks: number }>;
	cycles: WorkItem[];
	closed: WorkItem[];
}

/**
 * The decision view: what can be started now, what is waiting, and for each
 * waiting item how deep the chain is and how much it would unlock.
 */
export function topologySummary(graph: TopologyGraph): TopologySummary {
	const ready = graph.readyIds
		.map((id) => graph.nodes.get(id)!.item)
		.sort(byIdentifier);
	const itemsFor = (ids: string[]): WorkItem[] =>
		ids.map((id) => graph.nodes.get(id)!.item).sort(byIdentifier);
	const inProgress = itemsFor(graph.inProgressIds);
	const needsHuman = itemsFor(graph.needsHumanIds);
	const atRisk = itemsFor(graph.atRiskIds);
	const unlocks = new Map<string, number>();
	for (const id of [...new Set([...graph.readyIds, ...graph.inProgressIds, ...graph.waitingIds])]) {
		unlocks.set(id, unlockCount(graph, id));
	}
	const waiting = graph.waitingIds
		.map((id) => {
			const chain = upstreamChain(graph, id);
			return {
				item: graph.nodes.get(id)!.item,
				directBlockers: openBlockers(graph, id),
				transitiveBlockers: chain.open.length,
				unlocks: unlockCount(graph, id),
			};
		})
		.sort((a, b) => b.unlocks - a.unlocks || a.item.number - b.item.number);
	const cycles = graph.cycles.map((id) => graph.nodes.get(id)!.item);
	const closed = [...graph.nodes.values()]
		.filter((node) => !node.open)
		.map((node) => node.item)
		.sort((a, b) => Date.parse(b.updated_at) - Date.parse(a.updated_at) || byIdentifier(a, b));
	return { ready, inProgress, needsHuman, atRisk, waiting, cycles, closed, unlocks };
}
