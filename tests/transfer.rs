//! Backup and export contract (APM-50).
//!
//! The interesting parts are not the happy-path files: a backup must include
//! data that only lives in the WAL, and the export must never leak credentials
//! that sit beside the database (claim rows, session files, rest-token).

use std::path::{Path, PathBuf};
use std::process::Command;

use apm::store::claims::ClaimRequest;
use apm::*;
use serde_json::Value;

fn settings() -> Settings {
    Settings {
        lease_seconds: 900,
        stale_after_seconds: 300,
        no_progress_after_seconds: 86_400,
        done_recent_seconds: 86_400,
    }
}

fn context(goal: &str) -> ContextDoc {
    ContextDoc {
        goal: goal.to_string(),
        current_state: "fresh".to_string(),
        constraints: vec!["local only".to_string()],
        references: vec!["README.md".to_string()],
        notes: None,
    }
}

struct Ledger {
    dir: tempfile::TempDir,
    db: PathBuf,
    store: Store,
    project: ProjectView,
    claim_id: String,
}

fn ledger() -> Ledger {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join(".apm").join("apm.db");
    let mut store = Store::open(&db).unwrap();
    let (project, _) = store
        .init_project(
            "Transfer Test",
            Some("TRF"),
            Some("/tmp"),
            settings(),
            &Actor::human("setup"),
        )
        .unwrap();
    let first = store
        .create_item(
            Some(&project.id),
            NewItem {
                title: "first item".into(),
                context: context("prove transfer"),
                item_type: None,
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec!["transfer".into()],
                acceptance: vec!["backup includes WAL".into()],
                actor: Actor::agent("worker").with_session(Some("secret-session".into())),
            },
            1_000,
            None,
        )
        .unwrap()
        .item;
    let second = store
        .create_item(
            Some(&project.id),
            NewItem {
                title: "second item".into(),
                context: context("depends on the first"),
                item_type: None,
                priority: None,
                parent: None,
                depends_on: vec![first.identifier.clone()],
                labels: vec![],
                acceptance: vec![],
                actor: Actor::agent("worker").with_session(Some("secret-session".into())),
            },
            1_001,
            None,
        )
        .unwrap()
        .item;
    let _ = second;
    let issued = store
        .claim(
            &first.identifier,
            &Actor::agent("worker").with_session(Some("secret-session".into())),
            ClaimRequest::default(),
            1_002,
            None,
        )
        .unwrap();
    store
        .add_evidence(
            &first.identifier,
            "test",
            "WAL checkpoint proof",
            None,
            Some(&issued.token),
            &Actor::agent("worker"),
            1_003,
            None,
        )
        .unwrap();
    store
        .set_acceptance(
            &first.identifier,
            1,
            "met",
            Some("checked"),
            Some(&issued.token),
            &Actor::agent("worker"),
            1_004,
        )
        .unwrap();
    Ledger {
        dir,
        db,
        store,
        project,
        claim_id: issued.claim.id,
    }
}

fn wal_path(db: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", db.display()))
}

#[test]
fn backup_includes_latest_wal_state_and_refuses_overwrite() {
    let ledger = ledger();
    // The writer connection stays open, so recent commits live in the WAL.
    let wal = wal_path(&ledger.db);
    assert!(wal.exists(), "SQLite should keep a WAL beside the database");
    let wal_len = std::fs::metadata(&wal).unwrap().len();
    assert!(
        wal_len > 0,
        "test needs a non-empty WAL, got {wal_len} bytes"
    );

    let dest = ledger.dir.path().join("snapshot.db");
    let report = ledger.store.backup_to(&dest).unwrap();
    assert_eq!(report.integrity, "ok");
    assert!(
        report.items >= 2,
        "backup must contain every item: {}",
        report.items
    );
    assert!(report.bytes > 0);

    let backup = rusqlite::Connection::open(&dest).unwrap();
    let titles: Vec<String> = backup
        .prepare("SELECT title FROM work_items ORDER BY number")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(titles, vec!["first item", "second item"]);
    let evidence: i64 = backup
        .query_row("SELECT COUNT(*) FROM evidence", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        evidence, 1,
        "evidence written while the WAL was live must be in the snapshot"
    );

    let err = ledger.store.backup_to(&dest).unwrap_err();
    assert_eq!(err.code(), "invalid_input");
    assert!(err.to_string().contains("refusing to overwrite"), "{err}");
    // The refused second call must not touch the existing snapshot.
    assert!(dest.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a full ledger snapshot must be private"
        );
    }
}

#[test]
fn export_is_versioned_contains_business_data_and_no_credentials() {
    let ledger = ledger();
    let data_dir = ledger.store.data_dir();
    let sessions = data_dir.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let session_token = format!("apm_{}", "a".repeat(64));
    std::fs::write(
        sessions.join("secret-session.json"),
        format!("{{\"token\":\"{session_token}\"}}\n"),
    )
    .unwrap();
    let rest_token = format!("apm_rest_{}", "b".repeat(48));
    std::fs::write(data_dir.join("rest-token"), format!("{rest_token}\n")).unwrap();

    let mut out = Vec::new();
    let report = ledger
        .store
        .export_ndjson(Some(&ledger.project.id), &mut out)
        .unwrap();
    assert_eq!(report.schema_version, 1);
    assert_eq!(
        report.records,
        out.split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count()
    );

    let text = String::from_utf8(out.clone()).unwrap();
    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("every line is standalone JSON"))
        .collect();
    let types: Vec<&str> = records
        .iter()
        .filter_map(|record| record["type"].as_str())
        .collect();
    for expected in [
        "meta",
        "item",
        "context",
        "acceptance",
        "evidence",
        "dependency",
        "event",
    ] {
        assert!(
            types.contains(&expected),
            "missing {expected} record: {types:?}"
        );
    }
    assert!(types.iter().filter(|kind| **kind == "item").count() >= 2);
    for record in &records {
        assert_eq!(
            record["schema_version"], 1,
            "every record carries the schema version"
        );
    }
    let meta = records.first().unwrap();
    assert_eq!(meta["type"], "meta");
    assert_eq!(meta["project"]["prefix"], "TRF");
    assert!(text.contains("first item"));
    assert!(text.contains("prove transfer"));
    assert!(text.contains("backup includes WAL"));
    assert!(text.contains("WAL checkpoint proof"));
    assert!(text.contains("depends_on_id"));

    // Credentials never enter the export, even when they sit beside the database.
    for secret in [
        &session_token,
        &rest_token,
        "secret-session",
        "token_hash",
        "rest-token",
    ] {
        assert!(!text.contains(secret), "export leaked {secret}");
    }
    for forbidden in [
        "\"claims\"",
        "\"session_id\"",
        "\"run_id\"",
        "\"idempotency_key\"",
        "\"token\"",
    ] {
        assert!(!text.contains(forbidden), "export contains {forbidden}");
    }
}

#[test]
fn export_removes_session_fields_from_nested_event_payloads() {
    let mut ledger = ledger();
    // A real claim_expired event carries `session_id`/`run_id` inside its payload;
    // sanitising only the top level of each record would leak them.
    let expired = ledger
        .store
        .expire_claim(&ledger.claim_id, 1_000_000)
        .unwrap();
    assert!(
        expired.is_some(),
        "fixture must produce a claim_expired event"
    );

    let mut out = Vec::new();
    ledger
        .store
        .export_ndjson(Some(&ledger.project.id), &mut out)
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(
        !text.contains("secret-session"),
        "session id leaked into the export"
    );

    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let expired_event = records
        .iter()
        .find(|record| record["type"] == "event" && record["kind"] == "claim_expired")
        .expect("claim_expired event is exported");
    let payload = &expired_event["payload"];
    assert!(
        payload.get("claim_id").is_some(),
        "claim_id is a non-secret audit reference"
    );
    for key in [
        "session_id",
        "run_id",
        "session",
        "run",
        "token",
        "token_hash",
        "idempotency_key",
    ] {
        assert!(
            payload.get(key).is_none(),
            "nested payload leaked {key}: {payload}"
        );
    }
    assert!(
        !text.contains("\"session_id\""),
        "no session_id field at any depth"
    );
}

fn apm(db: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_apm"));
    command.args(args).env("APM_DB", db);
    command
        .env_remove("APM_SESSION_ID")
        .env_remove("APM_RUN_ID");
    command.output().expect("spawn apm")
}

#[test]
fn cli_backup_and_export_write_safe_files_and_refuse_overwrite() {
    let ledger = ledger();
    let out_dir = ledger.dir.path().join("out");
    let backup = out_dir.join("trusted.db");
    let export = out_dir.join("audit.ndjson");

    let ok = apm(
        &ledger.db,
        &["backup", "--out", backup.to_str().unwrap(), "--json"],
    );
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let report: Value = serde_json::from_slice(&ok.stdout).unwrap();
    assert_eq!(report["backup"]["integrity"], "ok");
    assert!(backup.exists());

    let again = apm(&ledger.db, &["backup", "--out", backup.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&again.stderr).contains("refusing to overwrite"));

    let exported = apm(
        &ledger.db,
        &["export", "--out", export.to_str().unwrap(), "--json"],
    );
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let summary: Value = serde_json::from_slice(&exported.stdout).unwrap();
    assert!(summary["export"]["records"].as_u64().unwrap() >= 7);
    assert!(export.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&export).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "export file must be private");
    }

    let exported_again = apm(&ledger.db, &["export", "--out", export.to_str().unwrap()]);
    assert_eq!(exported_again.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&exported_again.stderr).contains("refusing to overwrite"));

    // stdout mode is pure NDJSON, one parseable document per line.
    let streamed = apm(&ledger.db, &["export"]);
    assert!(streamed.status.success());
    for line in String::from_utf8_lossy(&streamed.stdout).lines() {
        let value: Value = serde_json::from_str(line).expect("stdout is NDJSON");
        assert_eq!(value["schema_version"], 1);
    }
}
