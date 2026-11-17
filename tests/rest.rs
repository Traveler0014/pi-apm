//! End-to-end tests for the optional REST adapter (`apm-serve`).
//!
//! These spawn the real binary on an ephemeral loopback port and talk to it with
//! a real TCP client, so the HTTP framing, auth, routing and error mapping are all
//! exercised — not just the handlers.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TOKEN: &str = "apm_rest_test_token_1234567890";

struct Ledger {
    dir: tempfile::TempDir,
    db: PathBuf,
}

impl Ledger {
    fn new() -> Ledger {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".apm").join("apm.db");
        let ledger = Ledger { dir, db };
        ledger.apm(&["init", "--name", "REST Test", "--prefix", "RST", "--json"]);
        ledger
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

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
        let created = self.apm(&[
            "create",
            "--title",
            title,
            "--goal",
            "prove the REST adapter is a thin translation",
            "--current-state",
            "the adapter exists",
            "--constraint",
            "loopback only",
            "--acceptance",
            "a REST client can complete the loop",
            "--json",
        ]);
        created["item"]["identifier"].as_str().unwrap().to_string()
    }
}

struct Server {
    child: Child,
    addr: String,
}

impl Server {
    fn start(ledger: &Ledger, extra: &[&str]) -> Server {
        let mut args = vec![
            "--db",
            ledger.db.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--token",
            TOKEN,
            "--agent",
            "rest-test",
        ];
        args.extend_from_slice(extra);
        let mut child = Command::new(env!("CARGO_BIN_EXE_apm-serve"))
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn apm-serve");
        let stdout = child.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut addr = None;
        while Instant::now() < deadline {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            if let Some(rest) = line.trim().strip_prefix("listening on ") {
                addr = Some(rest.to_string());
                break;
            }
        }
        let addr = addr.unwrap_or_else(|| panic!("apm-serve did not report an address"));
        // Keep draining stdout so the child never blocks on a full pipe.
        std::thread::spawn(move || {
            let mut sink = String::new();
            let _ = reader.read_to_string(&mut sink);
        });
        Server { child, addr }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct Reply {
    status: u16,
    body: Value,
}

impl Reply {
    fn error_code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
}

fn request(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(&str, &str)],
) -> Reply {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let payload = body.map(|value| value.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n"
    );
    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    if !payload.is_empty() {
        stream.write_all(payload.as_bytes()).unwrap();
    }
    stream.flush().unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("complete response");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    let body = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
    Reply { status, body }
}

fn cookie_free(addr: &str, method: &str, path: &str) -> Reply {
    // A request with no Authorization header at all.
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("complete response");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(0);
    Reply {
        status,
        body: serde_json::from_str(body.trim()).unwrap_or(Value::Null),
    }
}

#[test]
fn health_is_open_but_everything_else_requires_the_token() {
    let ledger = Ledger::new();
    let server = Server::start(&ledger, &[]);

    let health = cookie_free(&server.addr, "GET", "/health");
    assert_eq!(health.status, 200);
    assert_eq!(health.body["service"], json!("apm-rest"));
    assert!(
        health.body["dashboard"].is_null(),
        "health must not leak ledger state"
    );

    let unauthenticated = cookie_free(&server.addr, "GET", "/dashboard");
    assert_eq!(unauthenticated.status, 401);
    assert_eq!(unauthenticated.error_code(), "unauthorized");

    let wrong = request(
        &server.addr,
        "GET",
        "/dashboard",
        None,
        &[("Authorization", "Bearer nope")],
    );
    assert_eq!(wrong.status, 401);

    let authed = request(&server.addr, "GET", "/dashboard", None, &[]);
    assert_eq!(authed.status, 200);
    assert_eq!(authed.body["ok"], json!(true));
    assert_eq!(
        authed.body["dashboard"]["buckets"]["ready_unclaimed"],
        json!([])
    );
}

#[test]
fn browser_origins_and_foreign_hosts_are_refused() {
    let ledger = Ledger::new();
    let server = Server::start(&ledger, &[]);

    let origin = request(
        &server.addr,
        "GET",
        "/items",
        None,
        &[("Origin", "https://evil.example")],
    );
    assert_eq!(origin.status, 403);
    assert_eq!(origin.error_code(), "forbidden_origin");

    let host = request(
        &server.addr,
        "GET",
        "/items",
        None,
        &[("Host", "evil.example.com")],
    );
    assert_eq!(host.status, 403);
    assert_eq!(host.error_code(), "forbidden_host");
}

#[test]
fn items_claims_progress_and_transitions_work_end_to_end() {
    let ledger = Ledger::new();
    let identifier = ledger.create("drive the REST loop");
    let server = Server::start(&ledger, &[]);

    // list + get
    let list = request(&server.addr, "GET", "/items", None, &[]);
    assert_eq!(list.status, 200);
    assert_eq!(list.body["count"], json!(1));
    assert_eq!(list.body["items"][0]["identifier"], json!(identifier));

    let detail = request(
        &server.addr,
        "GET",
        &format!("/items/{identifier}"),
        None,
        &[],
    );
    assert_eq!(detail.status, 200);
    let version = detail.body["item"]["item"]["version"].as_i64().unwrap();

    // claim: no plaintext token in the response
    let claimed = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({ "lease_seconds": 600 })),
        &[],
    );
    assert_eq!(claimed.status, 200, "{}", claimed.body);
    assert_eq!(claimed.body["token"], Value::Null);
    assert_eq!(claimed.body["token_suppressed"], json!(true));
    assert_eq!(claimed.body["token_source"], json!("session_file"));
    assert!(!claimed.body.to_string().contains("apm_rest_test_token"));

    // claims listing never returns tokens
    let claims = request(&server.addr, "GET", "/claims", None, &[]);
    assert_eq!(claims.status, 200);
    assert_eq!(claims.body["count"], json!(1));
    assert_eq!(claims.body["tokens_suppressed"], json!(true));
    assert!(claims.body["tokens"].is_null());

    // transition
    let doing = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/status"),
        Some(&json!({ "to": "doing", "expect_version": version })),
        &[],
    );
    assert_eq!(doing.status, 200, "{}", doing.body);
    assert_eq!(doing.body["item"]["status"], json!("doing"));
    let doing_version = doing.body["item"]["version"].as_i64().unwrap();

    // progress
    let progress = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({
            "summary": "REST adapter drove the loop",
            "next_action": "document it",
            "expect_version": doing_version
        })),
        &[],
    );
    assert_eq!(progress.status, 200, "{}", progress.body);
    assert_eq!(progress.body["substantive"], json!(true));
    assert_eq!(
        progress.body["item"]["progress_summary"],
        json!("REST adapter drove the loop")
    );

    // release
    let released = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/release"),
        Some(&json!({ "reason": "done for now" })),
        &[],
    );
    assert_eq!(released.status, 200, "{}", released.body);
    assert_eq!(released.body["claim"]["status"], json!("released"));

    // the session token file is gone, and no token ever crossed the wire
    let sessions = ledger.dir().join(".apm").join("sessions");
    if let Ok(entries) = std::fs::read_dir(&sessions) {
        for entry in entries.flatten() {
            let raw = std::fs::read_to_string(entry.path()).unwrap_or_default();
            assert!(
                !raw.contains(TOKEN),
                "the REST token must not be stored as a claim token"
            );
        }
    }
}

#[test]
fn errors_keep_the_cli_codes_with_http_statuses() {
    let ledger = Ledger::new();
    let identifier = ledger.create("exercise REST error paths");
    let server = Server::start(&ledger, &[]);

    let missing = request(&server.addr, "GET", "/items/RST-999", None, &[]);
    assert_eq!(missing.status, 404);
    assert_eq!(missing.error_code(), "not_found");

    // A write with no claim: 409 + claim_required, not a 500.
    let unclaimed = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "sneaky" })),
        &[],
    );
    assert_eq!(unclaimed.status, 409);
    assert_eq!(unclaimed.error_code(), "claim_required");

    let detail = request(
        &server.addr,
        "GET",
        &format!("/items/{identifier}"),
        None,
        &[],
    );
    let version = detail.body["item"]["item"]["version"].as_i64().unwrap();
    let claimed = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({})),
        &[],
    );
    assert_eq!(claimed.status, 200);

    // A second claim from another session is a conflict.
    let conflict = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({})),
        &[("X-APM-Session", "someone-else")],
    );
    assert_eq!(conflict.status, 409);
    assert_eq!(conflict.error_code(), "claim_conflict");

    // Optimistic locking: the code and the details survive.
    let stale = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "raced", "expect_version": version + 99 })),
        &[],
    );
    assert_eq!(stale.status, 409);
    assert_eq!(stale.error_code(), "version_conflict");
    assert_eq!(
        stale.body["error"]["details"]["expected"],
        json!(version + 99)
    );

    // Idempotency: the same key twice returns the same version, no double write.
    let key = "rest-idem-1";
    let first = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "idempotent", "expect_version": version })),
        &[("Idempotency-Key", key)],
    );
    assert_eq!(first.status, 200, "{}", first.body);
    let replay = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "idempotent", "expect_version": version })),
        &[("Idempotency-Key", key)],
    );
    assert_eq!(replay.status, 200);
    assert_eq!(
        replay.body["item"]["version"],
        first.body["item"]["version"]
    );

    // A workflow violation stays a domain error (422), not a transport failure.
    let bogus = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/status"),
        Some(&json!({ "to": "shipped" })),
        &[],
    );
    assert_eq!(bogus.status, 422);
    assert_eq!(bogus.error_code(), "workflow_violation");

    // Malformed input is rejected with the CLI's invalid_input code.
    let bad = request(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "" })),
        &[],
    );
    assert_eq!(bad.status, 400);
    assert_eq!(bad.error_code(), "invalid_input");

    let unknown_route = request(&server.addr, "GET", "/nope", None, &[]);
    assert_eq!(unknown_route.status, 404);

    let wrong_method = request(&server.addr, "POST", "/dashboard", None, &[]);
    assert_eq!(wrong_method.status, 405);
    assert_eq!(wrong_method.error_code(), "method_not_allowed");
}

#[test]
fn list_filters_and_dashboard_query_parameters_are_honoured() {
    let ledger = Ledger::new();
    ledger.create("first");
    ledger.create("second");
    let server = Server::start(&ledger, &[]);

    let all = request(&server.addr, "GET", "/items?limit=10", None, &[]);
    assert_eq!(all.body["count"], json!(2));

    let limited = request(&server.addr, "GET", "/items?limit=1", None, &[]);
    assert_eq!(limited.body["count"], json!(1));

    let keyword = request(&server.addr, "GET", "/items?keywords=first", None, &[]);
    assert_eq!(keyword.body["count"], json!(1));

    let bad_limit = request(&server.addr, "GET", "/items?limit=abc", None, &[]);
    assert_eq!(bad_limit.status, 400);
    assert_eq!(bad_limit.error_code(), "invalid_input");

    let board = request(&server.addr, "GET", "/dashboard", None, &[]);
    assert_eq!(board.status, 200);
    assert_eq!(
        board.body["dashboard"]["buckets"]["ready_unclaimed"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn remote_binding_needs_an_explicit_flag() {
    let ledger = Ledger::new();
    let refused = Command::new(env!("CARGO_BIN_EXE_apm-serve"))
        .args([
            "--db",
            ledger.db.to_str().unwrap(),
            "--listen",
            "0.0.0.0:0",
            "--token",
            TOKEN,
        ])
        .output()
        .expect("spawn apm-serve");
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("--allow-remote"), "stderr: {stderr}");
    // Exit code 2 = invalid input, like the rest of the CLI.
    assert_eq!(refused.status.code(), Some(2));

    // With the flag it starts (and we stop it immediately).
    let mut allowed = Command::new(env!("CARGO_BIN_EXE_apm-serve"))
        .args([
            "--db",
            ledger.db.to_str().unwrap(),
            "--listen",
            "0.0.0.0:0",
            "--token",
            TOKEN,
            "--allow-remote",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn apm-serve");
    std::thread::sleep(Duration::from_millis(300));
    let _ = allowed.kill();
    let _ = allowed.wait();
}

#[test]
fn missing_database_is_a_clean_failure() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_apm-serve"))
        .current_dir(dir.path())
        .env_remove("APM_DB")
        .args(["--token", TOKEN])
        .output()
        .expect("spawn apm-serve");
    assert_eq!(output.status.code(), Some(3), "not_found exits 3");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("no .apm/apm.db"), "stderr: {stderr}");

    let explicit = dir.path().join("nope").join("apm.db");
    let output = Command::new(env!("CARGO_BIN_EXE_apm-serve"))
        .args([
            "--db",
            explicit.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--token",
            TOKEN,
        ])
        .output()
        .expect("spawn apm-serve");
    assert_eq!(output.status.code(), Some(3));
    assert!(!explicit.exists(), "apm-serve must not create a ledger");
}
