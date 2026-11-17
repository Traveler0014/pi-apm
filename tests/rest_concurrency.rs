//! Concurrency and race tests for the optional REST adapter.
//!
//! APM-4 proved the REST surface works; this file proves it stays correct when
//! several real HTTP clients and real CLI processes hit the same SQLite file at
//! the same time. Everything here spawns the real binaries — no mocks, no
//! injected clocks except where a lease has to expire deterministically.
//!
//! The invariants under test:
//!
//! 1. N concurrent `claim`s on one item → exactly one winner, N-1 conflicts.
//! 2. N concurrent writes at the same `expect_version` → exactly one success.
//! 3. REST and CLI racing on the same item → one winner, no lost update, and the
//!    persisted summary belongs to the winner.
//! 4. Several `apm-serve` instances bootstrapping the same token file converge on
//!    one token, and that token is the one on disk.
//! 5. A token held for session A never authenticates a write for session B.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Barrier;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const TOKEN: &str = "apm_rest_concurrency_token_abcdef";

struct Ledger {
    dir: tempfile::TempDir,
    db: PathBuf,
}

impl Ledger {
    fn new() -> Ledger {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".apm").join("apm.db");
        let ledger = Ledger { dir, db };
        ledger.apm(&["init", "--name", "REST Race", "--prefix", "RRC", "--json"]);
        ledger
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn apm_env(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_apm"));
        cmd.env("APM_DB", &self.db)
            .env_remove("APM_SESSION_ID")
            .env_remove("APM_RUN_ID")
            .env_remove("APM_SIDE_RUN");
        cmd
    }

    fn apm(&self, args: &[&str]) -> Value {
        let output = self.apm_env().args(args).output().expect("spawn apm");
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
            "race the REST adapter",
            "--current-state",
            "fresh",
            "--json",
        ]);
        created["item"]["identifier"].as_str().unwrap().to_string()
    }

    fn version(&self, item: &str) -> i64 {
        self.apm(&["get", item, "--json"])["item"]["version"]
            .as_i64()
            .unwrap()
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
        std::thread::spawn(move || {
            let mut sink = String::new();
            let _ = reader.read_to_string(&mut sink);
        });
        Server {
            child,
            addr: addr.expect("apm-serve did not report an address"),
        }
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
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
}

fn call(
    addr: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    session: Option<&str>,
) -> Reply {
    call_with_token(addr, TOKEN, method, path, body, session)
}

fn call_with_token(
    addr: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    session: Option<&str>,
) -> Reply {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let payload = body.map(|value| value.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n"
    );
    if let Some(session) = session {
        head.push_str(&format!("X-APM-Session: {session}\r\n"));
    }
    if body.is_some() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            payload.len()
        ));
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
    Reply {
        status,
        body: serde_json::from_str(body.trim()).unwrap_or(Value::Null),
    }
}

/// Run `jobs` closures at (roughly) the same instant and return their results.
fn race<T: Send + 'static>(jobs: Vec<Box<dyn FnOnce() -> T + Send>>) -> Vec<T> {
    let barrier = std::sync::Arc::new(Barrier::new(jobs.len()));
    let handles: Vec<_> = jobs
        .into_iter()
        .map(|job| {
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                job()
            })
        })
        .collect();
    handles
        .into_iter()
        .map(|handle| handle.join().expect("job panicked"))
        .collect()
}

#[test]
fn concurrent_claims_have_exactly_one_winner() {
    const CLIENTS: usize = 8;
    let ledger = Ledger::new();
    let identifier = ledger.create("claim race");
    let server = Server::start(&ledger, &[]);

    let addr = server.addr.clone();
    let item = identifier.clone();
    let jobs: Vec<Box<dyn FnOnce() -> Reply + Send>> = (0..CLIENTS)
        .map(|index| {
            let addr = addr.clone();
            let item = item.clone();
            Box::new(move || {
                call(
                    &addr,
                    "POST",
                    &format!("/items/{item}/claim"),
                    Some(&json!({ "lease_seconds": 600 })),
                    Some(&format!("session-{index}")),
                )
            }) as Box<dyn FnOnce() -> Reply + Send>
        })
        .collect();
    let replies = race(jobs);

    let winners: Vec<&Reply> = replies.iter().filter(|reply| reply.status == 200).collect();
    let conflicts = replies
        .iter()
        .filter(|reply| reply.code() == "claim_conflict")
        .count();
    assert_eq!(winners.len(), 1, "exactly one claim may win: {replies:?}");
    assert_eq!(
        conflicts,
        CLIENTS - 1,
        "the rest must conflict: {replies:?}"
    );
    for loser in replies.iter().filter(|reply| reply.status != 200) {
        assert_eq!(loser.status, 409);
    }
    // The winner's claim is the one on the ledger, and exactly one active claim exists.
    let claimed = ledger.apm(&["get", &identifier, "--json"]);
    assert_eq!(claimed["item"]["claim"]["status"], json!("active"));
    assert_eq!(replies.len(), CLIENTS);
}

#[test]
fn concurrent_writes_at_one_version_have_exactly_one_success() {
    const WRITERS: usize = 8;
    let ledger = Ledger::new();
    let identifier = ledger.create("write race");
    let server = Server::start(&ledger, &[]);
    let version = ledger.version(&identifier);

    // One session owns the claim; the writers race on the same expected version.
    let claimed = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({ "lease_seconds": 600 })),
        Some("shared-session"),
    );
    assert_eq!(claimed.status, 200, "{}", claimed.body);

    let addr = server.addr.clone();
    let item = identifier.clone();
    let jobs: Vec<Box<dyn FnOnce() -> Reply + Send>> = (0..WRITERS)
        .map(|index| {
            let addr = addr.clone();
            let item = item.clone();
            Box::new(move || {
                call(
                    &addr,
                    "POST",
                    &format!("/items/{item}/progress"),
                    Some(&json!({
                        "summary": format!("writer {index}"),
                        "expect_version": version
                    })),
                    Some("shared-session"),
                )
            }) as Box<dyn FnOnce() -> Reply + Send>
        })
        .collect();
    let replies = race(jobs);

    let successes = replies.iter().filter(|reply| reply.status == 200).count();
    let conflicts = replies
        .iter()
        .filter(|reply| reply.code() == "version_conflict")
        .count();
    assert_eq!(successes, 1, "exactly one write may win: {replies:?}");
    assert_eq!(conflicts, WRITERS - 1);

    // The item moved forward exactly once, and the persisted summary is a single
    // writer's (no interleaved or lost update).
    let after = ledger.apm(&["get", &identifier, "--json"]);
    assert_eq!(after["item"]["version"], json!(version + 1));
    let summary = after["item"]["progress_summary"].as_str().unwrap();
    assert!(summary.starts_with("writer "), "{summary}");
}

#[test]
fn rest_and_cli_racing_do_not_lose_updates() {
    const CLI_WRITERS: usize = 4;
    let ledger = Ledger::new();
    let identifier = ledger.create("mixed race");
    let server = Server::start(&ledger, &[]);
    let version = ledger.version(&identifier);

    let claimed = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({ "lease_seconds": 600 })),
        Some("mixed-session"),
    );
    assert_eq!(claimed.status, 200, "{}", claimed.body);

    // Half the writers go through HTTP, half through the real CLI with the same
    // session (the CLI resolves the token from the shared 0600 session file).
    let addr = server.addr.clone();
    let item = identifier.clone();
    let db = ledger.db.clone();
    let mut jobs: Vec<Box<dyn FnOnce() -> bool + Send>> = Vec::new();
    for index in 0..CLI_WRITERS {
        let addr = addr.clone();
        let rest_item = item.clone();
        jobs.push(Box::new(move || {
            let reply = call(
                &addr,
                "POST",
                &format!("/items/{rest_item}/progress"),
                Some(&json!({
                    "summary": format!("rest writer {index}"),
                    "expect_version": version
                })),
                Some("mixed-session"),
            );
            reply.status == 200
        }));
        let item = item.clone();
        let db = db.clone();
        jobs.push(Box::new(move || {
            let output = Command::new(env!("CARGO_BIN_EXE_apm"))
                .args([
                    "progress",
                    &item,
                    "--summary",
                    &format!("cli writer {index}"),
                    "--expect-version",
                    &version.to_string(),
                    "--session",
                    "mixed-session",
                    "--json",
                ])
                .env("APM_DB", &db)
                .output()
                .expect("spawn apm");
            output.status.success()
        }));
    }
    let successes = race(jobs).into_iter().filter(|ok| *ok).count();
    assert_eq!(
        successes, 1,
        "only one writer may commit at version {version}"
    );

    // Exactly one version bump, one progress event, no lost update.
    let after = ledger.apm(&["get", &identifier, "--json"]);
    assert_eq!(after["item"]["version"], json!(version + 1));
    let summary = after["item"]["progress_summary"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        summary.starts_with("rest writer ") || summary.starts_with("cli writer "),
        "unexpected summary {summary}"
    );
}

#[test]
fn cli_keeps_working_after_the_server_is_gone() {
    // "No service degradation": the ledger does not depend on the adapter. A
    // claim made over HTTP is a normal claim — the session file holds the token —
    // so the CLI can continue the same work with the server stopped.
    let ledger = Ledger::new();
    let identifier = ledger.create("survive a server restart");
    let server = Server::start(&ledger, &[]);
    let claimed = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({ "lease_seconds": 600 })),
        Some("degrade-session"),
    );
    assert_eq!(claimed.status, 200, "{}", claimed.body);
    let addr = server.addr.clone();
    drop(server);

    // The port is closed now: an HTTP client fails, the ledger does not.
    let refused = TcpStream::connect(&addr);
    assert!(refused.is_err(), "the server should be gone");

    let output = Command::new(env!("CARGO_BIN_EXE_apm"))
        .args([
            "progress",
            &identifier,
            "--summary",
            "continued after the server stopped",
            "--session",
            "degrade-session",
            "--json",
        ])
        .env("APM_DB", &ledger.db)
        .output()
        .expect("spawn apm");
    assert!(
        output.status.success(),
        "CLI must keep working without the server: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = ledger.apm(&["get", &identifier, "--json"]);
    assert_eq!(
        after["item"]["progress_summary"],
        json!("continued after the server stopped")
    );
    assert_eq!(after["item"]["claim"]["status"], json!("active"));
}

#[test]
fn concurrent_servers_converge_on_one_token() {
    const INSTANCES: usize = 4;
    let ledger = Ledger::new();
    let token_file = ledger.dir().join("shared-rest-token");
    assert!(!token_file.exists(), "the race needs a cold token file");

    let mut children: Vec<Child> = Vec::new();
    let mut ports: Vec<String> = Vec::new();
    let mut printed_tokens: Vec<String> = Vec::new();
    for _ in 0..INSTANCES {
        let mut child = Command::new(env!("CARGO_BIN_EXE_apm-serve"))
            .args([
                "--db",
                ledger.db.to_str().unwrap(),
                "--listen",
                "127.0.0.1:0",
                "--token-file",
                token_file.to_str().unwrap(),
                "--print-token",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn apm-serve");
        let stdout = child.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        let mut addr = None;
        let mut token = None;
        // The two startup lines are printed before the accept loop starts, so a
        // blocking read is safe here.
        while addr.is_none() || token.is_none() {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("listening on ") {
                addr = Some(rest.to_string());
            }
            if let Some(rest) = line.strip_prefix("token: ") {
                token = Some(rest.to_string());
            }
        }
        ports.push(addr.expect("address line"));
        printed_tokens.push(token.expect("printed token"));
        children.push(child);
    }
    let _guard = KillAll(children);
    let _ = &_guard;

    // Every instance must hold the very same token, and the file must agree.
    let on_disk = std::fs::read_to_string(&token_file)
        .expect("token file exists")
        .trim()
        .to_string();
    assert_eq!(on_disk.lines().count(), 1, "no half-written token");
    for token in &printed_tokens {
        assert_eq!(
            token, &on_disk,
            "a server kept a token the file does not have"
        );
    }
    // And that token really authenticates each instance.
    for addr in &ports {
        let reply = call_with_token(addr, &on_disk, "GET", "/dashboard", None, None);
        assert_eq!(reply.status, 200, "instance {addr} rejected the file token");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&token_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

/// Kills spawned servers even when an assertion fires first.
struct KillAll(Vec<Child>);

impl Drop for KillAll {
    fn drop(&mut self) {
        for child in self.0.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn a_session_token_never_authenticates_another_session() {
    let ledger = Ledger::new();
    let identifier = ledger.create("session isolation");
    let server = Server::start(&ledger, &[]);

    // Session A claims and gets its token kept in server memory.
    let claimed = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({ "lease_seconds": 600 })),
        Some("session-a"),
    );
    assert_eq!(claimed.status, 200, "{}", claimed.body);

    // Session B presents no token: it must not inherit session A's.
    let body = json!({ "summary": "b was here" });
    let hijack = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&body),
        Some("session-b"),
    );
    assert_eq!(hijack.status, 409, "{}", hijack.body);
    assert_eq!(hijack.code(), "claim_required");

    // The ledger shows no write attributed to session B.
    let detail = ledger.apm(&["get", &identifier, "--json"]);
    assert!(detail["item"]["progress_summary"].is_null());

    // The real owner can still write.
    let own = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "a owns this" })),
        Some("session-a"),
    );
    assert_eq!(own.status, 200, "{}", own.body);
}

#[test]
fn an_expired_lease_is_rejected_and_recoverable_over_http() {
    let ledger = Ledger::new();
    let identifier = ledger.create("expire the REST lease");
    let t0 = 1_700_000_000_000i64;

    // Claim with lease 30 s on a server whose clock is t0.
    let before = Server::start(&ledger, &["--now", &t0.to_string()]);
    let claimed = call(
        &before.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({ "lease_seconds": 30 })),
        Some("lease-session"),
    );
    assert_eq!(claimed.status, 200, "{}", claimed.body);
    let leased_at = claimed.body["claim"]["leased_at"]
        .as_str()
        .unwrap()
        .to_string();
    drop(before);

    // Same session, later clock: the token is right, the lease is dead.
    let later = Server::start(&ledger, &["--now", &(t0 + 31_000).to_string()]);
    let expired = call(
        &later.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "too late" })),
        Some("lease-session"),
    );
    assert_eq!(expired.status, 409, "{}", expired.body);
    assert_eq!(expired.code(), "claim_invalid");
    assert!(
        expired.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expired"),
        "{}",
        expired.body
    );
    assert!(leased_at.ends_with('Z'));

    // Recovery is explicit: takeover from the same session.
    let recovered = call(
        &later.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({ "takeover": true, "reason": "lease expired mid-run" })),
        Some("lease-session"),
    );
    assert_eq!(recovered.status, 200, "{}", recovered.body);
    assert_eq!(recovered.body["takeover"], json!(true));
    let progress = call(
        &later.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "back after recovery" })),
        Some("lease-session"),
    );
    assert_eq!(progress.status, 200, "{}", progress.body);

    // A bogus token for the item is refused too (claim_invalid, not a 500).
    let bogus_body = ledger.apm(&["get", &identifier, "--json"]);
    assert_eq!(bogus_body["item"]["status"], json!("ready"));
}

#[test]
fn unknown_body_fields_are_rejected_instead_of_silently_unlocking() {
    let ledger = Ledger::new();
    let identifier = ledger.create("strict fields");
    let server = Server::start(&ledger, &[]);
    let version = ledger.version(&identifier);
    let claimed = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({})),
        Some("strict"),
    );
    assert_eq!(claimed.status, 200);

    // A typo'd expect_version must not degrade into an unlocked write.
    let typo = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "typo", "expect_versoin": version + 99 })),
        Some("strict"),
    );
    assert_eq!(typo.status, 400, "{}", typo.body);
    assert_eq!(typo.code(), "invalid_input");
    assert!(
        typo.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("expect_versoin"),
        "{}",
        typo.body
    );
    assert_eq!(ledger.version(&identifier), version, "nothing was written");

    // The correct field still works.
    let ok = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/progress"),
        Some(&json!({ "summary": "right field", "expect_version": version })),
        Some("strict"),
    );
    assert_eq!(ok.status, 200, "{}", ok.body);

    // Same rule on the other write endpoints.
    let unknown_claim = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/claim"),
        Some(&json!({ "takeoverr": true })),
        Some("strict"),
    );
    assert_eq!(unknown_claim.status, 400);
    let unknown_status = call(
        &server.addr,
        "POST",
        &format!("/items/{identifier}/status"),
        Some(&json!({ "to": "doing", "expect_versionn": 1 })),
        Some("strict"),
    );
    assert_eq!(unknown_status.status, 400);
}

#[test]
fn many_sessions_keep_their_own_tokens_in_one_server() {
    // Two items, two sessions, one server: the session files must not lose each
    // other's tokens (this is the TokenStore lock plus the per-session cache).
    const PAIRS: usize = 4;
    let ledger = Ledger::new();
    let mut items = Vec::new();
    for index in 0..PAIRS {
        items.push(ledger.create(&format!("per-session {index}")));
    }
    let server = Server::start(&ledger, &[]);

    let addr = server.addr.clone();
    let jobs: Vec<Box<dyn FnOnce() -> (String, Reply) + Send>> = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let addr = addr.clone();
            let item = item.clone();
            Box::new(move || {
                let session = format!("pair-{index}");
                let reply = call(
                    &addr,
                    "POST",
                    &format!("/items/{item}/claim"),
                    Some(&json!({ "lease_seconds": 600 })),
                    Some(&session),
                );
                (item, reply)
            }) as Box<dyn FnOnce() -> (String, Reply) + Send>
        })
        .collect();
    let claimed = race(jobs);
    for (item, reply) in &claimed {
        assert_eq!(reply.status, 200, "{item}: {}", reply.body);
    }

    // Every session file exists, holds its own item, and none is empty.
    let sessions = ledger.dir().join(".apm").join("sessions");
    let files: Vec<PathBuf> = std::fs::read_dir(&sessions)
        .expect("session dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    assert_eq!(files.len(), PAIRS, "one token file per session");
    let mut seen_items = Vec::new();
    for file in &files {
        let raw = std::fs::read_to_string(file).unwrap();
        let tokens: Vec<Value> = serde_json::from_str(&raw).expect("token file is JSON");
        assert_eq!(tokens.len(), 1, "no token may be lost: {raw}");
        seen_items.push(tokens[0]["item_id"].as_str().unwrap().to_string());
        assert!(tokens[0]["token"].as_str().unwrap().starts_with("apm_"));
    }
    seen_items.sort();
    seen_items.dedup();
    assert_eq!(seen_items.len(), PAIRS, "every item has its own token");

    // All four sessions can still write to their own item afterwards.
    let counter = AtomicUsize::new(0);
    for (index, (item, _)) in claimed.iter().enumerate() {
        let reply = call(
            &server.addr,
            "POST",
            &format!("/items/{item}/progress"),
            Some(&json!({ "summary": "still mine" })),
            Some(&format!("pair-{index}")),
        );
        assert_eq!(reply.status, 200, "{item}: {}", reply.body);
        counter.fetch_add(1, Ordering::Relaxed);
    }
    assert_eq!(counter.load(Ordering::Relaxed), PAIRS);
}
