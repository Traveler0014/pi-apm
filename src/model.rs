//! Domain model: statuses, execution health, actors, the structured context
//! document, and the JSON views returned by the store and the CLI.
//!
//! Two axes are deliberately kept apart:
//!
//! * **business status** (`Status`) — what the work is doing, decided by agents
//!   or humans;
//! * **execution health** (`Health`) — whether the holder is still alive,
//!   derived from claims and heartbeats, never settable by hand.
//!
//! Liveness timestamps are also separate (`last_heartbeat_at`,
//! `last_progress_at`, `last_run_sync_at`) so a chatty heartbeat loop can never
//! masquerade as real progress.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// Current wall clock in milliseconds since the Unix epoch (UTC).
pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// Render epoch milliseconds as RFC 3339 UTC with millisecond precision.
pub fn ts(ms: i64) -> String {
    match DateTime::<Utc>::from_timestamp_millis(ms) {
        Some(dt) => dt.to_rfc3339_opts(SecondsFormat::Millis, true),
        None => ms.to_string(),
    }
}

pub fn ts_opt(ms: Option<i64>) -> Option<String> {
    ms.map(ts)
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Business status. Deliberately small; execution state lives in `Health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ready,
    Doing,
    Blocked,
    Review,
    Done,
    Cancelled,
}

impl Status {
    pub const ALL: [Status; 6] = [
        Status::Ready,
        Status::Doing,
        Status::Blocked,
        Status::Review,
        Status::Done,
        Status::Cancelled,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ready => "ready",
            Status::Doing => "doing",
            Status::Blocked => "blocked",
            Status::Review => "review",
            Status::Done => "done",
            Status::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Result<Status> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ready" => Ok(Status::Ready),
            "doing" | "in_progress" | "in-progress" => Ok(Status::Doing),
            "blocked" => Ok(Status::Blocked),
            "review" => Ok(Status::Review),
            "done" | "completed" => Ok(Status::Done),
            "cancelled" | "canceled" => Ok(Status::Cancelled),
            other => Err(Error::invalid(format!(
                "unknown status '{other}' (expected one of: ready, doing, blocked, review, done, cancelled)"
            ))),
        }
    }

    /// Open items are the ones the dashboard cares about.
    pub fn is_open(self) -> bool {
        !matches!(self, Status::Done | Status::Cancelled)
    }

    /// Coarse workflow category, exported in `apm workflow --json`.
    pub fn category(self) -> &'static str {
        match self {
            Status::Ready => "open",
            Status::Doing | Status::Review => "active",
            Status::Blocked => "waiting",
            Status::Done | Status::Cancelled => "closed",
        }
    }
}

// ---------------------------------------------------------------------------
// Execution health
// ---------------------------------------------------------------------------

/// Derived liveness of the work item. Never writable through the CLI: it is
/// computed from the latest claim, the clock, and the project thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Live claim, recent heartbeat.
    Active,
    /// Live claim, heartbeat older than the project's `stale_after` window.
    Stale,
    /// Claim lease ran out (or was marked expired) and nobody re-claimed.
    Expired,
    /// Last claim was released voluntarily.
    Released,
    /// Last claim was superseded by a takeover.
    Superseded,
    /// Never claimed (or claims were cleared).
    Unclaimed,
    /// Work item is closed; liveness is irrelevant.
    Ended,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Active => "active",
            Health::Stale => "stale",
            Health::Expired => "expired",
            Health::Released => "released",
            Health::Superseded => "superseded",
            Health::Unclaimed => "unclaimed",
            Health::Ended => "ended",
        }
    }

    /// Items whose holder may be gone. Drives the "lost lease" dashboard bucket.
    pub fn is_at_risk(self) -> bool {
        matches!(self, Health::Stale | Health::Expired)
    }
}

// ---------------------------------------------------------------------------
// Actors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Agent,
    Human,
}

impl ActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorKind::Agent => "agent",
            ActorKind::Human => "human",
        }
    }
}

/// Who is performing a write. `session_id`/`run_id` make events replayable and
/// let a side-run prove it belongs to the session that holds the claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub name: String,
    pub kind: ActorKind,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
}

impl Actor {
    pub fn agent(name: impl Into<String>) -> Actor {
        Actor {
            name: name.into(),
            kind: ActorKind::Agent,
            session_id: None,
            run_id: None,
        }
    }

    pub fn human(name: impl Into<String>) -> Actor {
        Actor {
            name: name.into(),
            kind: ActorKind::Human,
            session_id: None,
            run_id: None,
        }
    }

    pub fn with_session(mut self, session: Option<String>) -> Actor {
        self.session_id = session;
        self
    }

    pub fn with_run(mut self, run: Option<String>) -> Actor {
        self.run_id = run;
        self
    }

    pub fn is_human(&self) -> bool {
        self.kind == ActorKind::Human
    }
}

// ---------------------------------------------------------------------------
// Context document
// ---------------------------------------------------------------------------

/// Structured handover context. A new agent must be able to act on this
/// document without reading the previous conversation, so `goal` and
/// `current_state` are mandatory for every write that materialises a version.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextDoc {
    /// What "done" means for this item.
    pub goal: String,
    /// Where things stand right now (code, environment, partial results).
    pub current_state: String,
    /// Constraints a successor must respect (tooling, policy, style, limits).
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Paths, URLs, commits, dashboards a successor should open first.
    #[serde(default)]
    pub references: Vec<String>,
    /// Anything else that does not fit above.
    #[serde(default)]
    pub notes: Option<String>,
}

impl ContextDoc {
    pub fn validate(&self) -> Result<()> {
        if self.goal.trim().is_empty() {
            return Err(Error::MissingContext(
                "`goal` must be non-empty: a successor must know what done means".into(),
            ));
        }
        if self.current_state.trim().is_empty() {
            return Err(Error::MissingContext(
                "`current_state` must be non-empty: a successor must know where things stand"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Canonical, stable content hash used for change detection and history.
    pub fn hash(&self) -> String {
        let canonical = serde_json::to_string(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hex::encode(hasher.finalize())
    }

    pub fn is_empty(&self) -> bool {
        self.goal.trim().is_empty() && self.current_state.trim().is_empty()
    }
}

// ---------------------------------------------------------------------------
// Project settings
// ---------------------------------------------------------------------------

/// Per-project operating thresholds. Persisted as JSON on the project row so a
/// team can tune "stale" and "no progress" without a schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Lease granted per claim/renewal, in seconds.
    pub lease_seconds: i64,
    /// A live claim whose heartbeat is older than this is `stale`.
    pub stale_after_seconds: i64,
    /// Open items without a progress event for this long are `stalled`.
    pub no_progress_after_seconds: i64,
    /// Window for the "recently completed" dashboard bucket.
    pub done_recent_seconds: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lease_seconds: 900,
            stale_after_seconds: 300,
            no_progress_after_seconds: 86_400,
            done_recent_seconds: 86_400,
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.lease_seconds < 30 {
            return Err(Error::invalid("lease_seconds must be >= 30"));
        }
        if self.stale_after_seconds <= 0 {
            return Err(Error::invalid("stale_after_seconds must be > 0"));
        }
        if self.stale_after_seconds >= self.lease_seconds {
            return Err(Error::invalid(
                "stale_after_seconds must be smaller than lease_seconds",
            ));
        }
        if self.no_progress_after_seconds <= 0 {
            return Err(Error::invalid("no_progress_after_seconds must be > 0"));
        }
        if self.done_recent_seconds <= 0 {
            return Err(Error::invalid("done_recent_seconds must be > 0"));
        }
        Ok(())
    }

    pub fn lease_ms(&self) -> i64 {
        self.lease_seconds * 1000
    }

    pub fn stale_after_ms(&self) -> i64 {
        self.stale_after_seconds * 1000
    }

    pub fn no_progress_after_ms(&self) -> i64 {
        self.no_progress_after_seconds * 1000
    }

    pub fn done_recent_ms(&self) -> i64 {
        self.done_recent_seconds * 1000
    }
}

// ---------------------------------------------------------------------------
// View types (JSON contract)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectView {
    pub id: String,
    pub name: String,
    pub root_path: Option<String>,
    pub created_at: String,
    pub settings: Settings,
    pub db_path: String,
    pub workflow: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimView {
    pub id: String,
    pub work_item_id: String,
    pub identifier: String,
    pub holder: String,
    pub holder_kind: ActorKind,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    /// First characters of the token, for human correlation. Never the token.
    pub token_prefix: String,
    pub status: String,
    pub leased_at: String,
    pub heartbeat_at: String,
    pub expires_at: String,
    pub lease_seconds: i64,
    pub expired: bool,
    pub released_at: Option<String>,
    pub release_reason: Option<String>,
}

/// Returned exactly once, when a claim is issued.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimIssued {
    pub claim: ClaimView,
    /// Plaintext token. Empty on an idempotent replay: the database never stores
    /// it, so a replay can only return the session-file copy (if any).
    pub token: String,
    /// True when the lease was taken over from an expired/superseded claim.
    pub takeover: bool,
    /// True when this response was replayed from the idempotency record.
    pub replayed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LabelView {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyRef {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub status: Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptanceView {
    pub seq: i64,
    pub text: String,
    pub status: String,
    pub note: Option<String>,
    pub updated_by: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceView {
    pub id: String,
    pub kind: String,
    pub value: String,
    pub note: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextVersionView {
    pub work_item_id: String,
    pub version: i64,
    pub doc: ContextDoc,
    pub doc_hash: String,
    pub revises_version: Option<i64>,
    pub reason: String,
    pub author: String,
    pub actor_kind: ActorKind,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionBrief {
    pub id: String,
    pub question: String,
    pub blocking: bool,
    pub status: String,
}

/// A structured request for a human decision. This is the canonical "waiting for
/// human" record; `waiting_on_human` on the item is only a coarse flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttentionView {
    pub id: String,
    pub work_item_id: Option<String>,
    pub identifier: Option<String>,
    pub question: String,
    pub context: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    pub blocking: bool,
    pub status: String,
    pub response: Option<String>,
    pub requested_by: String,
    pub requested_kind: ActorKind,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub created_at: String,
    pub resolved_at: Option<String>,
    pub resolved_by: Option<String>,
}

/// Last run-end synchronisation recorded for an item, derived from the audit
/// trail so the dashboard can tell a normal sync from an interrupted run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSyncView {
    pub run_id: Option<String>,
    pub outcome: String,
    pub at: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItemView {
    pub id: String,
    pub project_id: String,
    pub project: String,
    pub number: i64,
    pub identifier: String,
    pub title: String,
    pub item_type: String,
    pub status: Status,
    pub status_category: String,
    pub priority: String,
    pub parent_id: Option<String>,
    pub parent_identifier: Option<String>,
    pub version: i64,
    pub context_version: i64,
    pub progress_summary: Option<String>,
    pub next_action: Option<String>,
    pub blocker: Option<String>,
    pub waiting_on_human: bool,
    pub waiting_reason: Option<String>,
    pub completion_note: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_heartbeat_at: Option<String>,
    pub last_progress_at: Option<String>,
    pub last_run_sync_at: Option<String>,
    pub done_at: Option<String>,
    pub cancelled_at: Option<String>,
    pub health: Health,
    pub claim: Option<ClaimView>,
    pub labels: Vec<String>,
    pub blocked_by: Vec<DependencyRef>,
    pub blocks: Vec<DependencyRef>,
    /// Open attention requests, blocking first.
    pub attention: Vec<AttentionBrief>,
    /// Last run-end sync (completed or aborted), from the audit trail.
    pub last_run: Option<RunSyncView>,
    /// Seconds since the last *substantive* progress event (never heartbeat).
    pub seconds_since_progress: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemDetail {
    pub item: WorkItemView,
    pub context: ContextVersionView,
    pub acceptance: Vec<AcceptanceView>,
    pub evidence: Vec<EvidenceView>,
    pub attention: Vec<AttentionView>,
    pub history_len: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventView {
    pub id: i64,
    pub at: String,
    pub kind: String,
    pub actor: String,
    pub actor_kind: ActorKind,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub item_id: Option<String>,
    pub identifier: Option<String>,
    pub prev_version: Option<i64>,
    pub new_version: Option<i64>,
    pub idempotency_key: Option<String>,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Buckets {
    pub waiting_human: Vec<WorkItemView>,
    pub in_progress: Vec<WorkItemView>,
    pub stalled_no_progress: Vec<WorkItemView>,
    pub lost_lease: Vec<WorkItemView>,
    pub waiting_dependencies: Vec<WorkItemView>,
    pub recently_done: Vec<WorkItemView>,
    pub ready_unclaimed: Vec<WorkItemView>,
    /// Items whose last side-run could not recover the lease (see `apm doctor`).
    pub recovery_failed: Vec<WorkItemView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dashboard {
    pub generated_at: String,
    pub project: String,
    pub project_id: String,
    pub settings: Settings,
    pub totals: BTreeMap<String, usize>,
    pub buckets: Buckets,
}

impl Dashboard {
    pub fn is_empty(&self) -> bool {
        self.buckets.waiting_human.is_empty()
            && self.buckets.in_progress.is_empty()
            && self.buckets.stalled_no_progress.is_empty()
            && self.buckets.lost_lease.is_empty()
            && self.buckets.waiting_dependencies.is_empty()
            && self.buckets.ready_unclaimed.is_empty()
            && self.buckets.recently_done.is_empty()
            && self.buckets.recovery_failed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip() {
        for s in Status::ALL {
            assert_eq!(Status::parse(s.as_str()).unwrap(), s);
        }
        assert!(Status::parse("nope").is_err());
        assert_eq!(Status::parse("Done").unwrap(), Status::Done);
    }

    #[test]
    fn settings_validation() {
        let mut s = Settings::default();
        assert!(s.validate().is_ok());
        s.stale_after_seconds = s.lease_seconds;
        assert!(s.validate().is_err());
        s = Settings::default();
        s.lease_seconds = 10;
        assert!(s.validate().is_err());
    }

    #[test]
    fn context_requires_goal_and_state() {
        let mut doc = ContextDoc::default();
        assert!(doc.validate().is_err());
        doc.goal = "ship it".into();
        assert!(doc.validate().is_err());
        doc.current_state = "half way".into();
        assert!(doc.validate().is_ok());
        let before = doc.hash();
        assert_eq!(before, doc.clone().hash());
        doc.notes = Some("changed".into());
        assert_ne!(before, doc.hash());
        doc.notes = None;
        assert_eq!(before, doc.hash());
        doc.current_state = "done".into();
        assert_ne!(before, doc.hash());
    }
}
