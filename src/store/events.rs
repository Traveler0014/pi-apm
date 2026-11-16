//! Append-only audit trail.
//!
//! Every state change writes one event with a UTC timestamp, the acting agent
//! (plus session/run), the previous and new item version, and an optional
//! idempotency key. Events are the recovery path when two agents disagree about
//! what happened, and they are what `apm doctor` and the dashboard use to
//! distinguish "alive but idle" from "aborted mid-run".

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};

use super::*;

pub struct EventInput<'a> {
    pub at: i64,
    pub project_id: &'a str,
    pub work_item_id: Option<&'a str>,
    pub kind: &'a str,
    pub actor: &'a Actor,
    pub prev_version: Option<i64>,
    pub new_version: Option<i64>,
    pub idempotency_key: Option<&'a str>,
    pub payload: Value,
}

pub(crate) fn append_event(tx: &Transaction<'_>, ev: EventInput<'_>) -> Result<i64> {
    // Scope the idempotency key per event kind so a single logical operation
    // can emit several distinct events without colliding on the unique index.
    let event_key = ev.idempotency_key.map(|k| format!("{k}#{}", ev.kind));
    tx.execute(
        "INSERT INTO events
         (at, project_id, work_item_id, kind, actor, actor_kind, session_id, run_id,
          prev_version, new_version, idempotency_key, payload_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            ev.at,
            ev.project_id,
            ev.work_item_id,
            ev.kind,
            ev.actor.name,
            ev.actor.kind.as_str(),
            ev.actor.session_id,
            ev.actor.run_id,
            ev.prev_version,
            ev.new_version,
            event_key,
            serde_json::to_string(&ev.payload)?,
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

pub(crate) const EVENT_SELECT: &str = "
SELECT e.id, e.at, e.kind, e.actor, e.actor_kind, e.session_id, e.run_id, e.work_item_id,
       i.identifier, e.prev_version, e.new_version, e.idempotency_key, e.payload_json
FROM events e
LEFT JOIN work_items i ON i.id = e.work_item_id
";

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventView> {
    let at: i64 = row.get(1)?;
    let kind: String = row.get(4)?;
    let payload: String = row.get(12)?;
    Ok(EventView {
        id: row.get(0)?,
        at: ts(at),
        kind: row.get(2)?,
        actor: row.get(3)?,
        actor_kind: if kind == "human" {
            ActorKind::Human
        } else {
            ActorKind::Agent
        },
        session_id: row.get(5)?,
        run_id: row.get(6)?,
        item_id: row.get(7)?,
        identifier: row.get(8)?,
        prev_version: row.get(9)?,
        new_version: row.get(10)?,
        idempotency_key: row.get(11)?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
    })
}

impl Store {
    /// History for one item, oldest first.
    pub fn item_events(&self, selector: &str, limit: i64) -> Result<Vec<EventView>> {
        let item = require_raw_item(&self.conn, selector)?;
        let sql = format!("{EVENT_SELECT} WHERE e.work_item_id = ?1 ORDER BY e.id DESC LIMIT ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(params![item.id, limit], event_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    pub fn project_events(&self, project_id: &str, limit: i64) -> Result<Vec<EventView>> {
        let sql = format!("{EVENT_SELECT} WHERE e.project_id = ?1 ORDER BY e.id DESC LIMIT ?2");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(params![project_id, limit], event_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// One event by id (used by doctor/diagnostics rendering).
    pub fn event_by_id(&self, id: i64) -> Result<Option<EventView>> {
        let sql = format!("{EVENT_SELECT} WHERE e.id = ?1");
        Ok(self
            .conn
            .query_row(&sql, params![id], event_from_row)
            .optional()?)
    }

    /// Append-only diagnostic note. Deliberately does not require a claim and
    /// does not touch item state or `version`: it is evidence for humans, not a
    /// work-item mutation. Used by the side-run when it cannot recover a lease.
    pub fn diagnose(
        &mut self,
        project: Option<&str>,
        item: Option<&str>,
        actor: &Actor,
        code: &str,
        message: &str,
        idempotency_key: Option<&str>,
    ) -> Result<EventView> {
        let now = now_ms();
        let project_row = self.require_project(project)?;
        let item_id = match item {
            Some(sel) => Some(require_raw_item(&self.conn, sel)?.id),
            None => None,
        };
        let idem = Idem::new("diag", idempotency_key)
            .item(item_id.as_deref())
            .request(json!({
                "code": code,
                "message": message,
                "actor": actor.name,
                "session": actor.session_id,
                "run": actor.run_id,
            }));
        self.idempotent(idem, |tx| {
            let id = append_event(
                tx,
                EventInput {
                    at: now,
                    project_id: &project_row.id,
                    work_item_id: item_id.as_deref(),
                    kind: "diagnostic",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key,
                    payload: json!({ "code": code, "message": message }),
                },
            )?;
            let ev = tx.query_row(
                &format!("{EVENT_SELECT} WHERE e.id = ?1"),
                params![id],
                event_from_row,
            )?;
            Ok(ev)
        })
    }

    /// Items that currently carry an unresolved side-run recovery diagnostic:
    /// a `diagnostic` event with a `claim_*` code after the last claim event.
    pub(crate) fn items_with_pending_diagnostic(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.work_item_id FROM events e
             WHERE e.kind = 'diagnostic' AND e.work_item_id IS NOT NULL
               AND json_extract(e.payload_json, '$.code') LIKE 'claim%'
               AND e.id > COALESCE((
                    SELECT MAX(c.id) FROM events c
                    WHERE c.work_item_id = e.work_item_id
                      AND c.kind IN ('claimed', 'claim_renewed', 'claim_taken_over', 'claim_released')
               ), 0)
             GROUP BY e.work_item_id",
        )?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Latest run-end event for an item, shaped for the dashboard.
pub(crate) fn last_run(conn: &rusqlite::Connection, item_id: &str) -> Result<Option<RunSyncView>> {
    let row = conn
        .query_row(
            "SELECT at, run_id, kind, payload_json FROM events
             WHERE work_item_id = ?1 AND kind IN ('run_synced', 'run_aborted')
             ORDER BY id DESC LIMIT 1",
            params![item_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|(at, run_id, kind, payload)| {
        let payload: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
        RunSyncView {
            run_id,
            outcome: if kind == "run_aborted" {
                "aborted".to_string()
            } else {
                payload
                    .get("outcome")
                    .and_then(|v| v.as_str())
                    .unwrap_or("completed")
                    .to_string()
            },
            at: ts(at),
            note: payload
                .get("note")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }
    }))
}
