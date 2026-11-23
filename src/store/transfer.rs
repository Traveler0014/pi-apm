//! Safe transfer of a ledger: consistent backups and redacted NDJSON export.
//!
//! `backup_to` uses SQLite's `VACUUM INTO`, which reads through the WAL and
//! writes a self-contained database without needing a second connection or a
//! heavy dependency. The destination is never overwritten.
//!
//! `export_ndjson` is an audit/archival view, not a sync mechanism. It walks the
//! business tables only: claims, sessions, idempotency keys and every token live
//! outside the export by construction, and event payloads pass through a
//! defensive redaction pass. There is deliberately no importer.

use std::io::Write;
use std::path::Path;

use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::model::*;

use super::Store;

/// NDJSON format version. Bump it only for breaking record/field changes.
pub const EXPORT_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct BackupReport {
    pub path: String,
    pub bytes: u64,
    pub items: i64,
    pub events: i64,
    pub integrity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub schema_version: i64,
    pub records: usize,
    pub bytes: u64,
}

impl Store {
    /// Create a WAL-consistent snapshot at `dest`. Refuses to overwrite.
    pub fn backup_to(&self, dest: &Path) -> Result<BackupReport> {
        if dest.exists() {
            return Err(Error::invalid(format!(
                "backup destination already exists: {} (refusing to overwrite)",
                dest.display()
            )));
        }
        if let Some(parent) = dest.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Internal(format!("cannot create {}: {e}", parent.display())))?;
        }
        let target = dest.to_string_lossy().to_string();
        // `VACUUM INTO` reads the current database *including committed WAL
        // frames* and writes a defragmented, self-contained copy.
        if let Err(err) = self.conn.execute("VACUUM INTO ?1", params![target.clone()]) {
            let _ = std::fs::remove_file(dest);
            return Err(Error::Internal(format!(
                "cannot create backup {}: {err}",
                dest.display()
            )));
        }

        let verified = verify_backup(dest);
        match verified {
            Ok((items, events)) => {
                if let Err(err) = restrict_backup_mode(dest) {
                    let _ = std::fs::remove_file(dest);
                    return Err(err);
                }
                let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
                Ok(BackupReport {
                    path: dest.display().to_string(),
                    bytes,
                    items,
                    events,
                    integrity: "ok".to_string(),
                })
            }
            Err(err) => {
                let _ = std::fs::remove_file(dest);
                Err(err)
            }
        }
    }

    /// Write the versioned, redacted NDJSON export of one project to `out`.
    ///
    /// The project selector follows the CLI (`--project`); with a single-project
    /// database `None` is enough.
    pub fn export_ndjson(
        &self,
        project: Option<&str>,
        mut out: impl Write,
    ) -> Result<ExportReport> {
        // One read snapshot for the whole walk. Without it each table read would
        // take its own SQLite snapshot, so a concurrent writer could make an
        // item and its events/acceptance disagree. `unchecked_transaction` is
        // the `&Connection` variant (we only read; the name is rusqlite's).
        let tx = self.conn.unchecked_transaction()?;
        let project = self.require_project(project)?;
        let mut records = Vec::new();
        let push = |value: Value, records: &mut Vec<Value>| {
            records.push(sanitize_value(value));
        };

        let project_row = self.project_export(&project)?;
        push(
            json!({
                "type": "meta",
                "schema_version": EXPORT_SCHEMA_VERSION,
                "exported_at": now_ms(),
                "project": project_row,
            }),
            &mut records,
        );

        for item in self.item_export(&project.id)? {
            push(item, &mut records);
        }
        for context in self.context_export(&project.id)? {
            push(context, &mut records);
        }
        for acceptance in self.acceptance_export(&project.id)? {
            push(acceptance, &mut records);
        }
        for evidence in self.evidence_export(&project.id)? {
            push(evidence, &mut records);
        }
        for dependency in self.dependency_export(&project.id)? {
            push(dependency, &mut records);
        }
        for event in self.event_export(&project.id)? {
            push(event, &mut records);
        }

        // Release the read snapshot before touching the output: writing to a
        // slow pipe/stdout must not keep the WAL read transaction open (and
        // hold back checkpoints) for as long as the consumer is slow.
        tx.commit()?;

        let mut bytes = 0u64;
        for record in &records {
            let line = serde_json::to_string(record)?;
            bytes += line.len() as u64 + 1;
            out.write_all(line.as_bytes())
                .map_err(|e| Error::Internal(format!("cannot write export: {e}")))?;
            out.write_all(b"\n")
                .map_err(|e| Error::Internal(format!("cannot write export: {e}")))?;
        }
        out.flush()
            .map_err(|e| Error::Internal(format!("cannot flush export: {e}")))?;
        Ok(ExportReport {
            schema_version: EXPORT_SCHEMA_VERSION,
            records: records.len(),
            bytes,
        })
    }

    fn project_export(&self, project: &super::ProjectRow) -> Result<Value> {
        let row = self.conn.query_row(
            "SELECT id, name, prefix, created_at, settings_json,
                    default_workflow_id, default_workflow_version
             FROM projects WHERE id = ?1",
            params![project.id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "prefix": row.get::<_, String>(2)?,
                    "created_at": row.get::<_, i64>(3)?,
                    "settings": parse_json(row.get::<_, String>(4)?),
                    "default_workflow_id": row.get::<_, Option<String>>(5)?,
                    "default_workflow_version": row.get::<_, Option<i64>>(6)?,
                }))
            },
        )?;
        Ok(row)
    }

    fn item_export(&self, project_id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, number, identifier, title, item_type, status, priority,
                    parent_id, version, context_version, progress_summary, next_action,
                    blocker, waiting_on_human, waiting_reason, completion_note,
                    created_by, created_at, updated_at, done_at, cancelled_at,
                    workflow_id, workflow_version, state_category
             FROM work_items WHERE project_id = ?1 ORDER BY number",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            let id: String = row.get(0)?;
            let labels = self
                .conn
                .prepare(
                    "SELECT l.name FROM item_labels il
                     JOIN labels l ON l.id = il.label_id
                     WHERE il.work_item_id = ?1 ORDER BY l.name",
                )?
                .query_map(params![id], |label| label.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(json!({
                "type": "item",
                "schema_version": EXPORT_SCHEMA_VERSION,
                "id": id,
                "number": row.get::<_, i64>(1)?,
                "identifier": row.get::<_, String>(2)?,
                "title": row.get::<_, String>(3)?,
                "item_type": row.get::<_, String>(4)?,
                "status": row.get::<_, String>(5)?,
                "priority": row.get::<_, String>(6)?,
                "parent_id": row.get::<_, Option<String>>(7)?,
                "version": row.get::<_, i64>(8)?,
                "context_version": row.get::<_, i64>(9)?,
                "progress_summary": row.get::<_, Option<String>>(10)?,
                "next_action": row.get::<_, Option<String>>(11)?,
                "blocker": row.get::<_, Option<String>>(12)?,
                "waiting_on_human": row.get::<_, bool>(13)?,
                "waiting_reason": row.get::<_, Option<String>>(14)?,
                "completion_note": row.get::<_, Option<String>>(15)?,
                "created_by": row.get::<_, String>(16)?,
                "created_at": row.get::<_, i64>(17)?,
                "updated_at": row.get::<_, i64>(18)?,
                "done_at": row.get::<_, Option<i64>>(19)?,
                "cancelled_at": row.get::<_, Option<i64>>(20)?,
                "workflow_id": row.get::<_, Option<String>>(21)?,
                "workflow_version": row.get::<_, Option<i64>>(22)?,
                "state_category": row.get::<_, Option<String>>(23)?,
                "labels": labels,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    fn context_export(&self, project_id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.work_item_id, c.version, c.goal, c.current_state,
                    c.constraints_json, c.references_json, c.notes, c.doc_hash,
                    c.revises_version, c.reason, c.author, c.actor_kind, c.created_at
             FROM context_versions c
             JOIN work_items w ON w.id = c.work_item_id
             WHERE w.project_id = ?1
               AND c.version = (SELECT MAX(x.version) FROM context_versions x
                                WHERE x.work_item_id = c.work_item_id)
             ORDER BY w.number",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(json!({
                "type": "context",
                "schema_version": EXPORT_SCHEMA_VERSION,
                "item_id": row.get::<_, String>(0)?,
                "version": row.get::<_, i64>(1)?,
                "goal": row.get::<_, String>(2)?,
                "current_state": row.get::<_, String>(3)?,
                "constraints": parse_json(row.get::<_, String>(4)?),
                "references": parse_json(row.get::<_, String>(5)?),
                "notes": row.get::<_, Option<String>>(6)?,
                "doc_hash": row.get::<_, String>(7)?,
                "revises_version": row.get::<_, Option<i64>>(8)?,
                "reason": row.get::<_, String>(9)?,
                "author": row.get::<_, String>(10)?,
                "actor_kind": row.get::<_, String>(11)?,
                "created_at": row.get::<_, i64>(12)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    fn acceptance_export(&self, project_id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.work_item_id, a.seq, a.text, a.status, a.note, a.updated_by, a.updated_at
             FROM acceptance_criteria a
             JOIN work_items w ON w.id = a.work_item_id
             WHERE w.project_id = ?1 ORDER BY w.number, a.seq",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(json!({
                "type": "acceptance",
                "schema_version": EXPORT_SCHEMA_VERSION,
                "item_id": row.get::<_, String>(0)?,
                "seq": row.get::<_, i64>(1)?,
                "text": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "note": row.get::<_, Option<String>>(4)?,
                "updated_by": row.get::<_, Option<String>>(5)?,
                "updated_at": row.get::<_, Option<i64>>(6)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    fn evidence_export(&self, project_id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT e.id, e.work_item_id, e.kind, e.value, e.note, e.created_by, e.created_at
             FROM evidence e
             JOIN work_items w ON w.id = e.work_item_id
             WHERE w.project_id = ?1 ORDER BY e.created_at, e.id",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(json!({
                "type": "evidence",
                "schema_version": EXPORT_SCHEMA_VERSION,
                "id": row.get::<_, String>(0)?,
                "item_id": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?,
                "value": row.get::<_, String>(3)?,
                "note": row.get::<_, Option<String>>(4)?,
                "created_by": row.get::<_, String>(5)?,
                "created_at": row.get::<_, i64>(6)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    fn dependency_export(&self, project_id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.work_item_id, d.depends_on_id, d.created_at, d.created_by
             FROM dependencies d
             JOIN work_items w ON w.id = d.work_item_id
             WHERE w.project_id = ?1 ORDER BY d.work_item_id, d.depends_on_id",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(json!({
                "type": "dependency",
                "schema_version": EXPORT_SCHEMA_VERSION,
                "item_id": row.get::<_, String>(0)?,
                "depends_on_id": row.get::<_, String>(1)?,
                "created_at": row.get::<_, i64>(2)?,
                "created_by": row.get::<_, String>(3)?,
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }

    fn event_export(&self, project_id: &str) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, at, work_item_id, kind, actor, actor_kind,
                    prev_version, new_version, payload_json
             FROM events WHERE project_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(json!({
                "type": "event",
                "schema_version": EXPORT_SCHEMA_VERSION,
                "id": row.get::<_, i64>(0)?,
                "at": row.get::<_, i64>(1)?,
                "item_id": row.get::<_, Option<String>>(2)?,
                "kind": row.get::<_, String>(3)?,
                "actor": row.get::<_, String>(4)?,
                "actor_kind": row.get::<_, String>(5)?,
                "prev_version": row.get::<_, Option<i64>>(6)?,
                "new_version": row.get::<_, Option<i64>>(7)?,
                "payload": parse_json(row.get::<_, String>(8)?),
            }))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Error::from)
    }
}

/// A backup is the whole ledger: it must be private even when the umask is lax.
#[cfg(unix)]
fn restrict_backup_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::Internal(format!("cannot restrict backup {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn restrict_backup_mode(_path: &Path) -> Result<()> {
    Ok(())
}

fn parse_json(raw: String) -> Value {
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

/// Verify the snapshot opens, is intact, and carries the business rows.
fn verify_backup(path: &Path) -> Result<(i64, i64)> {
    let conn = Connection::open(path)
        .map_err(|e| Error::Internal(format!("cannot open backup {}: {e}", path.display())))?;
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| Error::Internal(format!("cannot verify backup: {e}")))?;
    if integrity != "ok" {
        return Err(Error::Internal(format!(
            "backup integrity check failed: {integrity}"
        )));
    }
    let items: i64 = conn.query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))?;
    let events: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    Ok((items, events))
}

/// Field names that never belong in an export, at any nesting depth. Nested
/// event payloads (claim events in particular) carry `session`/`run` fields that
/// are not visible from the column list, so sanitising the whole tree is required.
const SENSITIVE_KEYS: &[&str] = &[
    "session",
    "session_id",
    "run",
    "run_id",
    "token",
    "tokens",
    "token_hash",
    "token_prefix",
    "idempotency_key",
    "authorization",
    "cookie",
    "secret",
    "rest_token",
    "api_token",
];

fn sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEYS.contains(&lower.as_str())
}

/// Remove known sensitive keys recursively, then redact token-shaped strings
/// with the shared [`crate::redact::redact_tokens`] scrub — the same one the MCP
/// adapter uses, so claim / `apm_rest_` / `apm_ui_` / `apm_boot_` tokens cannot
/// slip out of one surface while being caught by the other.
/// Best effort only: it is not a general-purpose secret scanner (see README).
fn sanitize_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(crate::redact::redact_tokens(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, _)| !sensitive_key(key))
                .map(|(key, value)| (key, sanitize_value(value)))
                .collect(),
        ),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_token_shaped_strings_only() {
        // Shared scrub: claim tokens and the prefixed REST/UI/boot shapes.
        let token = format!("apm_{}", "a".repeat(48));
        let text = format!("wrote {token} and apm_short here");
        let redacted = sanitize_value(Value::String(text));
        assert!(!redacted.to_string().contains(&token));
        assert!(redacted.to_string().contains("apm_[redacted]"));
        assert!(redacted.to_string().contains("apm_short"));
        for prefix in ["apm_rest_", "apm_ui_", "apm_boot_"] {
            let secret = format!("{prefix}{}", "b".repeat(40));
            let scrubbed = sanitize_value(Value::String(format!("id {secret}"))).to_string();
            assert!(!scrubbed.contains(&secret), "shape {prefix}");
            assert!(scrubbed.contains("apm_[redacted]"), "shape {prefix}");
        }
    }
}
