//! Persistence layer. One `Store` owns one SQLite connection; every mutating
//! method runs in `BEGIN IMMEDIATE` so that claim acquisition and
//! version-checked updates are serialised across processes.

pub mod claims;
pub mod context;
pub mod events;
pub mod items;
pub mod misc;
pub mod project;
pub mod workflows;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::error::{Error, Result};
use crate::model::*;

pub struct Store {
    pub(crate) conn: Connection,
    pub(crate) path: PathBuf,
}

impl Store {
    /// Open (creating if needed) the database at `path` and migrate it.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Internal(format!("cannot create {}: {e}", parent.display())))?;
        }
        let mut conn = Connection::open(&path)?;
        db::migrate(&mut conn)?;
        Ok(Store { conn, path })
    }

    /// In-memory store for tests. Not durable, obviously.
    pub fn open_in_memory() -> Result<Store> {
        let mut conn = Connection::open_in_memory()?;
        db::migrate(&mut conn)?;
        Ok(Store {
            conn,
            path: PathBuf::from(":memory:"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Directory that holds the database; session token files live beside it.
    pub fn data_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub(crate) fn with_tx<T>(
        &mut self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// Run `f` at most once per idempotency key, and only for the exact same
    /// operation, item and request payload.
    ///
    /// A replay returns the first response and writes no new events. Reusing a
    /// key for a *different* request is an error (`idempotency_conflict`) rather
    /// than a silent wrong answer — otherwise a mis-keyed side-run would think it
    /// had updated item B when it had actually updated item A.
    ///
    /// Everything happens in one transaction, so a crash cannot leave a
    /// half-applied write with a consumed key.
    pub(crate) fn idempotent<T>(
        &mut self,
        idem: Idem<'_>,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
    {
        let Idem {
            key,
            op,
            item_id,
            request,
        } = idem;
        let fingerprint = request_fingerprint(op, item_id.as_deref(), &request);
        self.with_tx(|tx| {
            if let Some(key) = key
                && let Some(stored) = lookup_idempotent(tx, key)?
            {
                if stored.op != op
                    || stored.work_item_id.as_deref() != item_id.as_deref()
                    || stored.fingerprint != fingerprint
                {
                    return Err(Error::IdempotencyConflict {
                        key: key.to_string(),
                    });
                }
                return serde_json::from_str::<T>(&stored.response_json).map_err(Error::Serde);
            }
            let out = f(tx)?;
            if let Some(key) = key {
                tx.execute(
                    "INSERT INTO idempotency (key, work_item_id, op, fingerprint, response_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        key,
                        item_id,
                        op,
                        fingerprint,
                        serde_json::to_string(&out)?,
                        now_ms()
                    ],
                )?;
            }
            Ok(out)
        })
    }
}

/// Idempotency scope for one mutating operation.
pub(crate) struct Idem<'a> {
    key: Option<&'a str>,
    op: &'a str,
    item_id: Option<String>,
    request: serde_json::Value,
}

impl<'a> Idem<'a> {
    pub(crate) fn new(op: &'a str, key: Option<&'a str>) -> Idem<'a> {
        Idem {
            key,
            op,
            item_id: None,
            request: serde_json::Value::Null,
        }
    }

    pub(crate) fn item(mut self, item_id: Option<&str>) -> Idem<'a> {
        self.item_id = item_id.map(str::to_string);
        self
    }

    pub(crate) fn request(mut self, request: serde_json::Value) -> Idem<'a> {
        self.request = request;
        self
    }
}

pub(crate) fn request_fingerprint(
    op: &str,
    item_id: Option<&str>,
    request: &serde_json::Value,
) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(op.as_bytes());
    hasher.update(b"\n");
    hasher.update(item_id.unwrap_or("").as_bytes());
    hasher.update(b"\n");
    // serde_json maps are BTreeMap-backed (no preserve_order feature), so this
    // is canonical: same fields, same order, same bytes.
    hasher.update(
        serde_json::to_string(request)
            .unwrap_or_default()
            .as_bytes(),
    );
    hex::encode(hasher.finalize())
}

pub(crate) struct StoredIdempotency {
    pub work_item_id: Option<String>,
    pub op: String,
    pub fingerprint: String,
    pub response_json: String,
}

pub(crate) fn lookup_idempotent(
    tx: &Transaction<'_>,
    key: &str,
) -> Result<Option<StoredIdempotency>> {
    Ok(tx
        .query_row(
            "SELECT work_item_id, op, fingerprint, response_json FROM idempotency WHERE key = ?1",
            params![key],
            |r| {
                Ok(StoredIdempotency {
                    work_item_id: r.get(0)?,
                    op: r.get(1)?,
                    fingerprint: r.get(2)?,
                    response_json: r.get(3)?,
                })
            },
        )
        .optional()?)
}

// ---------------------------------------------------------------------------
// Raw rows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct RawItem {
    pub id: String,
    pub project_id: String,
    pub project_name: String,
    pub number: i64,
    pub identifier: String,
    pub title: String,
    pub item_type: String,
    pub status: StateKey,
    pub state_category: String,
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
    pub created_at: i64,
    pub updated_at: i64,
    pub last_heartbeat_at: Option<i64>,
    pub last_progress_at: Option<i64>,
    pub last_run_sync_at: Option<i64>,
    pub done_at: Option<i64>,
    pub cancelled_at: Option<i64>,
}

pub(crate) const ITEM_SELECT: &str = "
SELECT i.id, i.project_id, p.name, i.number, i.identifier, i.title, i.item_type,
       i.status, i.state_category, i.workflow_id, i.workflow_version, i.priority,
       i.parent_id, par.identifier,
       i.version, i.context_version, i.progress_summary, i.next_action, i.blocker,
       i.waiting_on_human, i.waiting_reason, i.completion_note, i.created_by,
       i.created_at, i.updated_at, i.last_heartbeat_at, i.last_progress_at,
       i.last_run_sync_at, i.done_at, i.cancelled_at
FROM work_items i
JOIN projects p ON p.id = i.project_id
LEFT JOIN work_items par ON par.id = i.parent_id
";

impl RawItem {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<RawItem> {
        let status: String = row.get(7)?;
        Ok(RawItem {
            id: row.get(0)?,
            project_id: row.get(1)?,
            project_name: row.get(2)?,
            number: row.get(3)?,
            identifier: row.get(4)?,
            title: row.get(5)?,
            item_type: row.get(6)?,
            status: StateKey::parse(&status).unwrap_or_else(|_| StateKey::ready()),
            state_category: row.get(8)?,
            workflow_id: row.get(9)?,
            workflow_version: row.get(10)?,
            priority: row.get(11)?,
            parent_id: row.get(12)?,
            parent_identifier: row.get(13)?,
            version: row.get(14)?,
            context_version: row.get(15)?,
            progress_summary: row.get(16)?,
            next_action: row.get(17)?,
            blocker: row.get(18)?,
            waiting_on_human: row.get::<_, i64>(19)? != 0,
            waiting_reason: row.get(20)?,
            completion_note: row.get(21)?,
            created_by: row.get(22)?,
            created_at: row.get(23)?,
            updated_at: row.get(24)?,
            last_heartbeat_at: row.get(25)?,
            last_progress_at: row.get(26)?,
            last_run_sync_at: row.get(27)?,
            done_at: row.get(28)?,
            cancelled_at: row.get(29)?,
        })
    }

    /// Open means "not in a closed category", which is workflow-defined rather
    /// than a hard-coded state list.
    pub(crate) fn is_open(&self) -> bool {
        self.state_category != Category::Closed.as_str()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RawClaim {
    pub id: String,
    pub work_item_id: String,
    pub token_hash: String,
    pub token_prefix: String,
    pub holder: String,
    pub holder_kind: ActorKind,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub status: String,
    pub lease_seconds: i64,
    pub leased_at: i64,
    pub heartbeat_at: i64,
    pub expires_at: i64,
    pub released_at: Option<i64>,
    pub release_reason: Option<String>,
}

pub(crate) const CLAIM_SELECT: &str = "
SELECT id, work_item_id, token_hash, token_prefix, holder, holder_kind, session_id, run_id,
       status, lease_seconds, leased_at, heartbeat_at, expires_at, released_at, release_reason
FROM claims
";

impl RawClaim {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<RawClaim> {
        let kind: String = row.get(5)?;
        Ok(RawClaim {
            id: row.get(0)?,
            work_item_id: row.get(1)?,
            token_hash: row.get(2)?,
            token_prefix: row.get(3)?,
            holder: row.get(4)?,
            holder_kind: if kind == "human" {
                ActorKind::Human
            } else {
                ActorKind::Agent
            },
            session_id: row.get(6)?,
            run_id: row.get(7)?,
            status: row.get(8)?,
            lease_seconds: row.get(9)?,
            leased_at: row.get(10)?,
            heartbeat_at: row.get(11)?,
            expires_at: row.get(12)?,
            released_at: row.get(13)?,
            release_reason: row.get(14)?,
        })
    }

    pub(crate) fn view(&self, identifier: &str, now: i64) -> ClaimView {
        ClaimView {
            id: self.id.clone(),
            work_item_id: self.work_item_id.clone(),
            identifier: identifier.to_string(),
            holder: self.holder.clone(),
            holder_kind: self.holder_kind,
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            token_prefix: self.token_prefix.clone(),
            status: self.status.clone(),
            leased_at: ts(self.leased_at),
            heartbeat_at: ts(self.heartbeat_at),
            expires_at: ts(self.expires_at),
            lease_seconds: self.lease_seconds,
            expired: self.status == "active" && self.expires_at <= now,
            released_at: ts_opt(self.released_at),
            release_reason: self.release_reason.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RawContext {
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
    pub created_at: i64,
}

impl RawContext {
    pub(crate) fn view(&self) -> ContextVersionView {
        ContextVersionView {
            work_item_id: self.work_item_id.clone(),
            version: self.version,
            doc: self.doc.clone(),
            doc_hash: self.doc_hash.clone(),
            revises_version: self.revises_version,
            reason: self.reason.clone(),
            author: self.author.clone(),
            actor_kind: self.actor_kind,
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            created_at: ts(self.created_at),
        }
    }
}

pub(crate) const CONTEXT_SELECT: &str = "
SELECT work_item_id, version, goal, current_state, constraints_json, references_json, notes,
       doc_hash, revises_version, reason, author, actor_kind, session_id, run_id, created_at
FROM context_versions
";

impl RawContext {
    pub(crate) fn from_row(row: &Row<'_>) -> rusqlite::Result<RawContext> {
        let constraints: String = row.get(4)?;
        let references: String = row.get(5)?;
        let kind: String = row.get(11)?;
        Ok(RawContext {
            work_item_id: row.get(0)?,
            version: row.get(1)?,
            doc: ContextDoc {
                goal: row.get(2)?,
                current_state: row.get(3)?,
                constraints: serde_json::from_str(&constraints).unwrap_or_default(),
                references: serde_json::from_str(&references).unwrap_or_default(),
                notes: row.get(6)?,
            },
            doc_hash: row.get(7)?,
            revises_version: row.get(8)?,
            reason: row.get(9)?,
            author: row.get(10)?,
            actor_kind: if kind == "human" {
                ActorKind::Human
            } else {
                ActorKind::Agent
            },
            session_id: row.get(12)?,
            run_id: row.get(13)?,
            created_at: row.get(14)?,
        })
    }
}

// ---------------------------------------------------------------------------
// Lookups and hydration
// ---------------------------------------------------------------------------

/// Resolve an item by uuid, identifier (`APM-7`) or per-project number.
pub(crate) fn find_raw_item(conn: &Connection, selector: &str) -> Result<Option<RawItem>> {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let sql = format!("{ITEM_SELECT} WHERE i.id = ?1");
    if let Some(row) = conn
        .query_row(&sql, params![trimmed], RawItem::from_row)
        .optional()?
    {
        return Ok(Some(row));
    }
    let sql = format!("{ITEM_SELECT} WHERE i.identifier = ?1 COLLATE NOCASE");
    if let Some(row) = conn
        .query_row(&sql, params![trimmed], RawItem::from_row)
        .optional()?
    {
        return Ok(Some(row));
    }
    if let Ok(number) = trimmed.parse::<i64>() {
        let sql = format!("{ITEM_SELECT} WHERE i.number = ?1");
        let rows = collect_items(conn, &sql, params![number])?;
        if rows.len() == 1 {
            return Ok(rows.into_iter().next());
        }
    }
    Ok(None)
}

pub(crate) fn require_raw_item(conn: &Connection, selector: &str) -> Result<RawItem> {
    find_raw_item(conn, selector)?.ok_or_else(|| Error::NotFound {
        entity: "work item",
        id: selector.to_string(),
    })
}

pub(crate) fn collect_items<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<RawItem>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params, RawItem::from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn latest_claim(conn: &Connection, item_id: &str) -> Result<Option<RawClaim>> {
    let sql = format!(
        "{CLAIM_SELECT} WHERE work_item_id = ?1 ORDER BY leased_at DESC, rowid DESC LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![item_id], RawClaim::from_row)
        .optional()?)
}

pub(crate) fn active_claim(conn: &Connection, item_id: &str) -> Result<Option<RawClaim>> {
    let sql = format!(
        "{CLAIM_SELECT} WHERE work_item_id = ?1 AND status = 'active' ORDER BY leased_at DESC LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![item_id], RawClaim::from_row)
        .optional()?)
}

pub(crate) fn health_for(
    item: &RawItem,
    claim: Option<&RawClaim>,
    now: i64,
    settings: &Settings,
) -> Health {
    if !item.is_open() {
        return Health::Ended;
    }
    let Some(claim) = claim else {
        return Health::Unclaimed;
    };
    match claim.status.as_str() {
        "active" if claim.expires_at > now => {
            if now - claim.heartbeat_at <= settings.stale_after_ms() {
                Health::Active
            } else {
                Health::Stale
            }
        }
        "active" => Health::Expired,
        "expired" => Health::Expired,
        "released" => Health::Released,
        "superseded" => Health::Superseded,
        _ => Health::Unclaimed,
    }
}

pub(crate) fn hydrate_items(
    conn: &Connection,
    rows: Vec<RawItem>,
    now: i64,
    settings: &Settings,
) -> Result<Vec<WorkItemView>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(hydrate_item(conn, &row, now, settings)?);
    }
    Ok(out)
}

impl Store {
    /// Canonical item id for any selector (uuid, identifier, or number).
    pub fn item_id(&self, selector: &str) -> Result<String> {
        Ok(require_raw_item(&self.conn, selector)?.id)
    }

    /// Canonical `(id, identifier)` pair for any selector.
    pub fn item_ref(&self, selector: &str) -> Result<(String, String)> {
        let raw = require_raw_item(&self.conn, selector)?;
        Ok((raw.id, raw.identifier))
    }
}

pub(crate) fn hydrate_item(
    conn: &Connection,
    row: &RawItem,
    now: i64,
    settings: &Settings,
) -> Result<WorkItemView> {
    let claim = latest_claim(conn, &row.id)?;
    let health = health_for(row, claim.as_ref(), now, settings);
    let labels = labels_of(conn, &row.id)?;
    let blocked_by = dependencies_of(conn, &row.id)?;
    let blocks = dependents_of(conn, &row.id)?;
    let attention: Vec<AttentionBrief> = misc::attention_of(conn, Some(&row.id))?
        .into_iter()
        .filter(|a| a.status == "open")
        .map(|a| AttentionBrief {
            id: a.id,
            question: a.question,
            blocking: a.blocking,
            status: a.status,
        })
        .collect();
    let last_run = events::last_run(conn, &row.id)?;
    Ok(WorkItemView {
        id: row.id.clone(),
        project_id: row.project_id.clone(),
        project: row.project_name.clone(),
        number: row.number,
        identifier: row.identifier.clone(),
        title: row.title.clone(),
        item_type: row.item_type.clone(),
        status: row.status.clone(),
        status_category: row.state_category.clone(),
        workflow_id: row.workflow_id.clone(),
        workflow_version: row.workflow_version,
        priority: row.priority.clone(),
        parent_id: row.parent_id.clone(),
        parent_identifier: row.parent_identifier.clone(),
        version: row.version,
        context_version: row.context_version,
        progress_summary: row.progress_summary.clone(),
        next_action: row.next_action.clone(),
        blocker: row.blocker.clone(),
        waiting_on_human: row.waiting_on_human,
        waiting_reason: row.waiting_reason.clone(),
        completion_note: row.completion_note.clone(),
        created_by: row.created_by.clone(),
        created_at: ts(row.created_at),
        updated_at: ts(row.updated_at),
        last_heartbeat_at: ts_opt(row.last_heartbeat_at),
        last_progress_at: ts_opt(row.last_progress_at),
        last_run_sync_at: ts_opt(row.last_run_sync_at),
        done_at: ts_opt(row.done_at),
        cancelled_at: ts_opt(row.cancelled_at),
        health,
        claim: claim.map(|c| c.view(&row.identifier, now)),
        labels,
        blocked_by,
        blocks,
        attention,
        last_run,
        seconds_since_progress: row.last_progress_at.map(|p| ((now - p).max(0)) / 1000),
    })
}

pub(crate) fn labels_of(conn: &Connection, item_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT l.name FROM labels l JOIN item_labels il ON il.label_id = l.id
         WHERE il.work_item_id = ?1 ORDER BY l.name",
    )?;
    let rows = stmt
        .query_map(params![item_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn dependencies_of(conn: &Connection, item_id: &str) -> Result<Vec<DependencyRef>> {
    let mut stmt = conn.prepare(
        "SELECT i.id, i.identifier, i.title, i.status, i.state_category FROM dependencies d
         JOIN work_items i ON i.id = d.depends_on_id
         WHERE d.work_item_id = ?1 ORDER BY i.number",
    )?;
    let rows = stmt
        .query_map(params![item_id], |r| {
            let status: String = r.get(3)?;
            Ok(DependencyRef {
                id: r.get(0)?,
                identifier: r.get(1)?,
                title: r.get(2)?,
                status: StateKey::parse(&status).unwrap_or_else(|_| StateKey::ready()),
                status_category: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn dependents_of(conn: &Connection, item_id: &str) -> Result<Vec<DependencyRef>> {
    let mut stmt = conn.prepare(
        "SELECT i.id, i.identifier, i.title, i.status, i.state_category FROM dependencies d
         JOIN work_items i ON i.id = d.work_item_id
         WHERE d.depends_on_id = ?1 ORDER BY i.number",
    )?;
    let rows = stmt
        .query_map(params![item_id], |r| {
            let status: String = r.get(3)?;
            Ok(DependencyRef {
                id: r.get(0)?,
                identifier: r.get(1)?,
                title: r.get(2)?,
                status: StateKey::parse(&status).unwrap_or_else(|_| StateKey::ready()),
                status_category: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn acceptance_of(conn: &Connection, item_id: &str) -> Result<Vec<AcceptanceView>> {
    let mut stmt = conn.prepare(
        "SELECT seq, text, status, note, updated_by, updated_at FROM acceptance_criteria
         WHERE work_item_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![item_id], |r| {
            let updated_at: Option<i64> = r.get(5)?;
            Ok(AcceptanceView {
                seq: r.get(0)?,
                text: r.get(1)?,
                status: r.get(2)?,
                note: r.get(3)?,
                updated_by: r.get(4)?,
                updated_at: ts_opt(updated_at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn evidence_of(conn: &Connection, item_id: &str) -> Result<Vec<EvidenceView>> {
    let mut stmt = conn.prepare(
        "SELECT id, kind, value, note, created_by, created_at FROM evidence
         WHERE work_item_id = ?1 ORDER BY created_at, rowid",
    )?;
    let rows = stmt
        .query_map(params![item_id], |r| {
            let at: i64 = r.get(5)?;
            Ok(EvidenceView {
                id: r.get(0)?,
                kind: r.get(1)?,
                value: r.get(2)?,
                note: r.get(3)?,
                created_by: r.get(4)?,
                created_at: ts(at),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Project access
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub root_path: Option<String>,
    pub created_at: i64,
    pub settings: Settings,
    pub workflow_json: Option<String>,
    pub default_workflow_id: Option<String>,
    pub default_workflow_version: Option<i64>,
}

impl ProjectRow {
    pub fn view(&self, db_path: &Path) -> ProjectView {
        ProjectView {
            id: self.id.clone(),
            name: self.name.clone(),
            root_path: self.root_path.clone(),
            created_at: ts(self.created_at),
            settings: self.settings.clone(),
            db_path: db_path.display().to_string(),
            workflow: "default-v1".to_string(),
        }
    }
}

pub(crate) fn project_from_row(row: &Row<'_>) -> rusqlite::Result<ProjectRow> {
    let settings: String = row.get(5)?;
    Ok(ProjectRow {
        id: row.get(0)?,
        name: row.get(1)?,
        prefix: row.get(2)?,
        root_path: row.get(3)?,
        created_at: row.get(4)?,
        settings: serde_json::from_str(&settings).unwrap_or_default(),
        workflow_json: row.get(6)?,
        default_workflow_id: row.get(7)?,
        default_workflow_version: row.get(8)?,
    })
}

pub(crate) const PROJECT_SELECT: &str = "SELECT id, name, prefix, root_path, created_at,
    settings_json, workflow_json, default_workflow_id, default_workflow_version FROM projects";

impl Store {
    pub(crate) fn project_by_name(&self, name: &str) -> Result<Option<ProjectRow>> {
        let sql = format!("{PROJECT_SELECT} WHERE name = ?1 OR id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![name], project_from_row)
            .optional()?)
    }

    /// Resolve the active project: explicit name/id, else the only project.
    pub fn require_project(&self, explicit: Option<&str>) -> Result<ProjectRow> {
        if let Some(name) = explicit {
            return self.project_by_name(name)?.ok_or_else(|| Error::NotFound {
                entity: "project",
                id: name.to_string(),
            });
        }
        let mut stmt = self.conn.prepare(PROJECT_SELECT)?;
        let rows = stmt
            .query_map([], project_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match rows.len() {
            0 => Err(Error::NotFound {
                entity: "project",
                id: "(none initialised; run `apm init --name <name>`)".to_string(),
            }),
            1 => Ok(rows.into_iter().next().expect("checked len")),
            _ => Err(Error::invalid(
                "database contains several projects; pass --project <name>",
            )),
        }
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectView>> {
        let mut stmt = self.conn.prepare(PROJECT_SELECT)?;
        let rows = stmt
            .query_map([], project_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.iter().map(|p| p.view(&self.path)).collect())
    }

    pub(crate) fn settings_for(&self, project_id: &str) -> Result<Settings> {
        let raw: String = self.conn.query_row(
            "SELECT settings_json FROM projects WHERE id = ?1",
            params![project_id],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }
}
