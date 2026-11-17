//! CLI contract for workflow configuration, upgrades and legacy migration.
//!
//! These tests drive the real binary: machine-readable output, exit codes,
//! idempotency, concurrent transitions and the v2 -> v3 upgrade path.

use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};

struct Cli {
    dir: tempfile::TempDir,
    db: PathBuf,
}

struct Out {
    code: i32,
    json: Value,
    stderr: String,
}

impl Cli {
    fn new() -> Cli {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".apm").join("apm.db");
        Cli { dir, db }
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_apm"));
        cmd.args(args).env("APM_DB", &self.db);
        for key in [
            "APM_AGENT",
            "APM_SESSION_ID",
            "APM_RUN_ID",
            "APM_PROJECT",
            "APM_SIDE_RUN",
        ] {
            cmd.env_remove(key);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("spawn apm");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let code = out.status.code().unwrap_or(-1);
        let json = serde_json::from_str::<Value>(stdout.trim())
            .or_else(|_| serde_json::from_str::<Value>(stderr.trim()))
            .unwrap_or(Value::Null);
        Out { code, json, stderr }
    }

    fn run(&self, args: &[&str]) -> Out {
        self.run_with(args, &[])
    }

    fn init(&self) {
        let out = self.run(&[
            "init",
            "--name",
            "Workflow CLI",
            "--prefix",
            "WCL",
            "--json",
        ]);
        assert_eq!(out.code, 0, "init failed: {}", out.stderr);
    }

    fn write_json(&self, name: &str, value: &Value) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
        path
    }

    fn create(&self, title: &str, item_type: &str) -> String {
        let out = self.run(&[
            "create",
            "--title",
            title,
            "--goal",
            "goal",
            "--current-state",
            "state",
            "--type",
            item_type,
            "--json",
        ]);
        assert_eq!(out.code, 0, "create failed: {}", out.stderr);
        out.json["item"]["identifier"].as_str().unwrap().to_string()
    }
}

fn qa_flow(version: i64, state: &str) -> Value {
    json!({
        "id": "qa-flow",
        "version": version,
        "name": format!("QA flow v{version}"),
        "initial": "ready",
        "states": [
            {"key": "ready", "label": "Ready", "category": "open", "terminal": false},
            {"key": state, "label": "Review", "category": "review", "terminal": false},
            {"key": "done", "label": "Done", "category": "closed", "terminal": true},
            {"key": "cancelled", "label": "Cancelled", "category": "closed", "terminal": true, "cancel": true}
        ],
        "transitions": [
            {"from": "ready", "to": state, "requires": []},
            {"from": state, "to": "done", "requires": ["progress_or_note", "evidence_or_acceptance"]},
            {"from": state, "to": "cancelled", "requires": ["reason"]},
            {"from": "ready", "to": "cancelled", "requires": ["reason"]},
            {"from": "done", "to": state, "requires": ["reason"]},
            {"from": "cancelled", "to": "ready", "requires": ["reason"]}
        ],
        "guards": {
            "reason": {"kind": "reason"},
            "evidence_or_acceptance": {"kind": "evidence_or_acceptance"},
            "progress_or_note": {"kind": "progress_or_note"}
        }
    })
}

#[test]
fn show_list_types_and_validate_are_machine_readable() {
    let cli = Cli::new();
    cli.init();

    let out = cli.run(&["workflow", "--json"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.json["id"], "apm-default");
    assert_eq!(out.json["states"].as_array().unwrap().len(), 6);

    let out = cli.run(&["workflow", "show", "--json"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.json["version"], 1);

    let out = cli.run(&["workflow", "list", "--json"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.json["workflows"].as_array().unwrap().len(), 1);
    assert_eq!(out.json["workflows"][0]["is_project_default"], true);

    let out = cli.run(&["workflow", "types", "--json"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.json["default"]["workflow_id"], "apm-default");

    // Validation is pure: it works without a project database.
    let dir = tempfile::tempdir().unwrap();
    let valid_path = dir.path().join("qa.json");
    std::fs::write(&valid_path, serde_json::to_vec(&qa_flow(1, "qa")).unwrap()).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_apm"))
        .args([
            "workflow",
            "validate",
            "--file",
            valid_path.to_str().unwrap(),
            "--json",
        ])
        .current_dir(dir.path())
        .env_remove("APM_DB")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let payload: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(payload["valid"], true);
    assert_eq!(payload["states"], 4);

    // Invalid document: unreachable state -> invalid_input (exit 2), no writes.
    let mut broken = qa_flow(1, "qa");
    broken["states"].as_array_mut().unwrap().push(json!({
        "key": "icebox", "label": "Icebox", "category": "open", "terminal": false
    }));
    broken["transitions"].as_array_mut().unwrap().push(json!({
        "from": "icebox", "to": "done",
        "requires": ["progress_or_note", "evidence_or_acceptance"]
    }));
    let broken_path = cli.write_json("broken.json", &broken);
    let out = cli.run(&[
        "workflow",
        "validate",
        "--file",
        broken_path.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "invalid_input");
    let out = cli.run(&["workflow", "list", "--json"]);
    assert_eq!(out.json["workflows"].as_array().unwrap().len(), 1);
}

#[test]
fn set_binds_multiple_types_with_snapshots_and_is_idempotent() {
    let cli = Cli::new();
    cli.init();
    let path = cli.write_json("qa.json", &qa_flow(1, "qa"));

    let out = cli.run(&[
        "workflow",
        "set",
        "--file",
        path.to_str().unwrap(),
        "--bind",
        "spike,bug",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["created"], true);
    assert_eq!(out.json["bound"].as_array().unwrap().len(), 2);

    // Replaying the same document is a no-op with stable output.
    let again = cli.run(&[
        "workflow",
        "set",
        "--file",
        path.to_str().unwrap(),
        "--bind",
        "spike,bug",
        "--json",
    ]);
    assert_eq!(again.code, 0, "{}", again.stderr);
    assert_eq!(again.json["created"], false);
    let out = cli.run(&["workflow", "list", "--json"]);
    assert_eq!(
        out.json["workflows"].as_array().unwrap().len(),
        2,
        "no duplicate rows"
    );

    let types = cli.run(&["workflow", "types", "--json"]);
    let bindings = types.json["bindings"].as_array().unwrap();
    assert_eq!(bindings.len(), 2);
    assert!(bindings.iter().all(|b| b["workflow_id"] == "qa-flow"));

    // New items snapshot the binding; other types keep the default.
    let spike = cli.create("Spike item", "spike");
    let out = cli.run(&["get", &spike, "--json"]);
    assert_eq!(out.json["item"]["workflow_id"], "qa-flow");
    assert_eq!(out.json["item"]["status"], "ready");
    let task = cli.create("Task item", "task");
    let out = cli.run(&["get", &task, "--json"]);
    assert_eq!(out.json["item"]["workflow_id"], "apm-default");

    // Invalid config is rejected and leaves no workflow behind.
    let mut weak = qa_flow(9, "qa");
    for tr in weak["transitions"].as_array_mut().unwrap() {
        if tr["to"] == "done" {
            tr["requires"] = json!(["progress_or_note"]);
        }
    }
    let weak_path = cli.write_json("weak.json", &weak);
    let out = cli.run(&[
        "workflow",
        "set",
        "--file",
        weak_path.to_str().unwrap(),
        "--json",
    ]);
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "invalid_input");
    let out = cli.run(&["workflow", "list", "--json"]);
    assert!(
        !out.json["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["version"] == 9)
    );
}

#[test]
fn upgrade_migrates_open_items_and_refuses_unmapped_states() {
    let cli = Cli::new();
    cli.init();
    let v1 = cli.write_json("qa1.json", &qa_flow(1, "qa"));
    let v2 = cli.write_json("qa2.json", &qa_flow(2, "reviewing"));
    // Register v1 and bind the type to it; register v2 without re-binding yet.
    let out = cli.run(&[
        "workflow",
        "set",
        "--file",
        v1.to_str().unwrap(),
        "--bind",
        "spike",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let out = cli.run(&["workflow", "set", "--file", v2.to_str().unwrap(), "--json"]);
    assert_eq!(out.code, 0, "{}", out.stderr);

    let item = cli.create("Upgrade me", "spike");
    let env = [("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-up")];
    cli.run_with(&["claim", &item, "--json"], &env);
    let out = cli.run_with(&["status", &item, "--to", "qa", "--json"], &env);
    assert_eq!(out.code, 0, "{}", out.stderr);

    // Without a mapping the upgrade is blocked before any write.
    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--migrate-items",
        "--json",
    ]);
    assert_eq!(out.code, 6);
    assert_eq!(out.json["error"]["code"], "workflow_violation");
    let out = cli.run(&["get", &item, "--json"]);
    assert_eq!(
        out.json["item"]["workflow_version"], 1,
        "no partial migration"
    );

    // Dry run reports the plan without writing.
    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--migrate-items",
        "--map",
        "qa=reviewing",
        "--dry-run",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["dry_run"], true);
    assert_eq!(out.json["migrated"], 1);
    assert_eq!(out.json["items"][0]["action"], "migrate");
    let out = cli.run(&["get", &item, "--json"]);
    assert_eq!(out.json["item"]["workflow_version"], 1);

    // Apply: snapshot moves, state maps, audit event recorded.
    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--migrate-items",
        "--map",
        "qa=reviewing",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["migrated"], 1);
    let out = cli.run(&["get", &item, "--json"]);
    assert_eq!(out.json["item"]["workflow_version"], 2);
    assert_eq!(out.json["item"]["status"], "reviewing");
    let events = cli.run(&["events", &item, "--json"]);
    assert!(
        events.json["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "workflow_migrated")
    );

    // Re-running the upgrade is a no-op and adds no events.
    let before = events.json["events"].as_array().unwrap().len();
    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--migrate-items",
        "--map",
        "qa=reviewing",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["migrated"], 0);
    assert_eq!(out.json["binding_changed"], false);
    let after = cli.run(&["events", &item, "--json"]).json["events"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(after, before, "idempotent upgrade must not write events");
}

#[test]
fn concurrent_transitions_have_a_single_winner() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let cli = Arc::new(Cli::new());
    cli.init();
    let path = cli.write_json("qa.json", &qa_flow(1, "qa"));
    let out = cli.run(&[
        "workflow",
        "set",
        "--file",
        path.to_str().unwrap(),
        "--bind",
        "spike",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let item = cli.create("Race me", "spike");
    let env = [("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-race")];
    assert_eq!(cli.run_with(&["claim", &item, "--json"], &env).code, 0);

    let ok = Arc::new(AtomicUsize::new(0));
    let conflicts = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for _ in 0..6 {
        let cli = Arc::clone(&cli);
        let item = item.clone();
        let ok = Arc::clone(&ok);
        let conflicts = Arc::clone(&conflicts);
        let env = env.to_vec();
        handles.push(std::thread::spawn(move || {
            let out = cli.run_with(
                &[
                    "status",
                    &item,
                    "--to",
                    "qa",
                    "--expect-version",
                    "1",
                    "--json",
                ],
                &env,
            );
            match out.code {
                0 => ok.fetch_add(1, Ordering::SeqCst),
                4 => conflicts.fetch_add(1, Ordering::SeqCst),
                _ => panic!("unexpected exit {}: {}", out.code, out.stderr),
            };
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(
        ok.load(Ordering::SeqCst),
        1,
        "exactly one transition must win"
    );
    assert_eq!(conflicts.load(Ordering::SeqCst), 5);
    let out = cli.run(&["get", &item, "--json"]);
    assert_eq!(out.json["item"]["status"], "qa");
    assert_eq!(out.json["item"]["version"], 2);
}

#[test]
fn legacy_v2_database_is_upgraded_on_first_cli_use() {
    let cli = Cli::new();
    cli.init();
    let item = cli.create("Legacy item", "task");

    // Downgrade the file to the pre-workflow shape and plant legacy workflow JSON.
    let mut legacy = qa_flow(7, "qa");
    legacy["id"] = json!("legacy-flow");
    let conn = rusqlite::Connection::open(&cli.db).unwrap();
    conn.execute_batch(
        "DROP TABLE type_workflows;
         DROP TABLE workflows;
         ALTER TABLE work_items DROP COLUMN workflow_id;
         ALTER TABLE work_items DROP COLUMN workflow_version;
         ALTER TABLE work_items DROP COLUMN state_category;
         ALTER TABLE projects DROP COLUMN default_workflow_id;
         ALTER TABLE projects DROP COLUMN default_workflow_version;",
    )
    .unwrap();
    conn.execute(
        "UPDATE projects SET workflow_json = ?1",
        rusqlite::params![serde_json::to_string(&legacy).unwrap()],
    )
    .unwrap();
    conn.execute("UPDATE work_items SET status = 'legacy_state'", [])
        .unwrap();
    conn.pragma_update(None, "user_version", 2).unwrap();
    drop(conn);

    // Any CLI command opens (and migrates) the ledger.
    let out = cli.run(&["workflow", "list", "--json"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let ids: Vec<String> = out.json["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap().to_string())
        .collect();
    assert!(ids.contains(&"legacy-flow".to_string()));

    let out = cli.run(&["get", &item, "--json"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["item"]["workflow_id"], "legacy-flow");
    assert_eq!(out.json["item"]["workflow_version"], 7);
    // The orphan state was repaired to the workflow's initial state.
    assert_eq!(out.json["item"]["status"], "ready");
    let events = cli.run(&["events", &item, "--json"]);
    assert!(
        events.json["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "workflow_migrated")
    );
}

fn project_event_count(cli: &Cli, kind: &str) -> usize {
    let out = cli.run(&["events", "--limit", "500", "--json"]);
    out.json["events"]
        .as_array()
        .map(|events| events.iter().filter(|e| e["kind"] == kind).count())
        .unwrap_or(0)
}

#[test]
fn set_is_atomic_and_idempotent() {
    let cli = Cli::new();
    cli.init();
    let path = cli.write_json("qa.json", &qa_flow(1, "qa"));

    // Second binding is invalid: the whole `set` must roll back.
    let out = cli.run(&[
        "workflow",
        "set",
        "--file",
        path.to_str().unwrap(),
        "--bind",
        "spike,bad type",
        "--json",
    ]);
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "invalid_input");
    let list = cli.run(&["workflow", "list", "--json"]);
    assert_eq!(
        list.json["workflows"].as_array().unwrap().len(),
        1,
        "workflow must not be registered when a later binding is invalid"
    );
    let types = cli.run(&["workflow", "types", "--json"]);
    assert!(types.json["bindings"].as_array().unwrap().is_empty());
    assert_eq!(project_event_count(&cli, "workflow_registered"), 0);
    assert_eq!(project_event_count(&cli, "type_workflow_bound"), 0);

    // Successful set, then the exact replay: no new rows and no new events.
    let args = [
        "workflow",
        "set",
        "--file",
        path.to_str().unwrap(),
        "--bind",
        "spike",
        "--default",
        "--json",
    ];
    let first = cli.run(&args);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.json["created"], true);
    assert_eq!(first.json["made_default"], true);
    let after_first = (
        project_event_count(&cli, "workflow_registered"),
        project_event_count(&cli, "type_workflow_bound"),
        project_event_count(&cli, "project_default_workflow_changed"),
    );
    assert_eq!(after_first, (1, 1, 1));

    let second = cli.run(&args);
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.json["created"], false);
    assert_eq!(second.json["made_default"], false);
    let after_second = (
        project_event_count(&cli, "workflow_registered"),
        project_event_count(&cli, "type_workflow_bound"),
        project_event_count(&cli, "project_default_workflow_changed"),
    );
    assert_eq!(
        after_second, after_first,
        "idempotent replay must not add events"
    );
}

#[test]
fn closed_items_keep_their_snapshot_through_upgrades() {
    let cli = Cli::new();
    cli.init();
    let v1 = cli.write_json("v1.json", &qa_flow(1, "qa"));
    // v2 keeps only `done` as a closed state (no `cancelled`), and renames qa.
    let mut v2 = qa_flow(2, "reviewing");
    v2["states"] = json!([
        {"key": "ready", "label": "Ready", "category": "open", "terminal": false},
        {"key": "reviewing", "label": "Review", "category": "review", "terminal": false},
        {"key": "done", "label": "Done", "category": "closed", "terminal": true}
    ]);
    v2["transitions"] = json!([
        {"from": "ready", "to": "reviewing", "requires": []},
        {"from": "reviewing", "to": "done", "requires": ["progress_or_note", "evidence_or_acceptance"]},
        {"from": "done", "to": "reviewing", "requires": ["reason"]}
    ]);
    let v2 = cli.write_json("v2.json", &v2);
    assert_eq!(
        cli.run(&[
            "workflow",
            "set",
            "--file",
            v1.to_str().unwrap(),
            "--bind",
            "spike",
            "--json"
        ])
        .code,
        0
    );
    assert_eq!(
        cli.run(&["workflow", "set", "--file", v2.to_str().unwrap(), "--json"])
            .code,
        0
    );

    // Closed item whose state exists in both workflows: still keeps v1.
    let both = cli.create("Closed, state exists", "spike");
    // Closed item whose state only exists in v1: also keeps v1.
    let only_v1 = cli.create("Closed, state removed", "spike");
    let env = [("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-closed")];

    assert_eq!(cli.run_with(&["claim", &both, "--json"], &env).code, 0);
    // qa-flow v1 has no direct cancel guard failure: cancel needs a reason.
    let out = cli.run_with(
        &[
            "status",
            &both,
            "--to",
            "cancelled",
            "--reason",
            "not needed",
            "--json",
        ],
        &env,
    );
    assert_eq!(out.code, 0, "{}", out.stderr);

    assert_eq!(cli.run_with(&["claim", &only_v1, "--json"], &env).code, 0);
    let out = cli.run_with(
        &[
            "status",
            &only_v1,
            "--to",
            "cancelled",
            "--reason",
            "not needed",
            "--json",
        ],
        &env,
    );
    assert_eq!(out.code, 0, "{}", out.stderr);

    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--migrate-items",
        "--map",
        "qa=reviewing",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["migrated"], 0);
    for identifier in [&both, &only_v1] {
        let item = cli.run(&["get", identifier, "--json"]);
        assert_eq!(
            item.json["item"]["workflow_version"], 1,
            "{identifier} kept its snapshot"
        );
        assert_eq!(item.json["item"]["status"], "cancelled");
    }
    let actions: Vec<String> = out.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["action"].as_str().unwrap().to_string())
        .collect();
    assert!(actions.iter().all(|a| a == "skipped_closed"), "{actions:?}");

    // Reopening a closed item is a human action: agents may not claim closed
    // items, so the reopen goes through the human path. After that the item is
    // open again on its old snapshot and upgrades like any other open item.
    let reopened = cli.run_with(
        &[
            "--human",
            "status",
            &only_v1,
            "--to",
            "ready",
            "--reason",
            "reopening",
            "--json",
        ],
        &env,
    );
    assert_eq!(reopened.code, 0, "{}", reopened.stderr);
    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--migrate-items",
        "--map",
        "qa=reviewing",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["migrated"], 1);
    let item = cli.run(&["get", &only_v1, "--json"]);
    assert_eq!(item.json["item"]["workflow_version"], 2);
}

#[test]
fn upgrades_reject_closed_mappings_and_typos_without_writing() {
    let cli = Cli::new();
    cli.init();
    let v1 = cli.write_json("m1.json", &qa_flow(1, "qa"));
    // v2 drops `qa` and adds a closed `archived` state.
    let mut v2 = qa_flow(2, "reviewing");
    v2["states"] = json!([
        {"key": "ready", "label": "Ready", "category": "open", "terminal": false},
        {"key": "reviewing", "label": "Review", "category": "review", "terminal": false},
        {"key": "done", "label": "Done", "category": "closed", "terminal": true},
        {"key": "archived", "label": "Archived", "category": "closed", "terminal": true}
    ]);
    v2["transitions"] = json!([
        {"from": "ready", "to": "reviewing", "requires": []},
        {"from": "reviewing", "to": "done", "requires": ["progress_or_note", "evidence_or_acceptance"]},
        {"from": "reviewing", "to": "archived", "requires": ["progress_or_note", "evidence_or_acceptance"]},
        {"from": "done", "to": "archived", "requires": ["progress_or_note", "evidence_or_acceptance"]}
    ]);
    let v2 = cli.write_json("m2.json", &v2);
    assert_eq!(
        cli.run(&[
            "workflow",
            "set",
            "--file",
            v1.to_str().unwrap(),
            "--bind",
            "spike",
            "--json"
        ])
        .code,
        0
    );
    assert_eq!(
        cli.run(&["workflow", "set", "--file", v2.to_str().unwrap(), "--json"])
            .code,
        0
    );

    let item = cli.create("Open item", "spike");
    let env = [("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-map")];
    cli.run_with(&["claim", &item, "--json"], &env);
    assert_eq!(
        cli.run_with(&["status", &item, "--to", "qa", "--json"], &env)
            .code,
        0
    );

    // Mapping an open item into a closed state is refused: closing must go
    // through `apm status` so the completion guards run.
    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--migrate-items",
        "--map",
        "qa=archived",
        "--json",
    ]);
    assert_eq!(out.code, 6);
    assert_eq!(out.json["error"]["code"], "workflow_violation");
    assert!(
        out.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("without satisfying the completion guards")
    );
    let unchanged = cli.run(&["get", &item, "--json"]);
    assert_eq!(unchanged.json["item"]["workflow_version"], 1);
    assert_eq!(unchanged.json["item"]["status"], "qa");

    // A typo in the mapping target is reported as such, not "no mapping".
    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--migrate-items",
        "--map",
        "qa=archivd",
        "--json",
    ]);
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "invalid_input");
    assert!(
        out.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("mapping target 'archivd'")
    );
}

#[test]
fn rebind_without_migrating_reports_already_and_rebind_only() {
    let cli = Cli::new();
    cli.init();
    let v1 = cli.write_json("r1.json", &qa_flow(1, "qa"));
    let v2 = cli.write_json("r2.json", &qa_flow(2, "reviewing"));
    assert_eq!(
        cli.run(&[
            "workflow",
            "set",
            "--file",
            v1.to_str().unwrap(),
            "--bind",
            "spike",
            "--json"
        ])
        .code,
        0
    );
    let old_item = cli.create("On v1", "spike");
    assert_eq!(
        cli.run(&["workflow", "set", "--file", v2.to_str().unwrap(), "--json"])
            .code,
        0
    );

    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let actions: Vec<String> = out.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["action"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(actions, vec!["rebind_only"]);
    let unchanged = cli.run(&["get", &old_item, "--json"]);
    assert_eq!(unchanged.json["item"]["workflow_version"], 1);

    // A new item uses the new binding; re-running reports it as `already`.
    let new_item = cli.create("On v2", "spike");
    let out = cli.run(&[
        "workflow",
        "upgrade",
        "--type",
        "spike",
        "--to-version",
        "2",
        "--json",
    ]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let actions: Vec<(String, String)> = out.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["identifier"].as_str().unwrap().to_string(),
                i["action"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert!(
        actions.contains(&(new_item.clone(), "already".to_string())),
        "{actions:?}"
    );
}
