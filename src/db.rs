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

use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 2;

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

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;

    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > 0 && version < 2 {
        // v2 bound idempotency records to (operation, item, request fingerprint)
        // and removed the plaintext-token leak. The table is a short-lived replay
        // cache, so dropping it on upgrade is safe; the events stay untouched.
        conn.execute_batch("DROP TABLE IF EXISTS idempotency;")?;
    }

    conn.execute_batch(SCHEMA)?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }
}
