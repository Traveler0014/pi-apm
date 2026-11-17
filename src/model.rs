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

/// A business state key.
///
/// States are declared by a workflow, not by a Rust enum: a project can define
/// its own state machine per work-item type. The key is a validated slug, and
/// every state belongs to exactly one `Category` that the dashboard, health and
/// "is this item open?" logic use.
///
/// `Status` remains as an alias so older code (including the CLI) keeps
/// compiling: `Status::parse("doing")` still works and serialises as `"doing"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateKey(String);

/// Compatibility alias for the pre-workflow name of this type.
pub type Status = StateKey;

impl StateKey {
    pub const MAX_LEN: usize = 64;
    /// The six built-in state keys, in the order used by the default workflow.
    pub const BUILTIN: [&'static str; 6] =
        ["ready", "doing", "blocked", "review", "done", "cancelled"];

    pub fn ready() -> StateKey {
        StateKey::parse("ready").expect("builtin state")
    }
    pub fn doing() -> StateKey {
        StateKey::parse("doing").expect("builtin state")
    }
    pub fn blocked() -> StateKey {
        StateKey::parse("blocked").expect("builtin state")
    }
    pub fn review() -> StateKey {
        StateKey::parse("review").expect("builtin state")
    }
    pub fn done() -> StateKey {
        StateKey::parse("done").expect("builtin state")
    }
    pub fn cancelled() -> StateKey {
        StateKey::parse("cancelled").expect("builtin state")
    }

    /// Parse and normalise a state key.
    ///
    /// Legacy spellings from the fixed six-state era are folded onto their
    /// canonical keys; anything else must be a lowercase slug so the key is safe
    /// in JSON, SQL and CLI arguments. Whether the key exists is decided by the
    /// item's workflow, not here.
    pub fn parse(raw: &str) -> Result<StateKey> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(Error::invalid("state key must be non-empty"));
        }
        let normalised = match trimmed.to_ascii_lowercase().as_str() {
            "in_progress" | "in-progress" => "doing".to_string(),
            "complete" | "completed" => "done".to_string(),
            "canceled" => "cancelled".to_string(),
            other => other.to_string(),
        };
        if normalised.len() > Self::MAX_LEN {
            return Err(Error::invalid(format!(
                "state key '{normalised}' is longer than {} characters",
                Self::MAX_LEN
            )));
        }
        let first = normalised.chars().next().expect("non-empty");
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(Error::invalid(format!(
                "invalid state key '{normalised}': must start with a lowercase letter or digit"
            )));
        }
        if !normalised
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        {
            return Err(Error::invalid(format!(
                "invalid state key '{normalised}': use lowercase letters, digits, '.', '_' or '-'"
            )));
        }
        Ok(StateKey(normalised))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn is_builtin(&self) -> bool {
        Self::BUILTIN.contains(&self.0.as_str())
    }
}

impl std::fmt::Display for StateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for StateKey {
    type Err = Error;
    fn from_str(s: &str) -> Result<StateKey> {
        StateKey::parse(s)
    }
}

/// The five dashboard/health categories every workflow state must map onto.
///
/// Categories are what makes custom states safe: dashboard buckets, execution
/// health and the "closed" rule never depend on a state's literal key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Open,
    Active,
    Waiting,
    Review,
    Closed,
}

impl Category {
    pub const ALL: [Category; 5] = [
        Category::Open,
        Category::Active,
        Category::Waiting,
        Category::Review,
        Category::Closed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Category::Open => "open",
            Category::Active => "active",
            Category::Waiting => "waiting",
            Category::Review => "review",
            Category::Closed => "closed",
        }
    }

    pub fn parse(raw: &str) -> Result<Category> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "open" => Ok(Category::Open),
            "active" => Ok(Category::Active),
            "waiting" => Ok(Category::Waiting),
            "review" => Ok(Category::Review),
            "closed" => Ok(Category::Closed),
            other => Err(Error::invalid(format!(
                "unknown category '{other}' (expected one of: open, active, waiting, review, closed)"
            ))),
        }
    }

    pub fn is_closed(self) -> bool {
        self == Category::Closed
    }

    /// Legacy mapping used when migrating a v2 row whose state is one of the six
    /// built-ins and no workflow document is available.
    pub fn legacy_for_state(state: &str) -> Category {
        match state {
            "doing" | "review" => Category::Active,
            "blocked" => Category::Waiting,
            "done" | "cancelled" => Category::Closed,
            _ => Category::Open,
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
    pub status: StateKey,
    /// Category of the dependency's state, so "is this dependency still open?"
    /// works for custom workflows without a second lookup.
    pub status_category: String,
}

impl DependencyRef {
    pub fn is_open(&self) -> bool {
        self.status_category != Category::Closed.as_str()
    }
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
    pub status: StateKey,
    pub status_category: String,
    /// The workflow snapshot this item runs on (id + version), recorded at
    /// creation so later configuration changes cannot reinterpret history.
    pub workflow_id: String,
    pub workflow_version: i64,
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

impl WorkItemView {
    /// Closed is a category, not a hard-coded state key.
    pub fn is_open(&self) -> bool {
        self.status_category != Category::Closed.as_str()
    }
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
    fn state_key_roundtrip_and_normalisation() {
        for s in StateKey::BUILTIN {
            assert_eq!(StateKey::parse(s).unwrap().as_str(), s);
        }
        assert_eq!(StateKey::parse("Done").unwrap(), StateKey::done());
        assert_eq!(StateKey::parse("in_progress").unwrap(), StateKey::doing());
        assert_eq!(StateKey::parse("canceled").unwrap(), StateKey::cancelled());
        assert!(StateKey::parse("").is_err());
        assert!(StateKey::parse("has space").is_err());
        assert_eq!(StateKey::parse("Upper").unwrap().as_str(), "upper");
        assert!(StateKey::parse(&"x".repeat(65)).is_err());
        assert_eq!(
            StateKey::parse("agent_verified").unwrap().as_str(),
            "agent_verified"
        );
    }

    #[test]
    fn categories_roundtrip() {
        for c in Category::ALL {
            assert_eq!(Category::parse(c.as_str()).unwrap(), c);
        }
        assert_eq!(Category::legacy_for_state("review"), Category::Active);
        assert_eq!(Category::legacy_for_state("done"), Category::Closed);
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
