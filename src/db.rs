//! SQLite schema and migration.
//!
//! Design notes that matter for correctness:
//!
//! * `PRAGMA journal_mode = WAL` plus `BEGIN IMMEDIATE` for every mutating
//!   operation makes claim acquisition and version-checked updates atomic even
//!   with several agent processes hitting the same database file.
//! * The database is the single source of truth (local-first). There is no
//!   server, no background daemon, and no cache to invalidate.
//! * `events` is append-only and carries the audit trail. The unique index on
//!   `idempotency_key` is the second line of defence against duplicated
//!   side-run writes (the `idempotency` table is the first).

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use crate::model::{Category, StateKey};
use crate::workflow::Workflow;

pub const SCHEMA_VERSION: i64 = 3;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL UNIQUE,
    prefix          TEXT NOT NULL,
    root_path       TEXT,
    created_at      INTEGER NOT NULL,
    settings_json   TEXT NOT NULL,
    workflow_json   TEXT
);

CREATE TABLE IF NOT EXISTS work_items (
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number            INTEGER NOT NULL,
    identifier        TEXT NOT NULL,
    title             TEXT NOT NULL CHECK (length(trim(title)) > 0),
    item_type         TEXT NOT NULL DEFAULT 'task',
    status            TEXT NOT NULL,
    priority          TEXT NOT NULL DEFAULT 'normal',
    parent_id         TEXT REFERENCES work_items(id),
    version           INTEGER NOT NULL DEFAULT 1,
    context_version   INTEGER NOT NULL DEFAULT 1,
    progress_summary  TEXT,
    next_action       TEXT,
    blocker           TEXT,
    waiting_on_human  INTEGER NOT NULL DEFAULT 0,
    waiting_reason    TEXT,
    completion_note   TEXT,
    created_by        TEXT NOT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    last_heartbeat_at INTEGER,
    last_progress_at  INTEGER,
    last_run_sync_at  INTEGER,
    done_at           INTEGER,
    cancelled_at      INTEGER,
    UNIQUE (project_id, number),
    UNIQUE (project_id, identifier)
);
CREATE INDEX IF NOT EXISTS work_items_status ON work_items(project_id, status);
CREATE INDEX IF NOT EXISTS work_items_parent ON work_items(parent_id);

CREATE TABLE IF NOT EXISTS context_versions (
    work_item_id      TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    version           INTEGER NOT NULL,
    goal              TEXT NOT NULL,
    current_state     TEXT NOT NULL,
    constraints_json  TEXT NOT NULL,
    references_json   TEXT NOT NULL,
    notes             TEXT,
    doc_hash          TEXT NOT NULL,
    revises_version   INTEGER,
    reason            TEXT NOT NULL,
    author            TEXT NOT NULL,
    actor_kind        TEXT NOT NULL,
    session_id        TEXT,
    run_id            TEXT,
    created_at        INTEGER NOT NULL,
    PRIMARY KEY (work_item_id, version)
);

CREATE TABLE IF NOT EXISTS claims (
    id                TEXT PRIMARY KEY,
    work_item_id      TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    token_hash        TEXT NOT NULL,
    token_prefix      TEXT NOT NULL,
    holder            TEXT NOT NULL,
    holder_kind       TEXT NOT NULL,
    session_id        TEXT,
    run_id            TEXT,
    status            TEXT NOT NULL,
    lease_seconds     INTEGER NOT NULL,
    leased_at         INTEGER NOT NULL,
    heartbeat_at      INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL,
    released_at       INTEGER,
    release_reason    TEXT
);
CREATE INDEX IF NOT EXISTS claims_item ON claims(work_item_id, status);
CREATE INDEX IF NOT EXISTS claims_session ON claims(session_id);
CREATE UNIQUE INDEX IF NOT EXISTS claims_one_active
    ON claims(work_item_id) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS events (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    at                INTEGER NOT NULL,
    project_id        TEXT NOT NULL,
    work_item_id      TEXT,
    kind              TEXT NOT NULL,
    actor             TEXT NOT NULL,
    actor_kind        TEXT NOT NULL,
    session_id        TEXT,
    run_id            TEXT,
    prev_version      INTEGER,
    new_version       INTEGER,
    idempotency_key   TEXT,
    payload_json      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_item ON events(work_item_id, id);
CREATE INDEX IF NOT EXISTS events_project ON events(project_id, id);
CREATE UNIQUE INDEX IF NOT EXISTS events_idempotency
    ON events(idempotency_key) WHERE idempotency_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS idempotency (
    key             TEXT PRIMARY KEY,
    work_item_id    TEXT,
    op              TEXT NOT NULL,
    fingerprint     TEXT NOT NULL,
    response_json   TEXT NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS labels (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id  TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    UNIQUE (project_id, name)
);

CREATE TABLE IF NOT EXISTS item_labels (
    work_item_id  TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    label_id      INTEGER NOT NULL REFERENCES labels(id) ON DELETE CASCADE,
    PRIMARY KEY (work_item_id, label_id)
);

CREATE TABLE IF NOT EXISTS dependencies (
    work_item_id   TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    depends_on_id  TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    created_at     INTEGER NOT NULL,
    created_by     TEXT NOT NULL,
    PRIMARY KEY (work_item_id, depends_on_id)
);
CREATE INDEX IF NOT EXISTS dependencies_upstream ON dependencies(depends_on_id);

CREATE TABLE IF NOT EXISTS acceptance_criteria (
    work_item_id  TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,
    text          TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'open',
    note          TEXT,
    updated_by    TEXT,
    updated_at    INTEGER,
    PRIMARY KEY (work_item_id, seq)
);

CREATE TABLE IF NOT EXISTS evidence (
    id            TEXT PRIMARY KEY,
    work_item_id  TEXT NOT NULL REFERENCES work_items(id) ON DELETE CASCADE,
    kind          TEXT NOT NULL,
    value         TEXT NOT NULL,
    note          TEXT,
    created_by    TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS evidence_item ON evidence(work_item_id);

CREATE TABLE IF NOT EXISTS attention_requests (
    id                TEXT PRIMARY KEY,
    project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    work_item_id      TEXT REFERENCES work_items(id) ON DELETE CASCADE,
    question          TEXT NOT NULL,
    context           TEXT NOT NULL,
    options_json      TEXT NOT NULL DEFAULT '[]',
    recommendation    TEXT,
    blocking          INTEGER NOT NULL DEFAULT 1,
    status            TEXT NOT NULL,
    response          TEXT,
    requested_by      TEXT NOT NULL,
    requested_kind    TEXT NOT NULL,
    session_id        TEXT,
    run_id            TEXT,
    created_at        INTEGER NOT NULL,
    resolved_at       INTEGER,
    resolved_by       TEXT
);
CREATE INDEX IF NOT EXISTS attention_item ON attention_requests(work_item_id, status);
CREATE INDEX IF NOT EXISTS attention_project ON attention_requests(project_id, status);
"#;

pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;

    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    // Opening a database at the current schema is the common path (every CLI
    // call); running the whole `SCHEMA` batch again just to reach the same
    // `user_version` is pure overhead. A newer database is left untouched
    // rather than rewritten backwards by an older binary.
    if version >= SCHEMA_VERSION {
        return Ok(());
    }

    // The DDL, the data migration and the version stamp are one unit: a crash
    // must not leave a database whose tables are v3 but whose `user_version`
    // still says v1 (or the other way round).
    //
    // `IMMEDIATE` takes the write lock up front, which is what makes the second
    // `user_version` read meaningful: two processes opening a v1 database
    // serialise here, and the loser re-reads the version under the lock instead
    // of replaying the migration against an already-upgraded schema (the old
    // `DEFERRED` path could re-run `DROP TABLE idempotency` from a stale read).
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version >= SCHEMA_VERSION {
        // Another process migrated while we waited for the lock; drop the empty
        // transaction and leave its work alone.
        return Ok(());
    }
    if version > 0 && version < 2 {
        // v2 bound idempotency records to (operation, item, request fingerprint)
        // and removed the plaintext-token leak. The table is a short-lived replay
        // cache, so dropping it on upgrade is safe; the events stay untouched.
        tx.execute_batch("DROP TABLE IF EXISTS idempotency;")?;
    }

    tx.execute_batch(SCHEMA)?;
    if version < 3 {
        migrate_v3(&tx)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()
}

/// Schema v3: versioned workflows, per-type bindings and per-item snapshots.
///
/// The migration is written to be re-runnable (every step is `IF NOT EXISTS`,
/// `INSERT OR IGNORE`, or an idempotent backfill) so a crash mid-way does not
/// leave a half-upgraded database behind. Every item ends up bound to a workflow
/// that actually declares its state; anything else is repaired to the workflow's
/// initial state and recorded as a `workflow_migrated` event.
fn migrate_v3(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    add_column_if_missing(
        tx,
        "work_items",
        "workflow_id",
        "TEXT NOT NULL DEFAULT 'apm-default'",
    )?;
    add_column_if_missing(
        tx,
        "work_items",
        "workflow_version",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(
        tx,
        "work_items",
        "state_category",
        "TEXT NOT NULL DEFAULT 'open'",
    )?;
    add_column_if_missing(tx, "projects", "default_workflow_id", "TEXT")?;
    add_column_if_missing(tx, "projects", "default_workflow_version", "INTEGER")?;

    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS workflows (
            project_id       TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            id               TEXT NOT NULL,
            version          INTEGER NOT NULL,
            name             TEXT NOT NULL,
            definition_json  TEXT NOT NULL,
            created_at       INTEGER NOT NULL,
            created_by       TEXT NOT NULL,
            PRIMARY KEY (project_id, id, version)
        );
        CREATE TABLE IF NOT EXISTS type_workflows (
            project_id        TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            item_type         TEXT NOT NULL,
            workflow_id       TEXT NOT NULL,
            workflow_version  INTEGER NOT NULL,
            updated_at        INTEGER NOT NULL,
            updated_by        TEXT NOT NULL,
            PRIMARY KEY (project_id, item_type),
            FOREIGN KEY (project_id, workflow_id, workflow_version)
                REFERENCES workflows(project_id, id, version)
        );",
    )?;

    let at = chrono::Utc::now().timestamp_millis();
    let projects: Vec<(String, Option<String>)> = {
        let mut stmt = tx.prepare("SELECT id, workflow_json FROM projects ORDER BY created_at")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    // Cache of parsed workflow documents keyed by (id, version) so a project with
    // many items does not re-parse the same JSON per row.
    let mut cache: HashMap<(String, i64), Workflow> = HashMap::new();

    for (project_id, legacy_json) in projects {
        let workflow = legacy_json
            .as_deref()
            .and_then(|raw| Workflow::from_json(raw).ok())
            .unwrap_or_else(Workflow::default_v1);
        let workflow_json = workflow.to_json().unwrap_or_else(|_| "{}".to_string());
        cache.insert((workflow.id.clone(), workflow.version), workflow.clone());

        tx.execute(
            "INSERT OR IGNORE INTO workflows
             (project_id, id, version, name, definition_json, created_at, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'migration')",
            params![
                project_id,
                workflow.id,
                workflow.version,
                workflow.name,
                workflow_json,
                at
            ],
        )?;
        tx.execute(
            "UPDATE projects SET default_workflow_id = ?1, default_workflow_version = ?2
             WHERE id = ?3",
            params![workflow.id, workflow.version, project_id],
        )?;

        // Bind every item type that already exists in the project.
        let item_types: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT item_type FROM work_items WHERE project_id = ?1 ORDER BY item_type",
            )?;
            stmt.query_map(params![project_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for item_type in &item_types {
            tx.execute(
                "INSERT OR IGNORE INTO type_workflows
                 (project_id, item_type, workflow_id, workflow_version, updated_at, updated_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'migration')",
                params![project_id, item_type, workflow.id, workflow.version, at],
            )?;
        }

        // Backfill items with their snapshot and category, repairing any state
        // the workflow does not declare.
        let items: Vec<(String, String, String)> = {
            let mut stmt =
                tx.prepare("SELECT id, item_type, status FROM work_items WHERE project_id = ?1")?;
            stmt.query_map(params![project_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (item_id, item_type, current_state) in items {
            let binding: (String, i64) = tx
                .query_row(
                    "SELECT workflow_id, workflow_version FROM type_workflows
                     WHERE project_id = ?1 AND item_type = ?2",
                    params![project_id, item_type],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .unwrap_or((workflow.id.clone(), workflow.version));
            let bound = match cache.get(&binding) {
                Some(wf) => wf.clone(),
                None => {
                    let json: String = tx.query_row(
                        "SELECT definition_json FROM workflows
                         WHERE project_id = ?1 AND id = ?2 AND version = ?3",
                        params![project_id, binding.0, binding.1],
                        |r| r.get(0),
                    )?;
                    let wf = Workflow::from_json(&json).unwrap_or_else(|_| Workflow::default_v1());
                    cache.insert(binding.clone(), wf.clone());
                    wf
                }
            };
            let (state, repaired) = match StateKey::parse(&current_state) {
                Ok(key) if bound.state(key.as_str()).is_some() => (key, false),
                _ => (
                    StateKey::parse(&bound.initial).unwrap_or_else(|_| StateKey::ready()),
                    true,
                ),
            };
            let category = bound
                .category_of(state.as_str())
                .unwrap_or_else(|| Category::legacy_for_state(state.as_str()));
            tx.execute(
                "UPDATE work_items SET workflow_id = ?1, workflow_version = ?2,
                    state_category = ?3, status = ?4
                 WHERE id = ?5",
                params![
                    bound.id,
                    bound.version,
                    category.as_str(),
                    state.as_str(),
                    item_id
                ],
            )?;
            if repaired {
                tx.execute(
                    "INSERT INTO events
                     (at, project_id, work_item_id, kind, actor, actor_kind, session_id, run_id,
                      prev_version, new_version, idempotency_key, payload_json)
                     VALUES (?1, ?2, ?3, 'workflow_migrated', 'apm-migration', 'agent', NULL, NULL,
                             NULL, NULL, NULL, ?4)",
                    params![
                        at,
                        project_id,
                        item_id,
                        serde_json::json!({
                            "from": current_state,
                            "to": state.as_str(),
                            "workflow_id": bound.id,
                            "workflow_version": bound.version,
                            "reason": "state not declared by the bound workflow",
                        })
                        .to_string()
                    ],
                )?;
            }
        }
    }

    Ok(())
}

/// SQLite has no `ADD COLUMN IF NOT EXISTS`; check `PRAGMA table_info` first so
/// a re-run after a partial migration is harmless.
fn add_column_if_missing(
    tx: &Transaction<'_>,
    table: &str,
    column: &str,
    ddl: &str,
) -> rusqlite::Result<()> {
    let exists: bool = {
        let mut stmt = tx.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == column)
    };
    if !exists {
        tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {ddl};"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);

        // The evidence index is declared exactly once (it used to be listed
        // twice in `SCHEMA`, which made the migration look like it was adding
        // two indexes that SQLite silently collapsed into one).
        let indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'evidence_item'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 1);
    }

    #[test]
    fn a_newer_schema_is_left_alone() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        migrate(&mut conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION + 1, "never downgrade user_version");
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'work_items'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0, "an older binary must not rewrite a newer schema");
    }
}
