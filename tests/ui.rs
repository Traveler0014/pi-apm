//! End-to-end tests for `apm ui` — the embedded dashboard served by the REST
//! adapter.
//!
//! Every test spawns the real binary on an ephemeral loopback port and speaks
//! HTTP/1.1 over TCP, so routing, headers, asset lookup, auth and the one-time
//! bootstrap ticket are exercised as a browser (or a script) would hit them.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Ledger {
    dir: tempfile::TempDir,
    db: PathBuf,
}

impl Ledger {
    fn new() -> Ledger {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".apm").join("apm.db");
        let ledger = Ledger { dir, db };
        ledger.apm(&["init", "--name", "UI Test", "--prefix", "UIT", "--json"]);
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
        assert!(
            output.status.success(),
            "apm {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or(Value::Null)
    }
}

struct Ui {
    child: Child,
    addr: String,
    url: String,
    ticket: String,
    /// The real bearer token, only present after a successful exchange.
    token: Option<String>,
    startup: Vec<String>,
    stderr: Arc<Mutex<String>>,
}

impl Ui {
    /// Start with the default TTL and exchange the ticket immediately.
    fn start(ledger: &Ledger, extra: &[&str]) -> Ui {
        let mut ui = Ui::spawn(ledger, extra, &[], true);
        let token = ui.exchange(&ui.ticket.clone()).expect("ticket exchange");
        ui.token = Some(token);
        ui
    }

    fn spawn(ledger: &Ledger, extra: &[&str], env: &[(&str, &str)], no_open: bool) -> Ui {
        let mut args = vec!["ui", "--db", ledger.db.to_str().unwrap(), "--port", "0"];
        if no_open {
            args.push("--no-open");
        }
        args.extend_from_slice(extra);
        let mut command = Command::new(env!("CARGO_BIN_EXE_apm"));
        command
            .args(&args)
            .env_remove("APM_SESSION_ID")
            .env_remove("APM_RUN_ID");
        for (name, value) in env {
            command.env(name, value);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn apm ui");

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_pipe = child.stderr.take().expect("stderr");
        let stderr_sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let mut buffer = String::new();
            let _ = reader.read_to_string(&mut buffer);
            if let Ok(mut sink) = stderr_sink.lock() {
                sink.push_str(&buffer);
            }
        });

        let stdout = child.stdout.take().expect("stdout");
        let mut reader = BufReader::new(stdout);
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut startup = Vec::new();
        let mut addr = None;
        let mut url = None;
        while Instant::now() < deadline && (addr.is_none() || url.is_none()) {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                break;
            }
            let line = line.trim_end().to_string();
            startup.push(line.clone());
            if let Some(rest) = line.strip_prefix("listening on ") {
                addr = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("ui: ") {
                url = Some(rest.to_string());
            }
        }
        // Keep draining stdout so the child never blocks on a full pipe.
        std::thread::spawn(move || {
            let mut sink = String::new();
            let _ = reader.read_to_string(&mut sink);
        });
        let addr = addr.unwrap_or_else(|| panic!("apm ui did not report an address"));
        let url = url.unwrap_or_else(|| panic!("apm ui did not report a URL"));
        let ticket = url
            .rsplit_once("ticket=")
            .map(|(_, ticket)| ticket.to_string())
            .unwrap_or_else(|| panic!("ui URL has no fragment ticket: {url}"));
        Ui {
            child,
            addr,
            url,
            ticket,
            token: None,
            startup,
            stderr,
        }
    }

    fn token(&self) -> &str {
        self.token.as_deref().expect("no exchanged token")
    }

    /// POST /api/bootstrap with a JSON ticket body. Returns the body on 200.
    fn exchange(&self, ticket: &str) -> Result<String, Reply> {
        let body = json!({ "ticket": ticket }).to_string();
        let reply = self.request("POST", "/api/bootstrap", &[], Some(&body));
        if reply.status == 200 {
            let token = reply.json()["token"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            assert!(token.starts_with("apm_ui_"), "unexpected token: {token}");
            Ok(token)
        } else {
            Err(reply)
        }
    }

    fn get(&self, path: &str) -> Reply {
        self.request("GET", path, &[], None)
    }

    fn authed(&self, path: &str) -> Reply {
        let bearer = format!("Bearer {}", self.token());
        self.request("GET", path, &[("Authorization", &bearer)], None)
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Reply {
        http(&self.addr, method, path, headers, body)
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).unwrap();
    }
    stream.flush().unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    parse_reply(&raw)
}

#[derive(Debug)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn error_code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
}

/// The bundle's entry script, as referenced by the index document (the hash
/// changes on every frontend build, so never hard-code it).
fn first_js_asset(html: &str) -> String {
    for part in html.split('"') {
        let relative = part.strip_prefix("./").unwrap_or(part);
        if relative.starts_with("assets/") && relative.ends_with(".js") {
            return format!("/ui/{relative}");
        }
    }
    "/ui/assets/app.js".to_string()
}

fn parse_reply(raw: &[u8]) -> Reply {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in reply"));
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let body = raw[split + 4..].to_vec();
    let mut lines = head.lines();
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("bad status line: {status_line}"));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    Reply {
        status,
        headers,
        body,
    }
}

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'";

#[test]
fn ui_serves_embedded_assets_with_csp_and_without_secrets() {
    let ledger = Ledger::new();
    let ui = Ui::start(&ledger, &[]);

    let index = ui.get("/ui/");
    assert_eq!(index.status, 200, "index: {:?}", index.text());
    assert_eq!(
        index.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(index.header("cache-control"), Some("no-store"));
    assert_eq!(index.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(index.header("referrer-policy"), Some("no-referrer"));
    let csp = index.header("content-security-policy").expect("CSP header");
    assert!(csp.starts_with(CSP), "CSP drifted: {csp}");
    assert!(csp.contains("frame-ancestors 'none'"));
    assert!(csp.contains("object-src 'none'"));
    let html = index.text();
    assert!(!html.contains(ui.token()), "token leaked into HTML");
    assert!(!html.contains(&ui.ticket), "ticket leaked into HTML");
    assert!(!html.contains("localStorage"), "HTML must not talk storage");
    assert!(
        html.contains("/ui/assets/") || html.contains("./assets/"),
        "index must reference bundle assets: {html}"
    );

    let js = first_js_asset(&html);
    let asset = ui.get(&js);
    assert_eq!(asset.status, 200, "asset {js}");
    assert_eq!(
        asset.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(
        asset.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    // Unknown assets 404; extension-less paths fall back to the SPA document.
    assert_eq!(ui.get("/ui/assets/missing.js").status, 404);
    let spa = ui.get("/ui/items/UIT-1");
    assert_eq!(spa.status, 200);
    assert_eq!(spa.header("content-type"), Some("text/html; charset=utf-8"));

    // Canonical URLs.
    assert_eq!(ui.get("/ui").status, 301);
    assert_eq!(ui.get("/ui").header("location"), Some("/ui/"));
    assert_eq!(ui.get("/").status, 302);
    assert_eq!(ui.get("/").header("location"), Some("/ui/"));

    // Traversal never reaches the file system: there is no file system lookup.
    assert_eq!(ui.get("/ui/../Cargo.toml").status, 400);
    assert_eq!(ui.get("/ui/%2e%2e/Cargo.toml").status, 400);
    assert_eq!(ui.get("/ui/assets/..%2f..%2fCargo.toml").status, 400);

    // Only GET.
    let posted = ui.request("POST", "/ui/", &[], Some("{}"));
    assert_eq!(posted.status, 405);

    // Browsers always ask for /favicon.ico: answer without auth so the console
    // stays clean, but never require a token for it.
    let icon = ui.get("/favicon.ico");
    assert_eq!(icon.status, 204);
    assert_eq!(icon.header("content-length"), Some("0"));
    assert!(icon.body.is_empty());
}

#[test]
fn ui_api_alias_keeps_bearer_auth_and_rest_shapes() {
    let ledger = Ledger::new();
    let created = ledger.apm(&[
        "create",
        "--title",
        "dashboard item",
        "--goal",
        "see it in the dashboard",
        "--current-state",
        "created",
        "--json",
    ]);
    let identifier = created["item"]["identifier"]
        .as_str()
        .expect("identifier")
        .to_string();
    let ui = Ui::start(&ledger, &[]);

    // Health is the only unauthenticated endpoint, under both prefixes.
    assert_eq!(ui.get("/health").status, 200);
    let health = ui.get("/api/health");
    assert_eq!(health.status, 200);
    assert_eq!(health.json()["ok"], true);

    // Everything else needs the exchanged token as a bearer token.
    assert_eq!(ui.get("/api/dashboard").status, 401);
    assert_eq!(ui.get(&format!("/api/items/{identifier}")).status, 401);
    let dashboard = ui.authed("/api/dashboard");
    assert_eq!(dashboard.status, 200, "{:?}", dashboard.text());
    assert!(dashboard.json()["dashboard"].is_object());
    let items = ui.authed("/api/items");
    assert_eq!(items.status, 200);
    assert!(items.text().contains(&identifier));

    // The alias is 1:1: the unprefixed routes still answer the same way.
    let legacy = ui.authed("/dashboard");
    assert_eq!(legacy.status, 200);
    assert_eq!(
        legacy.json()["dashboard"]["totals"],
        dashboard.json()["dashboard"]["totals"]
    );

    assert_eq!(ui.get("/api").status, 401);
    assert_eq!(ui.authed("/api/nope").status, 404);
}

#[test]
fn ui_exchanges_a_one_time_ticket_for_the_token() {
    let ledger = Ledger::new();
    // Spawn without auto-exchange so the ticket is still fresh.
    let ui = Ui::spawn(&ledger, &[], &[], true);

    // The printed URL carries a single-use ticket, never the API token.
    assert!(ui.url.contains("/ui/#ticket=apm_boot_"));
    assert!(!ui.url.contains("apm_ui_"));
    assert!(ui.ticket.starts_with("apm_boot_"));

    // No ticket at all, and a wrong ticket, are rejected.
    let missing = ui.request("POST", "/api/bootstrap", &[], Some("{}"));
    assert_eq!(missing.status, 401);
    assert_eq!(missing.error_code(), "invalid_ticket");
    let wrong = ui.exchange("apm_boot_not-the-ticket").unwrap_err();
    assert_eq!(wrong.status, 401);
    assert_eq!(wrong.error_code(), "invalid_ticket");

    // The one-time ticket buys exactly one token.
    let token = ui.exchange(&ui.ticket).expect("first exchange");
    let body = ui.request(
        "POST",
        "/api/bootstrap",
        &[],
        Some(&json!({"ticket": ui.ticket}).to_string()),
    );
    assert_eq!(body.status, 409);
    assert_eq!(body.error_code(), "ticket_consumed");

    // The token works, and the ticket alone does not.
    let bearer = format!("Bearer {token}");
    let dashboard = ui.request("GET", "/api/dashboard", &[("Authorization", &bearer)], None);
    assert_eq!(dashboard.status, 200);
    let ticket_as_bearer = format!("Bearer {}", ui.ticket);
    let denied = ui.request(
        "GET",
        "/api/dashboard",
        &[("Authorization", &ticket_as_bearer)],
        None,
    );
    assert_eq!(denied.status, 401);

    // Bearer form of the exchange also works (script/curl convenience).
    let second = Ui::spawn(&ledger, &[], &[], true);
    let header = format!("Bearer {}", second.ticket);
    let via_header = second.request(
        "POST",
        "/api/bootstrap",
        &[("Authorization", &header)],
        None,
    );
    assert_eq!(via_header.status, 200);
    assert!(
        via_header.json()["token"]
            .as_str()
            .unwrap()
            .starts_with("apm_ui_")
    );
}

#[test]
fn ui_ticket_expires_and_restart_invalidates_ticket_and_token() {
    let ledger = Ledger::new();
    let expired = Ui::spawn(&ledger, &["--ticket-ttl", "0"], &[], true);
    let rejected = expired.exchange(&expired.ticket).unwrap_err();
    assert_eq!(rejected.status, 401);
    assert_eq!(rejected.error_code(), "ticket_expired");

    let first = Ui::start(&ledger, &[]);
    let old_token = first.token().to_string();
    let old_ticket = first.ticket.clone();
    drop(first);

    // A restarted process mints a new ticket and a new token; neither of the old
    // credentials authenticates anything.
    let second = Ui::start(&ledger, &[]);
    assert_ne!(second.token(), old_token);
    let old_ticket_reply = second.exchange(&old_ticket).unwrap_err();
    assert_eq!(old_ticket_reply.status, 401);
    assert_eq!(old_ticket_reply.error_code(), "invalid_ticket");
    let old_bearer = format!("Bearer {old_token}");
    let stale = second.request(
        "GET",
        "/api/dashboard",
        &[("Authorization", &old_bearer)],
        None,
    );
    assert_eq!(stale.status, 401);
}

#[test]
fn ui_never_prints_the_bearer_token() {
    let ledger = Ledger::new();
    let ui = Ui::start(&ledger, &[]);
    let token = ui.token().to_string();
    assert!(token.starts_with("apm_ui_"));

    // Startup output is the delivery channel for the ticket, never for the token.
    for line in &ui.startup {
        assert!(!line.contains(&token), "token leaked to stdout: {line}");
        assert!(
            !line.contains("apm_ui_"),
            "token prefix leaked to stdout: {line}"
        );
    }
    assert!(ui.startup.iter().any(|line| line.contains("#ticket=")));

    std::thread::sleep(Duration::from_millis(100));
    let stderr = ui.stderr.lock().unwrap().clone();
    assert!(!stderr.contains(&token), "token leaked to stderr: {stderr}");
    assert!(!stderr.contains("apm_ui_"), "token prefix leaked to stderr");
}

#[test]
fn ui_serves_recent_item_events_for_the_detail_drawer() {
    let ledger = Ledger::new();
    let created = ledger.apm(&[
        "create",
        "--title",
        "eventful item",
        "--goal",
        "show its history",
        "--current-state",
        "created",
        "--json",
    ]);
    let identifier = created["item"]["identifier"]
        .as_str()
        .expect("identifier")
        .to_string();
    // Produce a couple of audit entries (item_created is already there).
    ledger.apm(&["claim", &identifier, "--session", "ui-test", "--json"]);
    ledger.apm(&[
        "progress",
        &identifier,
        "--session",
        "ui-test",
        "--summary",
        "working on the drawer",
        "--json",
    ]);
    let ui = Ui::start(&ledger, &[]);

    assert_eq!(
        ui.get(&format!("/api/items/{identifier}/events")).status,
        401
    );

    let all = ui.authed(&format!("/api/items/{identifier}/events"));
    assert_eq!(all.status, 200, "{:?}", all.text());
    let body = all.json();
    assert_eq!(body["ok"], true);
    let events = body["events"].as_array().expect("events array");
    assert!(!events.is_empty());
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|event| event["kind"].as_str())
        .collect();
    assert!(kinds.contains(&"item_created"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"progress_recorded"), "kinds: {kinds:?}");
    assert!(events[0]["at"].is_string());
    assert!(events[0]["actor"].is_string());

    // Oldest first, and `limit` caps the window.
    let limited = ui.authed(&format!("/api/items/{identifier}/events?limit=1"));
    assert_eq!(limited.status, 200);
    let limited_events = limited.json()["events"].as_array().cloned().unwrap();
    assert_eq!(limited_events.len(), 1);
    assert_eq!(
        limited_events[0]["kind"], "progress_recorded",
        "limit keeps the newest entries"
    );

    // Unknown items are 404 and a non-numeric limit is a 400.
    assert_eq!(ui.authed("/api/items/UIT-999/events").status, 404);
    assert_eq!(
        ui.authed(&format!("/api/items/{identifier}/events?limit=abc"))
            .status,
        400
    );
}

#[test]
fn ui_token_is_process_local_and_never_persisted() {
    let ledger = Ledger::new();
    let ui = Ui::start(&ledger, &[]);
    assert!(ui.token().starts_with("apm_ui_"));
    assert!(ui.url.starts_with("http://127.0.0.1:"));
    assert!(ui.url.contains("/ui/#ticket="));
    // The address line a script parses carries no secret.
    assert!(!ui.addr.contains(ui.token()));
    assert!(!ui.addr.contains(&ui.ticket));

    // No token/ticket file anywhere under the project data directory.
    let entries: Vec<String> = std::fs::read_dir(ledger.dir().join(".apm"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !entries
            .iter()
            .any(|name| name.contains("token") || name.contains("ticket")),
        "apm ui must not persist credentials: {entries:?}"
    );
}

#[test]
fn ui_browser_launch_is_opt_out() {
    let ledger = Ledger::new();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("opened.txt");
    let opener = dir.path().join("fake-opener.sh");
    std::fs::write(
        &opener,
        format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > {}\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let opener = opener.to_string_lossy().to_string();

    // --no-open: the opener must never run, even when it is configured.
    let quiet = Ui::spawn(&ledger, &[], &[("APM_UI_OPEN_CMD", &opener)], true);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "--no-open still launched a browser");
    drop(quiet);
    assert!(!marker.exists());

    // Default: the opener receives exactly the printed URL (ticket only).
    let ui = Ui::spawn(&ledger, &[], &[("APM_UI_OPEN_CMD", &opener)], false);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut opened = String::new();
    while Instant::now() < deadline {
        if let Ok(value) = std::fs::read_to_string(&marker)
            && !value.is_empty()
        {
            opened = value;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(opened, ui.url, "opener did not receive the launch URL");
    assert!(opened.contains(&format!("#ticket={}", ui.ticket)));
    assert!(!opened.contains("apm_ui_"));
}

#[test]
fn ui_origin_must_match_scheme_and_authority_exactly() {
    let ledger = Ledger::new();
    let ui = Ui::start(&ledger, &[]);
    let http_same = format!("http://{}", ui.addr);
    let https_same = format!("https://{}", ui.addr);

    // Same origin (http + same authority) is allowed on the guarded page.
    let allowed = ui.request("GET", "/ui/", &[("Origin", &http_same)], None);
    assert_eq!(allowed.status, 200);

    // https on the same authority is a different origin.
    let secured = ui.request("GET", "/ui/", &[("Origin", &https_same)], None);
    assert_eq!(secured.status, 403);
    assert_eq!(secured.error_code(), "forbidden_origin");

    // Different port and different host are different origins too.
    let other_port = ui.request("GET", "/ui/", &[("Origin", "http://127.0.0.1:1")], None);
    assert_eq!(other_port.status, 403);
    let cross = ui.request("GET", "/ui/", &[("Origin", "http://evil.example")], None);
    assert_eq!(cross.status, 403);

    // The API enforces the same rule once a token is present.
    let bearer = format!("Bearer {}", ui.token());
    let api_cross = ui.request(
        "GET",
        "/api/dashboard",
        &[
            ("Authorization", &bearer),
            ("Origin", "https://evil.example"),
        ],
        None,
    );
    assert_eq!(api_cross.status, 403);
}

#[test]
fn apm_serve_does_not_expose_the_dashboard() {
    let ledger = Ledger::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_apm-serve"))
        .args([
            "--db",
            ledger.db.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--token",
            "apm_rest_test_token_ui_disabled",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn apm-serve");
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut addr = None;
    while Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(rest) = line.trim_end().strip_prefix("listening on ") {
            addr = Some(rest.to_string());
            break;
        }
    }
    std::thread::spawn(move || {
        let mut sink = String::new();
        let _ = reader.read_to_string(&mut sink);
    });
    let addr = addr.expect("apm-serve address");
    let reply = http(&addr, "GET", "/ui/", &[], None);
    assert_eq!(reply.status, 404, "{}", reply.text());
    let bootstrap = http(
        &addr,
        "POST",
        "/api/bootstrap",
        &[],
        Some(&json!({"ticket": "apm_boot_x"}).to_string()),
    );
    assert_eq!(bootstrap.status, 404);
    let _ = child.kill();
    let _ = child.wait();
}
