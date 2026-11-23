//! Work-item reads and writes.
//!
//! Every write follows the same shape: resolve item → check version (optimistic
//! lock) → check claim (authorisation) → mutate → append event → return the
//! fresh view. Because each call runs in `BEGIN IMMEDIATE`, a version check and
//! the write it guards cannot interleave with another process.

use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params, params_from_iter};
use serde_json::json;

use super::*;
use crate::store::claims::require_write_claim;
use crate::workflow::GuardContext;

pub const PRIORITIES: [&str; 5] = ["lowest", "low", "normal", "high", "highest"];
/// Default work-item types. Types are now validated as slugs, so a project can
/// bind additional types to their own workflows; this list documents the ones
/// the default workflow covers out of the box.
pub const ITEM_TYPES: [&str; 6] = ["task", "bug", "story", "chore", "spike", "question"];

pub struct NewItem {
    pub title: String,
    pub context: ContextDoc,
    pub item_type: Option<String>,
    pub priority: Option<String>,
    pub parent: Option<String>,
    pub depends_on: Vec<String>,
    pub labels: Vec<String>,
    pub acceptance: Vec<String>,
    pub actor: Actor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateOutcome {
    pub item: WorkItemView,
    pub context: ContextVersionView,
}

/// Partial update. `Some(None)` clears a nullable field.
#[derive(Debug, Clone, Default)]
pub struct ItemPatch {
    pub title: Option<String>,
    pub item_type: Option<String>,
    pub priority: Option<String>,
    pub next_action: Option<Option<String>>,
    pub blocker: Option<Option<String>>,
    pub waiting_on_human: Option<bool>,
    pub waiting_reason: Option<Option<String>>,
    pub parent: Option<Option<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub project: Option<String>,
    pub status: Vec<Status>,
    pub include_closed: bool,
    pub label: Option<String>,
    pub parent: Option<String>,
    pub session: Option<String>,
    pub agent: Option<String>,
    pub keywords: Option<String>,
    pub attention_only: bool,
    pub limit: i64,
}

/// Result of `apm progress`.
///
/// `no_op` means the request matched the current state exactly: nothing was
/// written, `version` and `last_progress_at` did not move. `substantive` is
/// false for bookkeeping-only updates (e.g. changing `next_action` while the
/// summary stays the same), which also do not advance the progress clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressOutcome {
    pub item: WorkItemView,
    pub no_op: bool,
    pub substantive: bool,
    pub changed_fields: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProgressRequest {
    summary: String,
    next_action: Option<Option<String>>,
    blocker: Option<Option<String>>,
    waiting_on_human: Option<bool>,
    waiting_reason: Option<Option<String>>,
}

impl ProgressRequest {
    /// Fields that would actually change. Empty means a full no-op.
    fn changed_fields(&self, raw: &RawItem) -> Vec<String> {
        let mut out = Vec::new();
        if raw.progress_summary.as_deref() != Some(self.summary.as_str()) {
            out.push("progress_summary".to_string());
        }
        if let Some(next) = &self.next_action
            && &raw.next_action != next
        {
            out.push("next_action".to_string());
        }
        if let Some(blocker) = &self.blocker
            && &raw.blocker != blocker
        {
            out.push("blocker".to_string());
        }
        if let Some(waiting) = self.waiting_on_human
            && raw.waiting_on_human != waiting
        {
            out.push("waiting_on_human".to_string());
        }
        if let Some(reason) = &self.waiting_reason
            && &raw.waiting_reason != reason
        {
            out.push("waiting_reason".to_string());
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncOutcome {
    pub item_id: String,
    pub identifier: String,
    pub version: i64,
    pub run: RunSyncView,
    pub last_run_sync_at: Option<String>,
    pub last_progress_at: Option<String>,
    pub note: Option<String>,
}

impl Store {
    pub fn create_item(
        &mut self,
        project: Option<&str>,
        new: NewItem,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<CreateOutcome> {
        let title = new.title.trim().to_string();
        if title.is_empty() {
            return Err(Error::invalid("title must be non-empty"));
        }
        new.context.validate()?;
        let item_type =
            super::workflows::item_type_key(new.item_type.as_deref().unwrap_or("task"))?;
        let priority = normalise_choice(
            new.priority.as_deref().unwrap_or("normal"),
            &PRIORITIES,
            "priority",
        )?;
        if let Some(parent) = &new.parent
            && parent.trim().is_empty()
        {
            return Err(Error::invalid("parent selector must be non-empty"));
        }
        let project_row = self.require_project(project)?;
        // Snapshot the workflow for this item type now: later re-binding must not
        // reinterpret an existing item's state machine.
        let workflow = self.workflow_for_type(&project_row.id, &item_type)?;
        let initial_state = workflow.initial_state().clone();
        let initial_category = initial_state.category.as_str().to_string();
        let actor = new.actor.clone();
        let idem = Idem::new("item.create", idempotency_key).request(json!({
            "title": title,
            "context_hash": new.context.hash(),
            "item_type": item_type,
            "workflow_id": workflow.id,
            "workflow_version": workflow.version,
            "priority": priority,
            "parent": new.parent,
            "depends_on": new.depends_on,
            "labels": new.labels,
            "acceptance": new.acceptance,
            "actor": actor.name,
            "actor_kind": actor.kind.as_str(),
            "session": actor.session_id,
            "run": actor.run_id,
        }));

        self.idempotent(idem, |tx| {
            let parent = match &new.parent {
                Some(sel) => {
                    let parent = require_raw_item(tx, sel)?;
                    if parent.project_id != project_row.id {
                        return Err(Error::invalid("parent belongs to another project"));
                    }
                    if parent.state_category == Category::Closed.as_str() {
                        return Err(Error::invalid("cannot nest under a closed item"));
                    }
                    Some(parent.id)
                }
                None => None,
            };

            let number: i64 = tx.query_row(
                "SELECT COALESCE(MAX(number), 0) + 1 FROM work_items WHERE project_id = ?1",
                params![project_row.id],
                |r| r.get(0),
            )?;
            let identifier = format!("{}-{}", project_row.prefix, number);
            let id = uuid::Uuid::new_v4().to_string();
            let doc_hash = new.context.hash();

            tx.execute(
                "INSERT INTO work_items
                 (id, project_id, number, identifier, title, item_type, status, state_category,
                  workflow_id, workflow_version, priority, parent_id,
                  version, context_version, progress_summary, next_action, blocker,
                  waiting_on_human, waiting_reason, completion_note, created_by, created_at,
                  updated_at, last_heartbeat_at, last_progress_at, last_run_sync_at, done_at, cancelled_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                         1, 1, NULL, NULL, NULL, 0, NULL, NULL, ?13, ?14, ?14, NULL, NULL, NULL, NULL, NULL)",
                params![
                    id,
                    project_row.id,
                    number,
                    identifier,
                    title,
                    item_type,
                    initial_state.key,
                    initial_category,
                    workflow.id,
                    workflow.version,
                    priority,
                    parent,
                    actor.name,
                    at
                ],
            )?;

            insert_context_version(
                tx,
                &id,
                1,
                &new.context,
                &doc_hash,
                None,
                "initial context",
                &actor,
                at,
            )?;

            for label in &new.labels {
                let label = label.trim();
                if label.is_empty() {
                    return Err(Error::invalid("labels must be non-empty"));
                }
                super::misc::attach_label(tx, &project_row.id, &id, label, &actor, at)?;
            }

            for (seq, criterion) in new.acceptance.iter().enumerate() {
                let text = criterion.trim();
                if text.is_empty() {
                    return Err(Error::invalid("acceptance criteria must be non-empty"));
                }
                tx.execute(
                    "INSERT INTO acceptance_criteria (work_item_id, seq, text, status)
                     VALUES (?1, ?2, ?3, 'open')",
                    params![id, (seq as i64) + 1, text],
                )?;
            }

            for dep in &new.depends_on {
                let dep = require_raw_item(tx, dep)?;
                if dep.id == id {
                    return Err(Error::Dependency("an item cannot depend on itself".into()));
                }
                if dep.project_id != project_row.id {
                    return Err(Error::Dependency(
                        "dependencies must live in the same project".into(),
                    ));
                }
                tx.execute(
                    "INSERT OR IGNORE INTO dependencies (work_item_id, depends_on_id, created_at, created_by)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![id, dep.id, at, actor.name],
                )?;
            }

            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&id),
                    kind: "item_created",
                    actor: &actor,
                    prev_version: None,
                    new_version: Some(1),
                    idempotency_key,
                    payload: json!({
                        "identifier": identifier,
                        "title": title,
                        "context_hash": doc_hash,
                    }),
                },
            )?;

            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&id),
                    kind: "context_revised",
                    actor: &actor,
                    prev_version: Some(1),
                    new_version: Some(1),
                    idempotency_key,
                    payload: json!({
                        "context_version": 1,
                        "reason": "initial context",
                        "doc_hash": doc_hash,
                        "snapshot": new.context,
                    }),
                },
            )?;

            let raw = require_raw_item(tx, &id)?;
            let item = hydrate_item(tx, &raw, at, &project_row.settings)?;
            let context = context_version(tx, &id, 1)?.view();
            Ok(CreateOutcome { item, context })
        })
    }

    pub fn get_item(&self, selector: &str, at: i64) -> Result<ItemDetail> {
        let raw = require_raw_item(&self.conn, selector)?;
        let settings = self.settings_for(&raw.project_id)?;
        let item = hydrate_item(&self.conn, &raw, at, &settings)?;
        let context = context_version(&self.conn, &raw.id, raw.context_version)?.view();
        let acceptance = acceptance_of(&self.conn, &raw.id)?;
        let evidence = evidence_of(&self.conn, &raw.id)?;
        let attention = super::misc::attention_of(&self.conn, Some(&raw.id))?;
        let history_len: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events WHERE work_item_id = ?1",
            params![raw.id],
            |r| r.get(0),
        )?;
        Ok(ItemDetail {
            item,
            context,
            acceptance,
            evidence,
            attention,
            history_len,
        })
    }

    pub fn list_items(&self, filter: ListFilter, at: i64) -> Result<Vec<WorkItemView>> {
        let project_row = self.require_project(filter.project.as_deref())?;
        let mut sql = format!("{ITEM_SELECT} WHERE i.project_id = ?1");
        let mut args: Vec<Value> = vec![Value::Text(project_row.id.clone())];

        if !filter.status.is_empty() {
            let placeholders = vec!["?"; filter.status.len()].join(", ");
            sql.push_str(&format!(" AND i.status IN ({placeholders})"));
            for s in &filter.status {
                args.push(Value::Text(s.as_str().to_string()));
            }
        } else if !filter.include_closed {
            // Closed is a workflow category, not a hard-coded pair of keys.
            sql.push_str(" AND i.state_category != 'closed'");
        }
        if let Some(parent) = &filter.parent {
            let parent = require_raw_item(&self.conn, parent)?;
            sql.push_str(" AND i.parent_id = ?");
            args.push(Value::Text(parent.id));
        }
        if let Some(label) = &filter.label {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM item_labels il JOIN labels l ON l.id = il.label_id
                              WHERE il.work_item_id = i.id AND l.name = ?)",
            );
            args.push(Value::Text(label.clone()));
        }
        if let Some(session) = &filter.session {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM claims c WHERE c.work_item_id = i.id
                              AND c.status = 'active' AND c.session_id = ?)",
            );
            args.push(Value::Text(session.clone()));
        }
        if let Some(agent) = &filter.agent {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM claims c WHERE c.work_item_id = i.id
                              AND c.status = 'active' AND c.holder = ?)",
            );
            args.push(Value::Text(agent.clone()));
        }
        if let Some(keywords) = &filter.keywords {
            // `LIKE` treats `%` and `_` as wildcards and `\` as the escape char;
            // a keyword search must match the literal text the user typed.
            sql.push_str(" AND (i.title LIKE ? ESCAPE '\\' OR i.identifier LIKE ? ESCAPE '\\')");
            let like = format!("%{}%", escape_like(keywords.trim()));
            args.push(Value::Text(like.clone()));
            args.push(Value::Text(like));
        }
        if filter.attention_only {
            sql.push_str(
                " AND EXISTS (SELECT 1 FROM attention_requests a WHERE a.work_item_id = i.id
                              AND a.status = 'open')",
            );
        }
        sql.push_str(" ORDER BY i.number DESC LIMIT ?");
        args.push(Value::Integer(if filter.limit <= 0 {
            200
        } else {
            filter.limit
        }));

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(args), RawItem::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        hydrate_items(&self.conn, rows, at, &project_row.settings)
    }

    #[allow(clippy::too_many_arguments)] // CLI-shaped write API; a param struct would just reorder noise
    pub fn update_item(
        &mut self,
        selector: &str,
        token: Option<&str>,
        expect_version: Option<i64>,
        patch: ItemPatch,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<WorkItemView> {
        let (project_row, item_id) = {
            let raw = require_raw_item(&self.conn, selector)?;
            (self.require_project(Some(&raw.project_id))?, raw.id.clone())
        };
        let idem = Idem::new("item.update", idempotency_key)
            .item(Some(&item_id))
            .request(json!({
                "title": patch.title,
                "item_type": patch.item_type,
                "priority": patch.priority,
                "next_action": patch.next_action,
                "blocker": patch.blocker,
                "waiting_on_human": patch.waiting_on_human,
                "waiting_reason": patch.waiting_reason,
                "parent": patch.parent,
                "expect_version": expect_version,
                "actor": actor.name,
                "session": actor.session_id,
            }));
        self.idempotent(idem, |tx| {
            let mut raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            check_version(&raw, expect_version)?;
            let before = raw.clone();

            if let Some(title) = &patch.title {
                let title = title.trim();
                if title.is_empty() {
                    return Err(Error::invalid("title must be non-empty"));
                }
                raw.title = title.to_string();
            }
            if let Some(item_type) = &patch.item_type {
                // Same slug rules as create. Changing the type does *not* migrate
                // the item to another workflow: the snapshot it was created with
                // stays authoritative (documented in README and covered by
                // `item_type_changes_do_not_reinterpret_the_workflow_snapshot`).
                raw.item_type = super::workflows::item_type_key(item_type)?;
            }
            if let Some(priority) = &patch.priority {
                raw.priority = normalise_choice(priority, &PRIORITIES, "priority")?;
            }
            if let Some(next) = &patch.next_action {
                raw.next_action = normalise_opt(next.clone());
            }
            if let Some(blocker) = &patch.blocker {
                raw.blocker = normalise_opt(blocker.clone());
            }
            if let Some(waiting) = patch.waiting_on_human {
                raw.waiting_on_human = waiting;
            }
            if let Some(reason) = &patch.waiting_reason {
                raw.waiting_reason = normalise_opt(reason.clone());
            }
            if let Some(parent) = &patch.parent {
                raw.parent_id = match parent {
                    Some(sel) => Some(resolve_parent_link(tx, &raw, sel)?),
                    None => None,
                };
            }

            let fields = changed_fields(&before, &raw);
            if fields.is_empty() {
                return Err(Error::invalid(
                    "no fields changed; refusing to bump the version for an empty update",
                ));
            }
            raw.version = before.version + 1;
            write_item(tx, &raw, at)?;
            let new_version = raw.version;

            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "item_updated",
                    actor,
                    prev_version: Some(before.version),
                    new_version: Some(new_version),
                    idempotency_key,
                    payload: json!({ "fields": fields }),
                },
            )?;

            let raw = require_raw_item(tx, &raw.id)?;
            hydrate_item(tx, &raw, at, &project_row.settings)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_status(
        &mut self,
        selector: &str,
        token: Option<&str>,
        expect_version: Option<i64>,
        to: Status,
        reason: Option<&str>,
        note: Option<&str>,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<WorkItemView> {
        let (project_row, item_id) = {
            let raw = require_raw_item(&self.conn, selector)?;
            (self.require_project(Some(&raw.project_id))?, raw.id.clone())
        };
        let idem = Idem::new("item.status", idempotency_key)
            .item(Some(&item_id))
            .request(json!({
                "to": to.as_str(),
                "reason": reason,
                "note": note,
                "expect_version": expect_version,
                "actor": actor.name,
                "session": actor.session_id,
            }));
        self.idempotent(idem, |tx| {
            let mut raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            check_version(&raw, expect_version)?;
            if raw.status == to {
                return Err(Error::WorkflowViolation(format!(
                    "item is already in status '{}'",
                    to.as_str()
                )));
            }

            // Read the snapshot *inside* `BEGIN IMMEDIATE`: the transition guard
            // and the write it authorises must come from the same view of the
            // workflow, never from a pre-transaction read that another process
            // could have re-bound in between.
            let workflow = super::workflows::load_workflow_on(
                tx,
                &raw.project_id,
                &raw.workflow_id,
                raw.workflow_version,
            )?;
            let Some(target) = workflow.state(to.as_str()) else {
                return Err(Error::WorkflowViolation(format!(
                    "state '{}' is not part of this item's workflow {} v{}",
                    to, workflow.id, workflow.version
                )));
            };
            let met_acceptance: i64 = tx.query_row(
                "SELECT COUNT(*) FROM acceptance_criteria WHERE work_item_id = ?1 AND status IN ('met', 'waived')",
                params![raw.id],
                |r| r.get(0),
            )?;
            let evidence: i64 = tx.query_row(
                "SELECT COUNT(*) FROM evidence WHERE work_item_id = ?1",
                params![raw.id],
                |r| r.get(0),
            )?;
            let guard_ctx = GuardContext {
                from: raw.status.clone(),
                to: to.clone(),
                reason,
                note,
                progress_summary: raw.progress_summary.as_deref(),
                completion_note: raw.completion_note.as_deref().or(note),
                met_acceptance,
                evidence_count: evidence,
                acceptance_total: acceptance_of(tx, &raw.id)?.len() as i64,
            };
            workflow.check_transition(&guard_ctx)?;

            let before_status = raw.status.clone();
            let before_category = raw.state_category.clone();
            let before_version = raw.version;
            raw.status = to.clone();
            raw.state_category = target.category.as_str().to_string();
            raw.version += 1;
            raw.updated_at = at;
            if target.category.is_closed() {
                if target.cancel {
                    raw.cancelled_at = Some(at);
                    raw.done_at = None;
                } else {
                    raw.done_at = Some(at);
                    raw.cancelled_at = None;
                }
                raw.waiting_on_human = false;
                // A new completion note replaces the old one: otherwise a reopen
                // followed by a second done would keep reporting the first
                // completion's note (and the guard would see stale text).
                if let Some(note) = note {
                    raw.completion_note = Some(note.to_string());
                }
            } else {
                raw.done_at = None;
                raw.cancelled_at = None;
            }
            write_item(tx, &raw, at)?;

            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "status_changed",
                    actor,
                    prev_version: Some(before_version),
                    new_version: Some(raw.version),
                    idempotency_key,
                    payload: json!({
                        "from": before_status.as_str(),
                        "to": to.as_str(),
                        "from_category": before_category,
                        "to_category": target.category.as_str(),
                        "workflow_id": workflow.id,
                        "workflow_version": workflow.version,
                        "terminal": target.terminal,
                        "reason": reason,
                        "note": note,
                        "met_acceptance": met_acceptance,
                        "evidence_count": evidence,
                    }),
                },
            )?;

            // Closing an item releases the claim so finished work cannot keep a
            // zombie lease alive. Reopening requires a fresh claim, by design.
            if target.category.is_closed()
                && let Some(claim) = active_claim(tx, &raw.id)?
            {
                {
                    tx.execute(
                        "UPDATE claims SET status = 'released', released_at = ?1, release_reason = ?2
                         WHERE id = ?3",
                        params![at, format!("closed:{}", to.as_str()), claim.id],
                    )?;
                    super::events::append_event(
                        tx,
                        super::events::EventInput {
                            at,
                            project_id: &project_row.id,
                            work_item_id: Some(&raw.id),
                            kind: "claim_released",
                            actor,
                            prev_version: None,
                            new_version: None,
                            idempotency_key,
                            payload: json!({ "claim_id": claim.id, "reason": format!("closed:{}", to.as_str()) }),
                        },
                    )?;
                }
            }

            let raw = require_raw_item(tx, &raw.id)?;
            hydrate_item(tx, &raw, at, &project_row.settings)
        })
    }

    /// Record a *substantive* progress update. Bumps the item version, moves
    /// `last_progress_at`, and is the only operation (besides completion) that
    /// may claim something actually advanced.
    #[allow(clippy::too_many_arguments)]
    pub fn record_progress(
        &mut self,
        selector: &str,
        token: Option<&str>,
        expect_version: Option<i64>,
        summary: &str,
        next_action: Option<Option<String>>,
        blocker: Option<Option<String>>,
        waiting_on_human: Option<bool>,
        waiting_reason: Option<Option<String>>,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<ProgressOutcome> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err(Error::invalid(
                "progress summary must be non-empty (use `apm sync` for liveness only)",
            ));
        }
        let (project_row, item_id) = {
            let raw = require_raw_item(&self.conn, selector)?;
            (self.require_project(Some(&raw.project_id))?, raw.id.clone())
        };
        let requested = ProgressRequest {
            summary: summary.to_string(),
            next_action: next_action.clone().map(normalise_opt),
            blocker: blocker.clone().map(normalise_opt),
            waiting_on_human,
            waiting_reason: waiting_reason.clone().map(normalise_opt),
        };
        let idem = Idem::new("item.progress", idempotency_key)
            .item(Some(&item_id))
            .request(json!({
                "summary": requested.summary,
                "next_action": requested.next_action,
                "blocker": requested.blocker,
                "waiting_on_human": requested.waiting_on_human,
                "waiting_reason": requested.waiting_reason,
                "expect_version": expect_version,
                "actor": actor.name,
                "session": actor.session_id,
            }));

        self.idempotent(idem, |tx| {
            let mut raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            check_version(&raw, expect_version)?;

            // A repeated summary is not new progress. Refuse to move the
            // substantive-progress clock (or the version, or the audit trail)
            // when nothing requested actually differs from the current state —
            // otherwise a side-run could manufacture "progress" by repeating
            // itself, and a retry loop would look like a productive agent.
            let changed = requested.changed_fields(&raw);
            if changed.is_empty() {
                let view = hydrate_item(tx, &raw, at, &project_row.settings)?;
                return Ok(ProgressOutcome {
                    item: view,
                    no_op: true,
                    substantive: false,
                    changed_fields: Vec::new(),
                });
            }

            // Substantive = the summary itself is new. Accompanying fields do not
            // change that: new net/next/blocker text with a repeated summary is
            // bookkeeping, not progress.
            let substantive = raw.progress_summary.as_deref() != Some(summary);

            let before_version = raw.version;
            let previous = raw.progress_summary.clone();
            raw.progress_summary = Some(requested.summary.clone());
            if let Some(next) = &requested.next_action {
                raw.next_action = next.clone();
            }
            if let Some(blocker) = &requested.blocker {
                raw.blocker = blocker.clone();
            }
            if let Some(waiting) = requested.waiting_on_human {
                raw.waiting_on_human = waiting;
            }
            if let Some(reason) = &requested.waiting_reason {
                raw.waiting_reason = reason.clone();
            }
            if substantive {
                raw.last_progress_at = Some(at);
                raw.last_run_sync_at = Some(at);
            }
            raw.updated_at = at;
            raw.version = before_version + 1;

            tx.execute(
                "UPDATE work_items SET progress_summary = ?1, next_action = ?2, blocker = ?3,
                    waiting_on_human = ?4, waiting_reason = ?5, last_progress_at = ?6,
                    last_run_sync_at = ?7, updated_at = ?8, version = ?9
                 WHERE id = ?10",
                params![
                    raw.progress_summary,
                    raw.next_action,
                    raw.blocker,
                    raw.waiting_on_human as i64,
                    raw.waiting_reason,
                    raw.last_progress_at,
                    raw.last_run_sync_at,
                    raw.updated_at,
                    raw.version,
                    raw.id
                ],
            )?;

            let kind = if substantive {
                "progress_recorded"
            } else {
                "progress_updated"
            };
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind,
                    actor,
                    prev_version: Some(before_version),
                    new_version: Some(raw.version),
                    idempotency_key,
                    payload: json!({
                        "summary": summary,
                        "previous_summary": previous,
                        "next_action": raw.next_action,
                        "blocker": raw.blocker,
                        "waiting_on_human": raw.waiting_on_human,
                        "substantive": substantive,
                        "last_progress_advanced": substantive,
                        "changed_fields": changed,
                    }),
                },
            )?;

            let raw = require_raw_item(tx, &raw.id)?;
            Ok(ProgressOutcome {
                item: hydrate_item(tx, &raw, at, &project_row.settings)?,
                no_op: false,
                substantive,
                changed_fields: changed,
            })
        })
    }

    /// End-of-run liveness sync. Deliberately does **not** touch
    /// `last_progress_at`, does not change business status, and does not bump
    /// `version`: it proves the run happened and renews the lease, nothing more.
    #[allow(clippy::too_many_arguments)]
    pub fn sync_run(
        &mut self,
        selector: &str,
        token: Option<&str>,
        outcome: RunOutcome,
        note: Option<&str>,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<SyncOutcome> {
        let (project_row, item_id) = {
            let raw = require_raw_item(&self.conn, selector)?;
            (self.require_project(Some(&raw.project_id))?, raw.id.clone())
        };
        let idem = Idem::new("item.sync", idempotency_key)
            .item(Some(&item_id))
            .request(json!({
                "outcome": outcome.as_str(),
                "note": note,
                "actor": actor.name,
                "session": actor.session_id,
                "run": actor.run_id,
            }));
        self.idempotent(idem, |tx| {
            let raw = require_raw_item(tx, selector)?;
            let claim = require_write_claim(tx, &raw, token, actor, at)?;

            // Run sync is an explicit liveness ping from the holder, so it
            // renews the lease. `last_progress_at` stays untouched. The renewed
            // window is the claim's own lease: a caller that asked for a long
            // lease must not be shortened to the project default by a sync.
            if let Some(claim) = &claim {
                tx.execute(
                    "UPDATE claims SET heartbeat_at = ?1, expires_at = ?2 WHERE id = ?3",
                    params![at, at + claim.lease_seconds * 1000, claim.id],
                )?;
                tx.execute(
                    "UPDATE work_items SET last_heartbeat_at = ?1 WHERE id = ?2",
                    params![at, raw.id],
                )?;
            }
            tx.execute(
                "UPDATE work_items SET last_run_sync_at = ?1, updated_at = ?2 WHERE id = ?3",
                params![at, at, raw.id],
            )?;

            let kind = match outcome {
                RunOutcome::Aborted => "run_aborted",
                _ => "run_synced",
            };
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind,
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key,
                    payload: json!({
                        "outcome": outcome.as_str(),
                        "note": note,
                        "progress_untouched": true,
                        "lease_renewed": claim.is_some(),
                    }),
                },
            )?;

            let raw = require_raw_item(tx, &raw.id)?;
            Ok(SyncOutcome {
                item_id: raw.id.clone(),
                identifier: raw.identifier.clone(),
                version: raw.version,
                run: RunSyncView {
                    run_id: actor.run_id.clone(),
                    outcome: outcome.as_str().to_string(),
                    at: ts(at),
                    note: note.map(str::to_string),
                },
                last_run_sync_at: ts_opt(raw.last_run_sync_at),
                last_progress_at: ts_opt(raw.last_progress_at),
                note: note.map(str::to_string),
            })
        })
    }

    /// Rename the item's business status is separate from health: a heartbeat
    /// never changes status. Exposed for completeness.
    pub fn heartbeat_for_item(&self, selector: &str) -> Result<Option<ClaimView>> {
        let raw = require_raw_item(&self.conn, selector)?;
        Ok(active_claim(&self.conn, &raw.id)?.map(|c| c.view(&raw.identifier, now_ms())))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Aborted,
    Unknown,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Aborted => "aborted",
            RunOutcome::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Result<RunOutcome> {
        match s.trim().to_ascii_lowercase().as_str() {
            "completed" | "complete" | "ok" => Ok(RunOutcome::Completed),
            "aborted" | "abort" | "interrupted" => Ok(RunOutcome::Aborted),
            "unknown" => Ok(RunOutcome::Unknown),
            other => Err(Error::invalid(format!(
                "unknown run outcome '{other}' (expected completed, aborted, unknown)"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn check_version(raw: &RawItem, expect: Option<i64>) -> Result<()> {
    match expect {
        Some(v) if v != raw.version => Err(Error::VersionConflict {
            expected: v,
            actual: raw.version,
        }),
        _ => Ok(()),
    }
}

pub(crate) fn normalise_opt(value: Option<String>) -> Option<String> {
    match value {
        Some(v) if v.trim().is_empty() => None,
        Some(v) => Some(v.trim().to_string()),
        None => None,
    }
}

fn normalise_choice(value: &str, allowed: &[&str], what: &str) -> Result<String> {
    let v = value.trim().to_ascii_lowercase();
    if !allowed.contains(&v.as_str()) {
        return Err(Error::invalid(format!(
            "unknown {what} '{value}' (expected one of: {})",
            allowed.join(", ")
        )));
    }
    Ok(v)
}

/// Persist every mutable column of `raw` and bump its version.
fn write_item(tx: &Transaction<'_>, raw: &RawItem, at: i64) -> Result<()> {
    tx.execute(
        "UPDATE work_items SET title = ?1, item_type = ?2, priority = ?3, parent_id = ?4,
            version = ?5, progress_summary = ?6, next_action = ?7, blocker = ?8,
            waiting_on_human = ?9, waiting_reason = ?10, completion_note = ?11,
            updated_at = ?12, done_at = ?13, cancelled_at = ?14, status = ?15,
            state_category = ?16, workflow_id = ?17, workflow_version = ?18
         WHERE id = ?19",
        params![
            raw.title,
            raw.item_type,
            raw.priority,
            raw.parent_id,
            raw.version,
            raw.progress_summary,
            raw.next_action,
            raw.blocker,
            raw.waiting_on_human as i64,
            raw.waiting_reason,
            raw.completion_note,
            at.max(raw.updated_at),
            raw.done_at,
            raw.cancelled_at,
            raw.status.as_str(),
            raw.state_category,
            raw.workflow_id,
            raw.workflow_version,
            raw.id
        ],
    )?;
    Ok(())
}

/// Validate a new parent link for `child`, returning the parent id.
///
/// Rejects a closed parent (finished work is not a container) and walks the
/// whole ancestor chain, so reparenting cannot close a cycle of any depth — not
/// just the direct self-parent case the old check covered.
fn resolve_parent_link(
    tx: &rusqlite::Connection,
    child: &RawItem,
    selector: &str,
) -> Result<String> {
    let parent = require_raw_item(tx, selector)?;
    if parent.id == child.id {
        return Err(Error::invalid("an item cannot be its own parent"));
    }
    if parent.project_id != child.project_id {
        return Err(Error::invalid("parent belongs to another project"));
    }
    if parent.state_category == Category::Closed.as_str() {
        return Err(Error::invalid("cannot nest under a closed item"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor = parent.parent_id.clone();
    while let Some(ancestor_id) = cursor {
        if ancestor_id == child.id {
            return Err(Error::invalid("parent chain would create a cycle"));
        }
        if !seen.insert(ancestor_id.clone()) {
            // A cycle already exists upstream (corrupt/legacy data): fail closed
            // instead of looping forever.
            return Err(Error::invalid("parent chain already contains a cycle"));
        }
        cursor = tx
            .query_row(
                "SELECT parent_id FROM work_items WHERE id = ?1",
                params![ancestor_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
    }
    Ok(parent.id)
}

/// Escape SQLite `LIKE` metacharacters so a keyword is matched literally.
/// The caller pairs this with `ESCAPE '\'`.
fn escape_like(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn changed_fields(before: &RawItem, after: &RawItem) -> Vec<&'static str> {
    let mut out = Vec::new();
    if before.title != after.title {
        out.push("title");
    }
    if before.item_type != after.item_type {
        out.push("item_type");
    }
    if before.priority != after.priority {
        out.push("priority");
    }
    if before.parent_id != after.parent_id {
        out.push("parent");
    }
    if before.next_action != after.next_action {
        out.push("next_action");
    }
    if before.blocker != after.blocker {
        out.push("blocker");
    }
    if before.waiting_on_human != after.waiting_on_human {
        out.push("waiting_on_human");
    }
    if before.waiting_reason != after.waiting_reason {
        out.push("waiting_reason");
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_context_version(
    tx: &Transaction<'_>,
    item_id: &str,
    version: i64,
    doc: &ContextDoc,
    doc_hash: &str,
    revises_version: Option<i64>,
    reason: &str,
    actor: &Actor,
    at: i64,
) -> Result<()> {
    tx.execute(
        "INSERT INTO context_versions
         (work_item_id, version, goal, current_state, constraints_json, references_json, notes,
          doc_hash, revises_version, reason, author, actor_kind, session_id, run_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            item_id,
            version,
            doc.goal,
            doc.current_state,
            serde_json::to_string(&doc.constraints)?,
            serde_json::to_string(&doc.references)?,
            doc.notes,
            doc_hash,
            revises_version,
            reason,
            actor.name,
            actor.kind.as_str(),
            actor.session_id,
            actor.run_id,
            at
        ],
    )?;
    Ok(())
}

pub(crate) fn context_version(
    conn: &rusqlite::Connection,
    item_id: &str,
    version: i64,
) -> Result<RawContext> {
    let sql = format!("{CONTEXT_SELECT} WHERE work_item_id = ?1 AND version = ?2");
    conn.query_row(&sql, params![item_id, version], RawContext::from_row)
        .optional()?
        .ok_or_else(|| Error::Internal(format!("context v{version} missing for item {item_id}")))
}
