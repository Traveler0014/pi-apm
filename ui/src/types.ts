/** Wire types for the read-only AgentPM API (`/api/*`, same shapes as the REST adapter). */

export type BucketKey =
	| "waiting_human"
	| "in_progress"
	| "stalled_no_progress"
	| "lost_lease"
	| "recovery_failed"
	| "waiting_dependencies"
	| "ready_unclaimed"
	| "recently_done";

export interface ClaimRef {
	id: string;
	holder: string;
	holder_kind: string;
	session_id?: string | null;
	run_id?: string | null;
	status: string;
	leased_at?: string;
	heartbeat_at?: string;
	expires_at: string;
	lease_seconds?: number;
	expired?: boolean;
	token_prefix?: string;
}

export interface ItemRef {
	id: string;
	identifier: string;
	title: string;
	status: string;
}

export interface AttentionBrief {
	id: string;
	question: string;
	status: string;
	blocking: boolean;
}

export interface WorkItem {
	id: string;
	project: string;
	project_id: string;
	number: number;
	identifier: string;
	title: string;
	item_type: string;
	status: string;
	status_category: string;
	workflow_id?: string;
	workflow_version?: number;
	priority: string;
	parent_id?: string | null;
	parent_identifier?: string | null;
	version: number;
	context_version: number;
	progress_summary?: string | null;
	next_action?: string | null;
	blocker?: string | null;
	waiting_on_human: boolean;
	waiting_reason?: string | null;
	completion_note?: string | null;
	created_by: string;
	created_at: string;
	updated_at: string;
	last_heartbeat_at?: string | null;
	last_progress_at?: string | null;
	last_run_sync_at?: string | null;
	done_at?: string | null;
	cancelled_at?: string | null;
	health: string;
	claim?: ClaimRef | null;
	labels: string[];
	blocked_by: ItemRef[];
	blocks: ItemRef[];
	attention: AttentionBrief[];
	seconds_since_progress?: number | null;
}

export interface Settings {
	lease_seconds: number;
	stale_after_seconds: number;
	no_progress_after_seconds: number;
	done_recent_seconds: number;
}

export interface Dashboard {
	generated_at: string;
	project: string;
	project_id: string;
	settings: Settings;
	totals: Record<string, number>;
	buckets: Record<BucketKey, WorkItem[]>;
}

export interface DashboardResponse {
	ok: true;
	dashboard: Dashboard;
}

export interface ItemsResponse {
	ok: true;
	count: number;
	items: WorkItem[];
}

export interface ContextDoc {
	goal: string;
	current_state: string;
	constraints: string[];
	references: string[];
	notes?: string | null;
}

export interface ContextVersion {
	version: number;
	doc: ContextDoc;
	reason: string;
	author: string;
	created_at: string;
	revises_version?: number | null;
}

export interface Acceptance {
	seq: number;
	text: string;
	status: string;
	note?: string | null;
	updated_by?: string | null;
	updated_at?: string | null;
}

export interface Evidence {
	id?: string;
	kind: string;
	value: string;
	note?: string | null;
	created_by?: string;
	created_at?: string;
}

export interface AttentionRequest {
	id: string;
	question: string;
	context?: string;
	options?: string[];
	recommendation?: string | null;
	blocking: boolean;
	status: string;
	response?: string | null;
	requested_by?: string;
	created_at?: string;
}

export interface ItemDetail {
	item: WorkItem;
	context: ContextVersion;
	acceptance: Acceptance[];
	evidence: Evidence[];
	attention: AttentionRequest[];
	history_len: number;
}

export interface ItemDetailResponse {
	ok: true;
	item: ItemDetail;
}

export interface EventView {
	id: number;
	at: string;
	kind: string;
	actor: string;
	actor_kind: string;
	identifier?: string | null;
	prev_version?: number | null;
	new_version?: number | null;
	run_id?: string | null;
	session_id?: string | null;
	payload?: unknown;
}

export interface EventsResponse {
	ok: true;
	events: EventView[];
}

export interface HealthResponse {
	ok: boolean;
	service?: string;
	version?: string;
}

export interface ApiErrorBody {
	ok: false;
	error: {
		code: string;
		message: string;
		exit_code?: number;
		details?: unknown;
	};
}

/** The subset of `fetch` the UI needs, so tests can inject a fake transport. */
export interface FetcherInit {
	method?: "GET" | "POST";
	headers: Record<string, string>;
	body?: string;
	cache?: "no-store";
	signal?: AbortSignal;
}

export type Fetcher = (
	url: string,
	init: FetcherInit,
) => Promise<{ ok: boolean; status: number; text(): Promise<string> }>;
