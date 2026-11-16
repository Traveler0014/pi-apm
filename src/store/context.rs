//! Context revisions: the handover document, versioned and optimistic-locked.
//!
//! Every revision stores a full snapshot (goal, current state, constraints,
//! references, notes) plus author, session, run, reason, timestamp and a content
//! hash. "Before" is simply the previous snapshot row, so history is a
//! first-class trail rather than an afterthought.

use rusqlite::params;
use serde_json::json;

use super::items::context_version;
use super::*;
use crate::store::claims::require_write_claim;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRevision {
    pub item: WorkItemView,
    pub context: ContextVersionView,
    pub previous_version: Option<i64>,
    pub changed_fields: Vec<String>,
}

impl Store {
    /// Current context version of an item.
    pub fn current_context(&self, selector: &str) -> Result<ContextVersionView> {
        let raw = require_raw_item(&self.conn, selector)?;
        Ok(context_version(&self.conn, &raw.id, raw.context_version)?.view())
    }

    /// One specific context version (or the current one when `version` is None).
    pub fn context_at(&self, selector: &str, version: Option<i64>) -> Result<ContextVersionView> {
        let raw = require_raw_item(&self.conn, selector)?;
        let version = version.unwrap_or(raw.context_version);
        Ok(context_version(&self.conn, &raw.id, version)?.view())
    }

    /// Full context history, newest first.
    pub fn context_history(&self, selector: &str, limit: i64) -> Result<Vec<ContextVersionView>> {
        let raw = require_raw_item(&self.conn, selector)?;
        let sql =
            format!("{CONTEXT_SELECT} WHERE work_item_id = ?1 ORDER BY version DESC LIMIT ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![raw.id, if limit <= 0 { 100 } else { limit }],
                RawContext::from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.iter().map(|r| r.view()).collect())
    }

    /// Create a new context version. Requires a valid claim and the expected
    /// item version, so two agents cannot silently overwrite each other.
    #[allow(clippy::too_many_arguments)]
    pub fn revise_context(
        &mut self,
        selector: &str,
        token: Option<&str>,
        expect_version: Option<i64>,
        expect_context_version: Option<i64>,
        doc: ContextDoc,
        reason: &str,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<ContextRevision> {
        doc.validate()?;
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(Error::invalid(
                "context revision requires a non-empty reason (why did the handover change?)",
            ));
        }
        let (project_row, item_id) = {
            let raw = require_raw_item(&self.conn, selector)?;
            (self.require_project(Some(&raw.project_id))?, raw.id.clone())
        };
        let idem = Idem::new("item.context", idempotency_key)
            .item(Some(&item_id))
            .request(json!({
                "doc": doc,
                "reason": reason,
                "expect_version": expect_version,
                "expect_context_version": expect_context_version,
                "actor": actor.name,
                "session": actor.session_id,
            }));
        self.idempotent(idem, |tx| {
            let raw = require_raw_item(tx, selector)?;
            require_write_claim(tx, &raw, token, actor, at)?;
            super::items::check_version(&raw, expect_version)?;
            if let Some(expected) = expect_context_version
                && expected != raw.context_version
            {
                return Err(Error::VersionConflict {
                    expected,
                    actual: raw.context_version,
                });
            }
            let previous = context_version(tx, &raw.id, raw.context_version)?;
            let new_hash = doc.hash();
            if new_hash == previous.doc_hash {
                return Err(Error::invalid(
                    "context unchanged; refusing to create an empty revision",
                ));
            }
            let changed = diff_fields(&previous.doc, &doc);
            let next_version = raw.context_version + 1;
            let before_version = raw.version;
            super::items::insert_context_version(
                tx,
                &raw.id,
                next_version,
                &doc,
                &new_hash,
                Some(previous.version),
                reason,
                actor,
                at,
            )?;
            tx.execute(
                "UPDATE work_items SET context_version = ?1, version = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![next_version, before_version + 1, at, raw.id],
            )?;

            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: Some(&raw.id),
                    kind: "context_revised",
                    actor,
                    prev_version: Some(before_version),
                    new_version: Some(before_version + 1),
                    idempotency_key,
                    payload: json!({
                        "context_version": next_version,
                        "previous_context_version": previous.version,
                        "reason": reason,
                        "previous_hash": previous.doc_hash,
                        "new_hash": new_hash,
                        "changed_fields": changed,
                    }),
                },
            )?;

            let raw = require_raw_item(tx, &raw.id)?;
            let item = hydrate_item(tx, &raw, at, &project_row.settings)?;
            let context = context_version(tx, &raw.id, next_version)?.view();
            Ok(ContextRevision {
                item,
                context,
                previous_version: Some(previous.version),
                changed_fields: changed,
            })
        })
    }
}

fn diff_fields(previous: &ContextDoc, next: &ContextDoc) -> Vec<String> {
    let mut out = Vec::new();
    if previous.goal != next.goal {
        out.push("goal".into());
    }
    if previous.current_state != next.current_state {
        out.push("current_state".into());
    }
    if previous.constraints != next.constraints {
        out.push("constraints".into());
    }
    if previous.references != next.references {
        out.push("references".into());
    }
    if previous.notes != next.notes {
        out.push("notes".into());
    }
    out
}
