//! Labels, dependencies, acceptance criteria, evidence, and the structured
//! attention-request channel ("waiting for a human").
//!
//! All item mutations here require a valid claim (humans may act without one)
//! and bump the item version, so a concurrent agent cannot lose an update.

use rusqlite::{OptionalExtension, params};
use serde_json::json;

use super::claims::require_write_claim;
use super::*;

pub const EVIDENCE_KINDS: [&str; 6] = ["test", "commit", "link", "artifact", "note", "review"];
pub const ACCEPTANCE_STATES: [&str; 3] = ["open", "met", "waived"];

#[derive(Debug, Clone)]
pub struct AttentionRequest {
    pub item: Option<String>,
    pub question: String,
    pub context: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    pub blocking: bool,
}

impl Store {
    pub fn add_label(
        &mut self,
        selector: &str,
        label: &str,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
    ) -> Result<WorkItemView> {
        let label = label.trim().to_ascii_lowercase();
        if label.is_empty() {
            return Err(Error::invalid("label must be non-empty"));
        }
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        self.with_tx(|tx| {
            let raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            let before = raw.version;
            attach_label(tx, &project_row.id, &raw.id, &label, actor, at)?;
            bump_version(tx, &raw.id, at)?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "label_added",
                    actor,
                    prev_version: Some(before),
                    new_version: Some(before + 1),
                    idempotency_key: None,
                    payload: json!({ "label": label }),
                },
            )?;
            let raw = require_raw_item(tx, &raw.id)?;
            hydrate_item(tx, &raw, at, &project_row.settings)
        })
    }

    pub fn remove_label(
        &mut self,
        selector: &str,
        label: &str,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
    ) -> Result<WorkItemView> {
        let label = label.trim().to_ascii_lowercase();
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        self.with_tx(|tx| {
            let raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            let before = raw.version;
            let changed = tx.execute(
                "DELETE FROM item_labels WHERE work_item_id = ?1 AND label_id IN
                 (SELECT id FROM labels WHERE project_id = ?2 AND name = ?3)",
                params![raw.id, project_row.id, label],
            )?;
            if changed == 0 {
                return Err(Error::NotFound {
                    entity: "label",
                    id: label,
                });
            }
            bump_version(tx, &raw.id, at)?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "label_removed",
                    actor,
                    prev_version: Some(before),
                    new_version: Some(before + 1),
                    idempotency_key: None,
                    payload: json!({ "label": label }),
                },
            )?;
            let raw = require_raw_item(tx, &raw.id)?;
            hydrate_item(tx, &raw, at, &project_row.settings)
        })
    }

    pub fn add_dependency(
        &mut self,
        selector: &str,
        depends_on: &str,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
    ) -> Result<WorkItemView> {
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        self.with_tx(|tx| {
            let raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            let dep = require_raw_item(tx, depends_on)?;
            if dep.id == raw.id {
                return Err(Error::Dependency("an item cannot depend on itself".into()));
            }
            if dep.project_id != raw.project_id {
                return Err(Error::Dependency(
                    "dependencies must live in the same project".into(),
                ));
            }
            if would_cycle(tx, &raw.id, &dep.id)? {
                return Err(Error::Dependency(format!(
                    "adding {} -> {} would create a cycle",
                    raw.identifier, dep.identifier
                )));
            }
            let before = raw.version;
            tx.execute(
                "INSERT OR IGNORE INTO dependencies (work_item_id, depends_on_id, created_at, created_by)
                 VALUES (?1, ?2, ?3, ?4)",
                params![raw.id, dep.id, at, actor.name],
            )?;
            bump_version(tx, &raw.id, at)?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "dependency_added",
                    actor,
                    prev_version: Some(before),
                    new_version: Some(before + 1),
                    idempotency_key: None,
                    payload: json!({ "depends_on": dep.identifier }),
                },
            )?;
            let raw = require_raw_item(tx, &raw.id)?;
            hydrate_item(tx, &raw, at, &project_row.settings)
        })
    }

    pub fn remove_dependency(
        &mut self,
        selector: &str,
        depends_on: &str,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
    ) -> Result<WorkItemView> {
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        self.with_tx(|tx| {
            let raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            let dep = require_raw_item(tx, depends_on)?;
            let before = raw.version;
            let changed = tx.execute(
                "DELETE FROM dependencies WHERE work_item_id = ?1 AND depends_on_id = ?2",
                params![raw.id, dep.id],
            )?;
            if changed == 0 {
                return Err(Error::NotFound {
                    entity: "dependency",
                    id: dep.identifier,
                });
            }
            bump_version(tx, &raw.id, at)?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "dependency_removed",
                    actor,
                    prev_version: Some(before),
                    new_version: Some(before + 1),
                    idempotency_key: None,
                    payload: json!({ "depends_on": dep.identifier }),
                },
            )?;
            let raw = require_raw_item(tx, &raw.id)?;
            hydrate_item(tx, &raw, at, &project_row.settings)
        })
    }

    pub fn add_acceptance(
        &mut self,
        selector: &str,
        text: &str,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
    ) -> Result<Vec<AcceptanceView>> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::invalid("acceptance criterion must be non-empty"));
        }
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        self.with_tx(|tx| {
            let raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            let seq: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM acceptance_criteria WHERE work_item_id = ?1",
                params![raw.id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO acceptance_criteria (work_item_id, seq, text, status, updated_by, updated_at)
                 VALUES (?1, ?2, ?3, 'open', ?4, ?5)",
                params![raw.id, seq, text, actor.name, at],
            )?;
            bump_version(tx, &raw.id, at)?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "acceptance_added",
                    actor,
                    prev_version: Some(raw.version),
                    new_version: Some(raw.version + 1),
                    idempotency_key: None,
                    payload: json!({ "seq": seq, "text": text }),
                },
            )?;
            acceptance_of(tx, &raw.id)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn set_acceptance(
        &mut self,
        selector: &str,
        seq: i64,
        status: &str,
        note: Option<&str>,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
    ) -> Result<Vec<AcceptanceView>> {
        let status = status.trim().to_ascii_lowercase();
        if !ACCEPTANCE_STATES.contains(&status.as_str()) {
            return Err(Error::invalid(format!(
                "unknown acceptance state '{status}' (expected open, met, waived)"
            )));
        }
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        self.with_tx(|tx| {
            let raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            let changed = tx.execute(
                "UPDATE acceptance_criteria SET status = ?1, note = ?2, updated_by = ?3, updated_at = ?4
                 WHERE work_item_id = ?5 AND seq = ?6",
                params![status, note, actor.name, at, raw.id, seq],
            )?;
            if changed == 0 {
                return Err(Error::NotFound {
                    entity: "acceptance criterion",
                    id: format!("{}#{}", raw.identifier, seq),
                });
            }
            bump_version(tx, &raw.id, at)?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "acceptance_updated",
                    actor,
                    prev_version: Some(raw.version),
                    new_version: Some(raw.version + 1),
                    idempotency_key: None,
                    payload: json!({ "seq": seq, "status": status, "note": note }),
                },
            )?;
            acceptance_of(tx, &raw.id)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_evidence(
        &mut self,
        selector: &str,
        kind: &str,
        value: &str,
        note: Option<&str>,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<EvidenceView> {
        let kind = kind.trim().to_ascii_lowercase();
        if !EVIDENCE_KINDS.contains(&kind.as_str()) {
            return Err(Error::invalid(format!(
                "unknown evidence kind '{kind}' (expected {})",
                EVIDENCE_KINDS.join(", ")
            )));
        }
        let value = value.trim();
        if value.is_empty() {
            return Err(Error::invalid("evidence value must be non-empty"));
        }
        let (project_row, item_id) = {
            let raw = require_raw_item(&self.conn, selector)?;
            (self.require_project(Some(&raw.project_id))?, raw.id.clone())
        };
        let idem = Idem::new("item.evidence", idempotency_key)
            .item(Some(&item_id))
            .request(json!({
                "kind": kind,
                "value": value,
                "note": note,
                "actor": actor.name,
                "session": actor.session_id,
            }));
        self.idempotent(idem, |tx| {
            let raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO evidence (id, work_item_id, kind, value, note, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, raw.id, kind, value, note, actor.name, at],
            )?;
            bump_version(tx, &raw.id, at)?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "evidence_added",
                    actor,
                    prev_version: Some(raw.version),
                    new_version: Some(raw.version + 1),
                    idempotency_key,
                    payload: json!({ "evidence_id": id, "kind": kind, "value": value, "note": note }),
                },
            )?;
            let evidence = evidence_of(tx, &raw.id)?;
            evidence
                .into_iter()
                .find(|e| e.id == id)
                .ok_or_else(|| Error::Internal("evidence vanished after insert".into()))
        })
    }
}

// ---------------------------------------------------------------------------
// Attention requests
// ---------------------------------------------------------------------------

impl Store {
    /// Register a structured question for a human. Agents need a claim on the
    /// item (it is an item-scoped write: it changes what the team sees).
    pub fn request_attention(
        &mut self,
        project: Option<&str>,
        req: AttentionRequest,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<AttentionView> {
        if req.question.trim().is_empty() {
            return Err(Error::invalid("attention question must be non-empty"));
        }
        if req.context.trim().is_empty() {
            return Err(Error::invalid(
                "attention request must explain the situation in `context` so a human can decide without the chat log",
            ));
        }
        let project_row = self.require_project(project)?;
        // Resolve and project-check the item before opening the transaction: an
        // attention request must never be attached to another project's work.
        let resolved_item = match &req.item {
            Some(sel) => {
                let item = require_raw_item(&self.conn, sel)?;
                if item.project_id != project_row.id {
                    return Err(Error::invalid(format!(
                        "work item {} belongs to another project; pass --project accordingly",
                        item.identifier
                    )));
                }
                Some(item)
            }
            None => None,
        };
        let idem = Idem::new("attention.request", idempotency_key)
            .item(resolved_item.as_ref().map(|i| i.id.as_str()))
            .request(json!({
                "question": req.question,
                "context": req.context,
                "options": req.options,
                "recommendation": req.recommendation,
                "blocking": req.blocking,
                "item": resolved_item.as_ref().map(|i| i.id.clone()),
                "actor": actor.name,
                "actor_kind": actor.kind.as_str(),
                "session": actor.session_id,
                "run": actor.run_id,
            }));
        self.idempotent(idem, |tx| {
            let item = match &resolved_item {
                Some(resolved) => Some(require_raw_item(tx, &resolved.id)?),
                None => None,
            };
            if let Some(item) = &item {
                require_write_claim(tx, item, token, actor, at)?;
            }
            let id = uuid::Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO attention_requests
                 (id, project_id, work_item_id, question, context, options_json, recommendation,
                  blocking, status, response, requested_by, requested_kind, session_id, run_id,
                  created_at, resolved_at, resolved_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'open', NULL, ?9, ?10, ?11, ?12, ?13, NULL, NULL)",
                params![
                    id,
                    project_row.id,
                    item.as_ref().map(|i| i.id.clone()),
                    req.question.trim(),
                    req.context.trim(),
                    serde_json::to_string(&req.options)?,
                    req.recommendation,
                    req.blocking as i64,
                    actor.name,
                    actor.kind.as_str(),
                    actor.session_id,
                    actor.run_id,
                    at
                ],
            )?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: item.as_ref().map(|i| i.id.as_str()),
                    kind: "attention_requested",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key,
                    payload: json!({
                        "attention_id": id,
                        "question": req.question,
                        "blocking": req.blocking,
                        "options": req.options,
                    }),
                },
            )?;
            attention_one(tx, &id)
        })
    }

    /// Human answer. Deliberately human-only in v1: an agent must not resolve
    /// its own question to a human, and resolving is not an item mutation, so
    /// there is no claim to check against.
    pub fn reply_attention(
        &mut self,
        id: &str,
        response: &str,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<AttentionView> {
        if !actor.is_human() {
            return Err(Error::Forbidden(
                "only a human may answer an attention request; agents ask, humans decide".into(),
            ));
        }
        let response = response.trim();
        if response.is_empty() {
            return Err(Error::invalid("attention response must be non-empty"));
        }
        let current = attention_one(&self.conn, id)?;
        let project_row = self.require_project(Some(&current_project(&self.conn, &current)?))?;
        let idem = Idem::new("attention.reply", idempotency_key)
            .item(current.work_item_id.as_deref())
            .request(json!({
                "attention_id": id,
                "response": response,
                "actor": actor.name,
            }));
        self.idempotent(idem, |tx| {
            let changed = tx.execute(
                "UPDATE attention_requests SET status = 'resolved', response = ?1,
                    resolved_at = ?2, resolved_by = ?3
                 WHERE id = ?4 AND status = 'open'",
                params![response, at, actor.name, id],
            )?;
            if changed == 0 {
                return Err(Error::invalid(format!(
                    "attention request {id} is not open (already resolved or withdrawn)"
                )));
            }
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: current.work_item_id.as_deref(),
                    kind: "attention_replied",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key,
                    payload: json!({ "attention_id": id, "response": response }),
                },
            )?;
            attention_one(tx, id)
        })
    }

    /// Withdraw an open request.
    ///
    /// * humans may withdraw anything (management path);
    /// * project-level requests may be withdrawn by their requester;
    /// * item-scoped requests may be withdrawn by an agent only with a valid
    ///   claim on that item and the same session (or the same agent name) that
    ///   asked. Losing the claim means losing the right to retract.
    pub fn withdraw_attention(
        &mut self,
        id: &str,
        token: Option<&str>,
        actor: &Actor,
        at: i64,
    ) -> Result<AttentionView> {
        let current = attention_one(&self.conn, id)?;
        let project_row = self.require_project(Some(&current_project(&self.conn, &current)?))?;
        self.with_tx(|tx| {
            if !actor.is_human() {
                match &current.work_item_id {
                    Some(item_id) => {
                        let item = require_raw_item(tx, item_id)?;
                        require_write_claim(tx, &item, token, actor, at)?;
                        let same_session = current.session_id.is_some()
                            && current.session_id == actor.session_id;
                        let same_agent = current.requested_by == actor.name;
                        if !same_session && !same_agent {
                            return Err(Error::Forbidden(
                                "only the requesting agent (same session/name) or a human may \
                                 withdraw this attention request"
                                    .into(),
                            ));
                        }
                    }
                    None => {
                        if current.requested_by != actor.name
                            && current.session_id != actor.session_id
                        {
                            return Err(Error::Forbidden(
                                "only the requester or a human may withdraw this project-level \
                                 attention request"
                                    .into(),
                            ));
                        }
                    }
                }
            }
            let changed = tx.execute(
                "UPDATE attention_requests SET status = 'withdrawn', resolved_at = ?1, resolved_by = ?2
                 WHERE id = ?3 AND status = 'open'",
                params![at, actor.name, id],
            )?;
            if changed == 0 {
                return Err(Error::invalid(format!(
                    "attention request {id} is not open"
                )));
            }
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: current.work_item_id.as_deref(),
                    kind: "attention_withdrawn",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key: None,
                    payload: json!({ "attention_id": id }),
                },
            )?;
            attention_one(tx, id)
        })
    }

    /// Item a request is attached to (used by the CLI to resolve a write token).
    pub fn attention_target(&self, id: &str) -> Result<Option<(String, String)>> {
        let view = attention_one(&self.conn, id)?;
        match view.work_item_id {
            Some(item_id) => {
                let item = require_raw_item(&self.conn, &item_id)?;
                Ok(Some((item.id, item.identifier)))
            }
            None => Ok(None),
        }
    }

    pub fn list_attention(
        &self,
        project: Option<&str>,
        item: Option<&str>,
        status: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AttentionView>> {
        let project_row = self.require_project(project)?;
        let mut sql = String::from(
            "SELECT a.id, a.work_item_id, i.identifier, a.question, a.context, a.options_json,
                    a.recommendation, a.blocking, a.status, a.response, a.requested_by,
                    a.requested_kind, a.session_id, a.run_id, a.created_at, a.resolved_at,
                    a.resolved_by, a.project_id
             FROM attention_requests a
             LEFT JOIN work_items i ON i.id = a.work_item_id
             WHERE a.project_id = ?1",
        );
        let mut args: Vec<rusqlite::types::Value> =
            vec![rusqlite::types::Value::Text(project_row.id.clone())];
        if let Some(item) = item {
            let raw = require_raw_item(&self.conn, item)?;
            sql.push_str(" AND a.work_item_id = ?");
            args.push(rusqlite::types::Value::Text(raw.id));
        }
        if let Some(status) = status {
            sql.push_str(" AND a.status = ?");
            args.push(rusqlite::types::Value::Text(status.to_string()));
        }
        sql.push_str(" ORDER BY a.blocking DESC, a.created_at DESC LIMIT ?");
        args.push(rusqlite::types::Value::Integer(if limit <= 0 {
            100
        } else {
            limit
        }));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(args), attention_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

pub(crate) fn attention_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AttentionView> {
    let created: i64 = row.get(14)?;
    let resolved: Option<i64> = row.get(15)?;
    let options: String = row.get(5)?;
    let requested_kind: String = row.get(11)?;
    Ok(AttentionView {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        identifier: row.get(2)?,
        question: row.get(3)?,
        context: row.get(4)?,
        options: serde_json::from_str(&options).unwrap_or_default(),
        recommendation: row.get(6)?,
        blocking: row.get::<_, i64>(7)? != 0,
        status: row.get(8)?,
        response: row.get(9)?,
        requested_by: row.get(10)?,
        requested_kind: if requested_kind == "human" {
            ActorKind::Human
        } else {
            ActorKind::Agent
        },
        session_id: row.get(12)?,
        run_id: row.get(13)?,
        created_at: ts(created),
        resolved_at: ts_opt(resolved),
        resolved_by: row.get(16)?,
    })
}

fn attention_projection() -> &'static str {
    "SELECT a.id, a.work_item_id, i.identifier, a.question, a.context, a.options_json,
            a.recommendation, a.blocking, a.status, a.response, a.requested_by,
            a.requested_kind, a.session_id, a.run_id, a.created_at, a.resolved_at,
            a.resolved_by, a.project_id
     FROM attention_requests a
     LEFT JOIN work_items i ON i.id = a.work_item_id"
}

pub(crate) fn attention_one(conn: &rusqlite::Connection, id: &str) -> Result<AttentionView> {
    conn.query_row(
        &format!("{} WHERE a.id = ?1", attention_projection()),
        params![id],
        attention_from_row,
    )
    .optional()?
    .ok_or_else(|| Error::NotFound {
        entity: "attention request",
        id: id.to_string(),
    })
}

pub(crate) fn attention_of(
    conn: &rusqlite::Connection,
    item_id: Option<&str>,
) -> Result<Vec<AttentionView>> {
    let Some(item_id) = item_id else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(&format!(
        "{} WHERE a.work_item_id = ?1 ORDER BY a.blocking DESC, a.created_at DESC",
        attention_projection()
    ))?;
    let rows = stmt
        .query_map(params![item_id], attention_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn current_project(conn: &rusqlite::Connection, attention: &AttentionView) -> Result<String> {
    let id = conn.query_row(
        "SELECT project_id FROM attention_requests WHERE id = ?1",
        params![attention.id],
        |r| r.get::<_, String>(0),
    )?;
    Ok(id)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn attach_label(
    tx: &rusqlite::Transaction<'_>,
    project_id: &str,
    item_id: &str,
    label: &str,
    actor: &Actor,
    at: i64,
) -> Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO labels (project_id, name) VALUES (?1, ?2)",
        params![project_id, label],
    )?;
    let label_id: i64 = tx.query_row(
        "SELECT id FROM labels WHERE project_id = ?1 AND name = ?2",
        params![project_id, label],
        |r| r.get(0),
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO item_labels (work_item_id, label_id) VALUES (?1, ?2)",
        params![item_id, label_id],
    )?;
    let _ = (actor, at);
    Ok(())
}

pub(crate) fn bump_version(tx: &rusqlite::Transaction<'_>, item_id: &str, at: i64) -> Result<()> {
    tx.execute(
        "UPDATE work_items SET version = version + 1, updated_at = ?1 WHERE id = ?2",
        params![at, item_id],
    )?;
    Ok(())
}

fn would_cycle(tx: &rusqlite::Transaction<'_>, item_id: &str, depends_on_id: &str) -> Result<bool> {
    // Adding `item -> depends_on` creates a cycle iff `item` is already
    // reachable from `depends_on` following dependency edges.
    let mut stmt = tx.prepare(
        "WITH RECURSIVE reach(id) AS (
             SELECT depends_on_id FROM dependencies WHERE work_item_id = ?1
             UNION
             SELECT d.depends_on_id FROM dependencies d JOIN reach r ON d.work_item_id = r.id
         )
         SELECT COUNT(*) FROM reach WHERE id = ?2",
    )?;
    let count: i64 = stmt.query_row(params![depends_on_id, item_id], |r| r.get(0))?;
    Ok(count > 0)
}
