//! End-to-end tests for the MCP stdio server (`apm-mcp`).
//!
//! These spawn the real binary and drive it like an MCP client would: one
//! newline-delimited JSON-RPC message per line on stdin/stdout. The database is
//! created by the real `apm` CLI first, so the fixture is exactly what a user
//! gets.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use apm::workflow::{StateDef, TransitionDef, Workflow};
use apm::{Actor, Category, Store, now_ms};
use serde_json::{Value, json};

struct Fixture {
    dir: tempfile::TempDir,
    db: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".apm").join("apm.db");
        let fixture = Fixture { dir, db };
        fixture.apm(&["init", "--name", "MCP E2E", "--prefix", "E2E", "--json"]);
        fixture
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Run the `apm` CLI (not the MCP server) for fixture setup.
    fn apm(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_apm"))
            .args(args)
            .env("APM_DB", &self.db)
            .env_remove("APM_SESSION_ID")
            .env_remove("APM_RUN_ID")
            .env_remove("APM_SIDE_RUN")
            .output()
            .expect("spawn apm");
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        assert!(
            output.status.success(),
            "apm {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_str(&stdout).unwrap_or(Value::Null)
    }

    fn create(&self, title: &str) -> String {
        self.create_typed(title, "task")
    }

    fn create_typed(&self, title: &str, item_type: &str) -> String {
        let created = self.apm(&[
            "create",
            "--title",
            title,
            "--goal",
            "prove the MCP transport works",
            "--current-state",
            "the adapter exists but is untested",
            "--constraint",
            "no daemon",
            "--reference",
            "docs/mcp.md",
            "--acceptance",
            "initialize/tools/list/tools/call work",
            "--type",
            item_type,
            "--json",
        ]);
        created["item"]["identifier"].as_str().unwrap().to_string()
    }

    /// Register a workflow with an extra `qa` state and bind it to one item
    /// type. The CLI has no workflow-admin command yet, so the fixture uses the
    /// store API — the same call a future `apm workflow register` would make.
    fn seed_custom_workflow(&self) {
        let mut store = Store::open(&self.db).expect("open the ledger");
        let actor = Actor::human("fixture");
        let at = now_ms();
        let mut workflow = Workflow::default_v1();
        workflow.id = "qa-flow".to_string();
        workflow.version = 1;
        workflow.name = "QA flow".to_string();
        workflow.states.push(StateDef {
            key: "qa".to_string(),
            label: "QA".to_string(),
            category: Category::Active,
            terminal: false,
            cancel: false,
        });
        for (from, to, requires) in [
            ("ready", "qa", vec![]),
            ("qa", "doing", vec![]),
            (
                "qa",
                "done",
                vec![
                    "evidence_or_acceptance".to_string(),
                    "progress_or_note".to_string(),
                ],
            ),
        ] {
            workflow.transitions.push(TransitionDef {
                from: from.to_string(),
                to: to.to_string(),
                requires,
            });
        }
        store
            .register_workflow(None, workflow, &actor, at)
            .expect("register the custom workflow");
        store
            .bind_type_workflow(None, "spike", "qa-flow", 1, &actor, at)
            .expect("bind the custom workflow to the spike type");
    }
}

/// A live `apm-mcp` process plus the framing helpers a client needs.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpClient {
    fn start(db: &Path, session: &str) -> McpClient {
        let mut child = Command::new(env!("CARGO_BIN_EXE_apm-mcp"))
            .env("APM_DB", db)
            .env("APM_SESSION_ID", session)
            .env("APM_AGENT", "e2e-mcp")
            .env_remove("APM_RUN_ID")
            .env_remove("APM_SIDE_RUN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn apm-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        McpClient {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").expect("write frame");
        self.stdin.flush().expect("flush frame");
    }

    fn notify(&mut self, method: &str) {
        self.send(json!({ "jsonrpc": "2.0", "method": method }));
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).expect("read frame");
        assert!(read > 0, "server closed stdout unexpectedly");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid frame {line:?}: {e}"))
    }

    fn request(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.read()
    }

    fn handshake(&mut self) {
        let init = self.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "apm-e2e", "version": "0.1.0" }
            }),
        );
        assert_eq!(init["result"]["protocolVersion"], json!("2025-06-18"));
        self.notify("notifications/initialized");
    }

    fn call(&mut self, id: i64, name: &str, arguments: Value) -> Value {
        self.request(
            id,
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    /// Close stdin, drain the rest, and report the exit status + stderr.
    fn finish(mut self) -> (Option<i32>, String) {
        drop(self.stdin);
        let mut rest = String::new();
        let _ = std::io::Read::read_to_string(&mut self.stdout, &mut rest);
        let status = self.child.wait().expect("wait for apm-mcp");
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
        }
        (status.code(), stderr)
    }
}

fn text_of(response: &Value) -> String {
    response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn structured(response: &Value) -> Value {
    response["result"]["structuredContent"].clone()
}

#[test]
fn stdio_session_covers_the_core_tool_mapping() {
    let fixture = Fixture::new();
    let identifier = fixture.create("drive the ledger over stdio");
    let mut client = McpClient::start(&fixture.db, "e2e-session");
    client.handshake();

    // tools/list
    let listed = client.request(2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for required in [
        "apm_dashboard",
        "apm_get",
        "apm_claim",
        "apm_progress",
        "apm_status",
        "apm_context",
    ] {
        assert!(names.contains(&required), "missing {required} in {names:?}");
    }

    // dashboard
    let board = client.call(3, "apm_dashboard", json!({}));
    assert!(
        board["error"].is_null(),
        "dashboard protocol error: {board}"
    );
    assert!(
        result_text_contains(&board, "READY / UNCLAIMED (1)"),
        "{}",
        text_of(&board)
    );

    // get
    let detail = client.call(4, "apm_get", json!({ "item": identifier }));
    let item = &structured(&detail)["item"];
    assert_eq!(item["identifier"], json!(identifier));
    let version = item["version"].as_i64().unwrap();

    // claim: no plaintext token anywhere in the frame
    let claimed = client.call(
        5,
        "apm_claim",
        json!({ "item": identifier, "lease_seconds": 600 }),
    );
    let frame = claimed.to_string();
    assert_eq!(structured(&claimed)["token"], Value::Null);
    assert_eq!(structured(&claimed)["token_suppressed"], json!(true));
    assert!(
        !text_of(&claimed).contains("token: apm_"),
        "{}",
        text_of(&claimed)
    );

    // The 0600 session file exists and holds a token-shaped secret.
    let sessions = fixture.dir().join(".apm").join("sessions");
    let files: Vec<PathBuf> = std::fs::read_dir(&sessions)
        .expect("session dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    assert_eq!(files.len(), 1, "one session token file expected");
    let raw = std::fs::read_to_string(&files[0]).unwrap();
    let stored: Value = serde_json::from_str(&raw).expect("session file is JSON");
    let token = stored[0]["token"]
        .as_str()
        .expect("stored token")
        .to_string();
    assert!(
        token.starts_with("apm_"),
        "session file should hold the claim token"
    );
    assert!(
        !frame.contains(&token),
        "the session-file token must not appear in the tool result"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&files[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "session file must be 0600");
    }

    // status -> doing (token resolved from the session file by the server)
    let doing = client.call(
        6,
        "apm_status",
        json!({ "item": identifier, "to": "doing", "expect_version": version }),
    );
    assert!(doing["error"].is_null(), "{doing}");
    assert_eq!(structured(&doing)["item"]["status"], json!("doing"));
    let doing_version = structured(&doing)["item"]["version"].as_i64().unwrap();

    // progress
    let progress = client.call(
        7,
        "apm_progress",
        json!({
            "item": identifier,
            "summary": "transport verified end to end",
            "next_action": "publish docs/mcp.md",
            "expect_version": doing_version
        }),
    );
    assert!(progress["error"].is_null(), "{progress}");
    assert_eq!(structured(&progress)["substantive"], json!(true));

    // context show + revise
    let shown = client.call(
        8,
        "apm_context",
        json!({ "item": identifier, "action": "show" }),
    );
    assert_eq!(structured(&shown)["context"]["version"], json!(1));
    let revised = client.call(
        9,
        "apm_context",
        json!({
            "item": identifier,
            "action": "revise",
            "reason": "documented the transport",
            "current_state": "stdio adapter verified"
        }),
    );
    assert!(revised["error"].is_null(), "{revised}");
    assert_eq!(structured(&revised)["context"]["version"], json!(2));
    assert_eq!(
        structured(&revised)["context"]["doc"]["goal"],
        json!("prove the MCP transport works")
    );

    // A stale expect_version is reported as a domain error, not a crash.
    let stale = client.call(
        10,
        "apm_progress",
        json!({ "item": identifier, "summary": "raced", "expect_version": 1 }),
    );
    assert_eq!(stale["result"]["isError"], json!(true));
    assert_eq!(
        structured(&stale)["error"]["code"],
        json!("version_conflict")
    );

    // release, then the ledger shows the item unclaimed again
    let released = client.call(11, "apm_release", json!({ "item": identifier }));
    assert!(released["error"].is_null(), "{released}");
    assert_eq!(structured(&released)["claim"]["status"], json!("released"));

    // Unknown method is a protocol error; the stream keeps working afterwards.
    let unknown = client.request(12, "resources/list", json!({}));
    assert_eq!(unknown["error"]["code"], json!(-32601));
    let ping = client.request(13, "ping", json!({}));
    assert_eq!(ping["result"], json!({}));

    let (code, stderr) = client.finish();
    assert_eq!(code, Some(0), "clean EOF should exit 0; stderr: {stderr}");
    assert!(
        !stderr.contains("apm_"),
        "diagnostics must not carry tokens: {stderr}"
    );
}

#[test]
fn custom_workflow_state_flows_through_mcp() {
    let fixture = Fixture::new();
    fixture.seed_custom_workflow();
    let identifier = fixture.create_typed("spike with a custom state", "spike");
    let mut client = McpClient::start(&fixture.db, "custom-flow");
    client.handshake();

    // The catalogue must not hide custom states behind a fixed enum.
    let listed = client.request(2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    let status_tool = tools
        .iter()
        .find(|t| t["name"] == json!("apm_status"))
        .unwrap();
    let to_schema = &status_tool["inputSchema"]["properties"]["to"];
    assert_eq!(to_schema["type"], json!("string"));
    assert!(
        to_schema.get("enum").is_none(),
        "apm_status.to must not pin an enum"
    );

    let detail = client.call(3, "apm_get", json!({ "item": identifier }));
    let version = structured(&detail)["item"]["version"].as_i64().unwrap();
    assert_eq!(
        structured(&detail)["item"]["workflow_id"],
        json!("qa-flow"),
        "the item must carry its workflow snapshot"
    );

    let claimed = client.call(4, "apm_claim", json!({ "item": identifier }));
    assert!(claimed["error"].is_null(), "{claimed}");

    // ready -> qa is a custom transition; the state key is a free-form slug.
    let to_qa = client.call(
        5,
        "apm_status",
        json!({ "item": identifier, "to": "qa", "expect_version": version }),
    );
    assert!(to_qa["error"].is_null(), "{to_qa}");
    assert_eq!(structured(&to_qa)["item"]["status"], json!("qa"));
    assert_eq!(
        structured(&to_qa)["item"]["status_category"],
        json!("active")
    );
    let qa_version = structured(&to_qa)["item"]["version"].as_i64().unwrap();

    // A state the workflow does not declare is a workflow violation, not a
    // protocol error: the model must be able to read and recover from it.
    let bogus = client.call(
        6,
        "apm_status",
        json!({ "item": identifier, "to": "shipped", "expect_version": qa_version }),
    );
    assert_eq!(bogus["result"]["isError"], json!(true));
    assert_eq!(
        structured(&bogus)["error"]["code"],
        json!("workflow_violation")
    );

    // The completion guards still apply inside a custom workflow.
    let done = client.call(
        7,
        "apm_status",
        json!({ "item": identifier, "to": "done", "note": "looks fine to me" }),
    );
    assert_eq!(done["result"]["isError"], json!(true));
    assert_eq!(
        structured(&done)["error"]["code"],
        json!("completion_evidence_required")
    );

    // Existing six-state items keep the default workflow: a custom state is not
    // silently accepted just because the server learned about it.
    let plain = fixture.create("default workflow item");
    let plain_detail = client.call(8, "apm_get", json!({ "item": plain }));
    assert_eq!(
        structured(&plain_detail)["item"]["workflow_id"],
        json!("apm-default")
    );

    let (code, stderr) = client.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn two_sessions_cannot_hold_the_same_item() {
    let fixture = Fixture::new();
    let identifier = fixture.create("contended item");

    let mut first = McpClient::start(&fixture.db, "session-a");
    first.handshake();
    let won = first.call(2, "apm_claim", json!({ "item": identifier }));
    assert!(won["error"].is_null(), "{won}");

    let mut second = McpClient::start(&fixture.db, "session-b");
    second.handshake();
    let lost = second.call(2, "apm_claim", json!({ "item": identifier }));
    assert_eq!(lost["result"]["isError"], json!(true));
    let error = &structured(&lost)["error"];
    assert_eq!(error["code"], json!("claim_conflict"));
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .contains("report who holds it"),
        "hint should tell the model not to work around the claim: {error}"
    );
    // A write from the losing session is refused too.
    let write = second.call(
        3,
        "apm_progress",
        json!({ "item": identifier, "summary": "sneaky write" }),
    );
    assert_eq!(write["result"]["isError"], json!(true));

    let (code, stderr) = first.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let (code, stderr) = second.finish();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn missing_database_is_a_clean_failure() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_apm-mcp"))
        .current_dir(dir.path())
        .env_remove("APM_DB")
        .stdin(Stdio::null())
        .output()
        .expect("spawn apm-mcp");
    assert_eq!(output.status.code(), Some(3), "not_found exits 3");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no .apm/apm.db"), "stderr: {stderr}");
    assert!(output.stdout.is_empty(), "stdout must stay protocol-only");

    // An explicit but missing --db must not be created behind the client's back.
    let missing = dir.path().join("nope").join("apm.db");
    let output = Command::new(env!("CARGO_BIN_EXE_apm-mcp"))
        .args(["--db", missing.to_str().unwrap()])
        .stdin(Stdio::null())
        .output()
        .expect("spawn apm-mcp");
    assert_eq!(output.status.code(), Some(3));
    assert!(
        !missing.exists(),
        "the server must never create a database implicitly"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("apm init"), "stderr: {stderr}");
}

fn result_text_contains(response: &Value, needle: &str) -> bool {
    text_of(response).contains(needle)
}
