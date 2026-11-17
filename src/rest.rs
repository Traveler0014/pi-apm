//! Optional loopback REST adapter.
//!
//! The HTTP layer is a *thin* translation of the same `Store` calls the CLI and
//! the MCP adapter use, and it is never started implicitly: `apm-serve` runs in
//! the foreground until you stop it, and the CLI keeps working with the server
//! down. Nothing about the ledger's authority changes — SQLite is still the
//! single source of truth, and every invariant (claims, optimistic versions,
//! workflow guards, idempotency) is enforced by the store, not here.
//!
//! Design notes that matter for review:
//!
//! * **No framework.** The transport is deliberately a small HTTP/1.1 subset:
//!   one request per connection, `Content-Length` bodies only (no chunked, no
//!   keep-alive), 8 KiB header cap, 64 KiB body cap, 10 s read/write timeouts.
//!   Adding `axum`/`hyper` would pull an async runtime plus a large dependency
//!   tree into a binary whose whole job is "call Store, serialise JSON" on
//!   loopback. The parser is the part to replace first if the surface grows.
//! * **Loopback by default.** Binding a non-loopback address requires
//!   `--allow-remote` (and prints a warning): the ledger is local-first, and a
//!   REST endpoint on `0.0.0.0` is one firewall mistake away from being shared.
//! * **Bearer token on every non-health request.** The token is generated at
//!   startup, persisted to `<data_dir>/rest-token` with mode 0600 and printed
//!   only when you ask (`--print-token`). `Host` must name loopback and
//!   browser-origin requests are refused, which is what stops a random web page
//!   (or a DNS-rebinding trick) from driving your ledger.
//! * **Claim tokens never leave the process.** Same rule as the MCP adapter:
//!   the plaintext from `claim` goes to the 0600 session file (and process
//!   memory) and the response reports `token: null` / `token_source`.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::dashboard::DashboardOptions;
use crate::error::{Error, Result};
use crate::model::*;
use crate::session_store::{SessionToken, TokenStore};
use crate::store::Store;
use crate::store::claims::{ClaimPolicy, ClaimRequest};
use crate::store::items::ListFilter;

/// Default listen address: loopback, unprivileged port.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7717";
/// Hard cap on request headers; anything bigger is a client bug or an attack.
pub const MAX_HEADER_BYTES: usize = 8 * 1024;
/// Hard cap on a JSON body. Ledger payloads are small; progress summaries are
/// the largest thing a caller sends.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the browser has to exchange its startup ticket. A minute is enough
/// for a slow browser to cold-start, and short enough that a stale URL in a
/// terminal scrollback or CI log is useless.
pub const DEFAULT_BOOTSTRAP_TTL: Duration = Duration::from_secs(60);
const SESSION_HEADER: &str = "x-apm-session";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RestConfig {
    pub db: PathBuf,
    /// Default project when the database holds several.
    pub project: Option<String>,
    /// Actor name recorded in the audit trail.
    pub agent: String,
    /// `host:port` to bind; `port = 0` picks a free port (tests).
    pub listen: String,
    /// Required to bind anything other than loopback.
    pub allow_remote: bool,
    /// Session used when the client does not send `X-APM-Session`.
    pub session: Option<String>,
    pub run: Option<String>,
    /// Bearer token required on every non-`/health` request.
    pub token: String,
    pub read_timeout: Duration,
    /// Injected clock for tests/demos (same seam as the CLI's hidden `--now`):
    /// lets a lease expire deterministically instead of by sleeping.
    pub now: Option<i64>,
    /// Serve the embedded dashboard (`/ui`) and the `/api/*` aliases.
    /// `apm-serve` leaves it off; `apm ui` turns it on.
    pub ui: bool,
    /// One-time bootstrap ticket handed to the browser in the URL fragment.
    /// It is exchanged for [`RestConfig::token`] at `POST /api/bootstrap` and
    /// then burned; the real token never appears in a URL or a log line.
    pub bootstrap_ticket: Option<String>,
    /// Lifetime of the bootstrap ticket from server start.
    pub bootstrap_ttl: Duration,
}

impl RestConfig {
    pub fn new(db: impl Into<PathBuf>) -> RestConfig {
        RestConfig {
            db: db.into(),
            project: None,
            agent: "apm-rest".to_string(),
            listen: DEFAULT_LISTEN.to_string(),
            allow_remote: false,
            session: None,
            run: None,
            token: String::new(),
            read_timeout: DEFAULT_TIMEOUT,
            now: None,
            ui: false,
            bootstrap_ticket: None,
            bootstrap_ttl: DEFAULT_BOOTSTRAP_TTL,
        }
    }
}

/// Load the bearer token from `path`, or create one with mode 0600.
///
/// The token is local state, like the database itself: possession of the file is
/// the same trust boundary as possession of `.apm/apm.db`.
pub fn load_or_create_token(path: &Path) -> Result<String> {
    if let Some(token) = read_token(path)? {
        return Ok(token);
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Internal(format!("cannot create {}: {e}", parent.display())))?;
    }

    // Two servers starting at the same moment must converge on ONE token: the
    // one in the file. Write a private temp file, then publish it with
    // `hard_link`, which is atomic and fails with `AlreadyExists` instead of
    // overwriting. The winner's file appears complete (written and fsynced
    // before the link), and every loser reads the winner's token, so a running
    // server can never hold a token that the file does not describe.
    let token = format!("apm_rest_{}", uuid::Uuid::new_v4().simple());
    let tmp = path.with_file_name(format!(
        "rest-token.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let write = || -> Result<()> {
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| Error::Internal(format!("cannot create {}: {e}", tmp.display())))?;
        file.write_all(format!("{token}\n").as_bytes())
            .map_err(|e| Error::Internal(format!("cannot write {}: {e}", tmp.display())))?;
        file.sync_all()
            .map_err(|e| Error::Internal(format!("cannot sync {}: {e}", tmp.display())))?;
        restrict_mode(&tmp)
    };
    let result = write().and_then(|()| {
        std::fs::hard_link(&tmp, path).map_err(|e| {
            Error::Internal(format!("cannot publish {}", path.display()) + &format!(": {e}"))
        })
    });
    let _ = std::fs::remove_file(&tmp);
    match result {
        Ok(()) => Ok(token),
        Err(err) => {
            // Lost the race (the link target already exists) → use that token.
            if let Some(existing) = read_token(path)? {
                return Ok(existing);
            }
            // Filesystems without hard links: fall back to exclusive create, and
            // let readers retry an empty file instead of failing hard.
            match try_exclusive_create(path, &token) {
                Ok(()) => Ok(token),
                Err(Error::Internal(_)) => Err(err),
                Err(other) => Err(other),
            }
        }
    }
}

/// Read a token, treating a present-but-empty file as "not ready yet".
///
/// The empty window only exists on filesystems without hard links, where the
/// fallback creates the file and then writes it; a 2 s grace is far more than
/// the write needs and far less than a user's patience.
fn read_token(path: &Path) -> Result<Option<String>> {
    let mut waited = 0;
    loop {
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let token = raw.trim().to_string();
                if !token.is_empty() {
                    return Ok(Some(token));
                }
                if waited >= 200 {
                    return Ok(None);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(Error::Internal(format!(
                    "cannot read {}: {err}",
                    path.display()
                )));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
        waited += 1;
    }
}

/// Last-resort bootstrap for filesystems without hard links.
fn try_exclusive_create(path: &Path, token: &str) -> Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            file.write_all(format!("{token}\n").as_bytes())
                .map_err(|e| Error::Internal(format!("cannot write {}: {e}", path.display())))?;
            file.sync_all()
                .map_err(|e| Error::Internal(format!("cannot sync {}: {e}", path.display())))?;
            restrict_mode(path)?;
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(Error::Internal(format!(
            "cannot create {}: {err}",
            path.display()
        ))),
    }
}

#[cfg(unix)]
fn restrict_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| Error::Internal(format!("cannot chmod {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn restrict_mode(_path: &Path) -> Result<()> {
    Ok(())
}

/// Parse `host:port` and refuse to leave loopback without an explicit opt-in.
pub fn resolve_listen(raw: &str, allow_remote: bool) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = raw
        .to_socket_addrs()
        .map_err(|e| Error::invalid(format!("cannot resolve --listen {raw}: {e}")))?
        .collect();
    let addr = *addrs
        .first()
        .ok_or_else(|| Error::invalid(format!("--listen {raw} resolved to nothing")))?;
    let loopback = match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    };
    if !loopback && !allow_remote {
        return Err(Error::invalid(format!(
            "refusing to bind {addr}: the REST adapter is loopback-only by default \
             (pass --allow-remote to override, and read docs/rest.md first)"
        )));
    }
    Ok(addr)
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// A bound REST server. `serve()` blocks until the process is stopped.
pub struct RestServer {
    listener: TcpListener,
    state: Arc<State>,
}

struct State {
    store: Mutex<Store>,
    tokens: TokenStore,
    config: RestConfig,
    /// Process-local claim tokens, keyed by `(session, item id)`. Keying by
    /// session as well is what keeps one caller's token from authenticating
    /// another caller's write (the audit trail would silently blame the wrong
    /// session).
    held_tokens: Mutex<BTreeMap<(String, String), String>>,
    /// The one-time dashboard bootstrap ticket: handed out in the URL fragment,
    /// exchanged once for the bearer token, then dead.
    bootstrap: Mutex<Bootstrap>,
}

struct Bootstrap {
    ticket: String,
    expires_at: i64,
    consumed: bool,
}

impl RestServer {
    /// Bind the listener. Binding is separate from serving so a caller (or a
    /// test) can read the actual address, including an ephemeral port.
    pub fn bind(config: RestConfig) -> Result<RestServer> {
        if config.token.trim().is_empty() {
            return Err(Error::invalid(
                "a REST bearer token is required (see --token / --token-file)",
            ));
        }
        let addr = resolve_listen(&config.listen, config.allow_remote)?;
        // Never create a ledger implicitly: a REST client pointed at the wrong
        // path should fail loudly, not start writing into an empty database.
        if !config.db.exists() {
            return Err(Error::NotFound {
                entity: "database",
                id: config.db.display().to_string(),
            });
        }
        let store = Store::open(&config.db)?;
        let tokens = TokenStore::new(store.data_dir());
        let listener = TcpListener::bind(addr)
            .map_err(|e| Error::Internal(format!("cannot bind {addr}: {e}")))?;
        let expires_at =
            config.now.unwrap_or_else(now_ms) + config.bootstrap_ttl.as_millis() as i64;
        let bootstrap = Bootstrap {
            ticket: config.bootstrap_ticket.clone().unwrap_or_default(),
            expires_at,
            consumed: false,
        };
        Ok(RestServer {
            listener,
            state: Arc::new(State {
                store: Mutex::new(store),
                tokens,
                config,
                held_tokens: Mutex::new(BTreeMap::new()),
                bootstrap: Mutex::new(bootstrap),
            }),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.listener
            .local_addr()
            .map_err(|e| Error::Internal(format!("cannot read the bound address: {e}")))
    }

    /// Accept connections until the process is stopped.
    pub fn serve(&self) -> Result<()> {
        for incoming in self.listener.incoming() {
            let stream = match incoming {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!("apm-serve: accept failed: {err}");
                    continue;
                }
            };
            let state = Arc::clone(&self.state);
            std::thread::spawn(move || {
                if let Err(err) = handle_connection(stream, &state) {
                    eprintln!("apm-serve: connection error: {err}");
                }
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTTP/1.1 subset
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Cache policy for a response body.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cache {
    NoStore,
    /// Content-hashed bundle assets: safe to cache for a year.
    Immutable,
}

impl Response {
    fn ok(body: Value) -> Response {
        Response::json(200, body)
    }

    fn json(status: u16, body: Value) -> Response {
        let bytes = serde_json::to_vec_pretty(&body).unwrap_or_else(|_| b"{}".to_vec());
        Response::bytes(
            status,
            "application/json; charset=utf-8",
            Cache::NoStore,
            bytes,
        )
    }

    fn bytes(status: u16, mime: &str, cache: Cache, body: Vec<u8>) -> Response {
        let cache_header = match cache {
            Cache::NoStore => "no-store",
            Cache::Immutable => "public, max-age=31536000, immutable",
        };
        Response {
            status,
            headers: vec![
                ("Content-Type".to_string(), mime.to_string()),
                ("Cache-Control".to_string(), cache_header.to_string()),
                ("X-Content-Type-Options".to_string(), "nosniff".to_string()),
                ("Referrer-Policy".to_string(), "no-referrer".to_string()),
            ],
            body,
        }
    }

    /// A static (or redirect) response that carries the dashboard security
    /// headers. Only `/ui` responses need the CSP, but sending it everywhere is
    /// harmless and means a misrouted asset can never be rendered as a document.
    fn ui_bytes(status: u16, mime: &str, cache: Cache, body: Vec<u8>) -> Response {
        let mut response = Response::bytes(status, mime, cache, body);
        response.headers.push((
            "Content-Security-Policy".to_string(),
            crate::ui::CSP.to_string(),
        ));
        response.headers.push((
            "Cross-Origin-Resource-Policy".to_string(),
            "same-origin".to_string(),
        ));
        response.headers.push((
            "Cross-Origin-Opener-Policy".to_string(),
            "same-origin".to_string(),
        ));
        response
    }

    fn redirect(status: u16, location: &str) -> Response {
        let mut response = Response::ui_bytes(
            status,
            "text/plain; charset=utf-8",
            Cache::NoStore,
            Vec::new(),
        );
        response
            .headers
            .push(("Location".to_string(), location.to_string()));
        response
    }

    fn error(status: u16, code: &str, message: impl Into<String>) -> Response {
        Response::json(
            status,
            json!({
                "ok": false,
                "error": { "code": code, "message": message.into(), "status": status }
            }),
        )
    }

    fn domain(err: &Error) -> Response {
        Response::json(status_for(err), err.to_json())
    }
}

/// HTTP status for a domain error. The *code* stays the CLI's; the status is
/// just the transport-level hint, so a caller can branch on either.
pub fn status_for(err: &Error) -> u16 {
    match err {
        Error::InvalidInput(_) | Error::MissingContext(_) => 400,
        Error::IdempotencyConflict { .. } => 400,
        Error::NotFound { .. } => 404,
        Error::Forbidden(_) => 403,
        Error::VersionConflict { .. } => 409,
        Error::ClaimRequired { .. } | Error::ClaimInvalid { .. } | Error::ClaimConflict { .. } => {
            409
        }
        Error::WorkflowViolation(_) | Error::CompletionEvidenceRequired(_) => 422,
        Error::Dependency(_) => 422,
        Error::Db(_) | Error::Serde(_) | Error::Internal(_) => 500,
    }
}

fn handle_connection(mut stream: TcpStream, state: &State) -> Result<()> {
    stream
        .set_read_timeout(Some(state.config.read_timeout))
        .ok();
    stream
        .set_write_timeout(Some(state.config.read_timeout))
        .ok();

    let response = match read_request(&mut stream) {
        Ok(request) => route(&request, state),
        Err(RouteError::Http(response)) => response,
        Err(RouteError::Io(err)) => {
            return Err(Error::Internal(format!("cannot read the request: {err}")));
        }
    };
    write_response(&mut stream, response)
}

enum RouteError {
    Http(Response),
    Io(std::io::Error),
}

impl From<Response> for RouteError {
    fn from(response: Response) -> RouteError {
        RouteError::Http(response)
    }
}

fn read_request(stream: &mut TcpStream) -> std::result::Result<Request, RouteError> {
    let mut reader = BufReader::new(stream.try_clone().map_err(RouteError::Io)?);

    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(RouteError::Io)?;
    let request_line = request_line.trim_end();
    if request_line.is_empty() {
        return Err(Response::error(400, "bad_request", "empty request line").into());
    }
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let version = parts.next().unwrap_or_default();
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        return Err(Response::error(400, "bad_request", "malformed request line").into());
    }

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(RouteError::Io)?;
        header_bytes += line.len();
        if header_bytes > MAX_HEADER_BYTES {
            return Err(
                Response::error(431, "headers_too_large", "header section is too large").into(),
            );
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Response::error(400, "bad_request", "malformed header line").into());
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    if headers.contains_key("transfer-encoding") {
        return Err(Response::error(
            400,
            "bad_request",
            "Transfer-Encoding is not supported; send Content-Length",
        )
        .into());
    }
    let length = match headers.get("content-length") {
        Some(raw) => raw
            .parse::<usize>()
            .map_err(|_| Response::error(400, "bad_request", "invalid Content-Length"))?,
        None => 0,
    };
    if length > MAX_BODY_BYTES {
        return Err(Response::error(413, "payload_too_large", "request body is too large").into());
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).map_err(RouteError::Io)?;
    }

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.clone(), String::new()),
    };
    let mut query = BTreeMap::new();
    for pair in raw_query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (pair.to_string(), String::new()),
        };
        query.insert(percent_decode(&key), percent_decode(&value));
    }

    Ok(Request {
        method: method.to_ascii_uppercase(),
        path: percent_decode(&raw_path),
        query,
        headers,
        body,
    })
}

fn write_response(stream: &mut TcpStream, response: Response) -> Result<()> {
    let reason = reason_phrase(response.status);
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    ));
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(&response.body))
        .and_then(|()| stream.flush())
        .map_err(|e| Error::Internal(format!("cannot write the response: {e}")))
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        422 => "Unprocessable Entity",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

fn route(request: &Request, state: &State) -> Response {
    let segments: Vec<&str> = request
        .path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // Health is the only unauthenticated endpoint, and it says nothing about the
    // ledger: it exists so a supervisor can tell "up" from "down". `/api/health`
    // is the same thing under the UI's prefix.
    if matches!(segments.as_slice(), ["health"] | ["api", "health"]) && request.method == "GET" {
        return Response::ok(json!({
            "ok": true,
            "service": "apm-rest",
            "version": env!("CARGO_PKG_VERSION"),
        }));
    }

    // Browsers request a favicon for every page. Answer it without auth (it is
    // not ledger data) so the console stays clean; 204 means "no icon".
    if request.path == "/favicon.ico" && request.method == "GET" {
        return Response::bytes(204, "image/x-icon", Cache::NoStore, Vec::new());
    }

    // Local-access protection. Order matters: host/origin checks explain *why* a
    // request from a browser is refused before the token check hides it.
    if let Some(response) = guard_local(request) {
        return response;
    }

    // The dashboard: static assets embedded at compile time. No bearer token —
    // the page has to load before it can read the fragment token — but the same
    // Host/Origin guards and a strict CSP apply. Only enabled by `apm ui`.
    if segments.is_empty() || segments.first() == Some(&"ui") {
        return serve_ui(request, state, &segments);
    }

    // One-time exchange of the URL-fragment ticket for the bearer token. The
    // ticket is not a session: it works once, within its TTL, and only from a
    // same-origin caller (the guard above already ran).
    if segments.as_slice() == ["api", "bootstrap"] {
        return bootstrap(request, state);
    }
    let Some(auth) = request.headers.get("authorization") else {
        return Response::error(
            401,
            "unauthorized",
            "missing Authorization header (use: Authorization: Bearer <token>)",
        );
    };
    let Some(token) = auth.strip_prefix("Bearer ").map(str::trim) else {
        return Response::error(
            401,
            "unauthorized",
            "Authorization must be `Bearer <token>`",
        );
    };
    if !constant_time_eq(token.as_bytes(), state.config.token.as_bytes()) {
        return Response::error(401, "unauthorized", "invalid REST token");
    }

    let session = request
        .headers
        .get(SESSION_HEADER)
        .cloned()
        .or_else(|| state.config.session.clone())
        .unwrap_or_else(|| "rest".to_string());

    // `/api/*` is a 1:1 alias of the REST routes below: the frontend fetches
    // absolute `/api/items` paths so its own mount point (`/ui/`) never leaks
    // into the API layer.
    let api: &[&str] = if segments.first() == Some(&"api") {
        &segments[1..]
    } else {
        &segments[..]
    };
    if api.is_empty() {
        return Response::error(
            404,
            "not_found",
            format!("no route for {} {}", request.method, request.path),
        );
    }

    // Method check first, so a wrong verb on a real path is 405 (not 404) and a
    // genuinely unknown path is 404 regardless of the verb.
    let allowed: &[&str] = match api {
        ["dashboard"] | ["items"] | ["claims"] => &["GET"],
        ["items", _] => &["GET"],
        ["items", _, "events"] => &["GET"],
        ["items", _, "claim" | "release" | "progress" | "status"] => &["POST"],
        _ => {
            return Response::error(
                404,
                "not_found",
                format!("no route for {} {}", request.method, request.path),
            );
        }
    };
    if !allowed.contains(&request.method.as_str()) {
        return Response::error(
            405,
            "method_not_allowed",
            format!(
                "method {} is not allowed on {} (allowed: {})",
                request.method,
                request.path,
                allowed.join(", ")
            ),
        );
    }

    let outcome = match (request.method.as_str(), api) {
        ("GET", ["dashboard"]) => dashboard(request, state),
        ("GET", ["items"]) => list_items(request, state),
        ("GET", ["items", id]) => get_item(id, state),
        ("GET", ["items", id, "events"]) => item_events(id, request, state),
        ("GET", ["claims"]) => claims(request, state, &session),
        ("POST", ["items", id, "claim"]) => claim(id, request, state, &session),
        ("POST", ["items", id, "release"]) => release(id, request, state, &session),
        ("POST", ["items", id, "progress"]) => progress(id, request, state, &session),
        ("POST", ["items", id, "status"]) => transition(id, request, state, &session),
        // Unreachable: the guard above already rejected anything else.
        (method, _) => {
            return Response::error(
                405,
                "method_not_allowed",
                format!("method {method} is not allowed on {}", request.path),
            );
        }
    };
    match outcome {
        Ok(response) => response,
        Err(err) => Response::domain(&err),
    }
}

/// Host/Origin guards shared by the API and the dashboard.
///
/// Browsers omit `Origin` on same-origin GETs, so a read-only dashboard works
/// unchanged; a request that *does* carry one must come from this very server
/// (scheme `http` plus the same authority). That blocks cross-site requests and
/// DNS-rebinding pages while still allowing the UI's own future writes.
fn guard_local(request: &Request) -> Option<Response> {
    if let Some(origin) = request.headers.get("origin") {
        let host = request
            .headers
            .get("host")
            .map(String::as_str)
            .unwrap_or("");
        if !origin_matches_host(origin, host) {
            return Some(Response::error(
                403,
                "forbidden_origin",
                format!(
                    "origin {origin} is not this server ({host}); cross-origin browser                      requests are refused"
                ),
            ));
        }
    }
    if let Some(host) = request.headers.get("host")
        && !host_names_loopback(host)
    {
        return Some(Response::error(
            403,
            "forbidden_host",
            format!("Host {host} is not loopback; refusing to serve it"),
        ));
    }
    None
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let Some((scheme, authority)) = origin.split_once("://") else {
        return false;
    };
    let authority = authority.trim_end_matches('/');
    scheme.eq_ignore_ascii_case("http") && !host.is_empty() && authority.eq_ignore_ascii_case(host)
}

/// Serve the embedded dashboard: `GET /` redirects to `/ui/`, assets come from
/// the compile-time table, and extension-less paths fall back to the index so a
/// client-side route reload still works. The table lookup is by exact name, so
/// path traversal cannot escape: there is no file system access at all.
fn serve_ui(request: &Request, state: &State, segments: &[&str]) -> Response {
    if !state.config.ui {
        return Response::error(
            404,
            "not_found",
            format!(
                "no route for {} {} (start `apm ui` to serve the dashboard)",
                request.method, request.path
            ),
        );
    }
    if request.method != "GET" {
        return Response::error(
            405,
            "method_not_allowed",
            format!(
                "the dashboard is read-only; {} is not allowed",
                request.method
            ),
        );
    }
    if segments.is_empty() {
        return Response::redirect(302, "/ui/");
    }
    // Canonical trailing slash: relative URLs in the document resolve under
    // `/ui/`, not at the server root.
    if request.path == "/ui" {
        return Response::redirect(301, "/ui/");
    }
    let rest = &segments[1..];
    if rest.is_empty() {
        return index_response();
    }
    if rest.iter().any(|segment| {
        matches!(*segment, "." | "..") || segment.contains('\\') || segment.contains('\0')
    }) {
        return Response::error(400, "bad_path", "invalid path segment");
    }
    let joined = rest.join("/");
    if let Some(asset) = crate::ui::find(&joined) {
        let cache = if joined.starts_with("assets/") {
            Cache::Immutable
        } else {
            Cache::NoStore
        };
        return Response::ui_bytes(200, asset.mime, cache, asset.bytes.to_vec());
    }
    // Client-side route (no extension) → index; a missing asset → 404.
    if !rest.last().is_some_and(|last| last.contains('.')) {
        return index_response();
    }
    Response::error(404, "not_found", format!("no embedded asset {joined}"))
}

/// Exchange the one-time bootstrap ticket for the process-scoped bearer token.
///
/// The ticket arrives in the JSON body (`{"ticket":"…"}`) or, for scripts, as
/// `Authorization: Bearer <ticket>`. It is burned on success, so replaying a URL
/// from a log or a terminal scrollback cannot mint another token.
fn bootstrap(request: &Request, state: &State) -> Response {
    if !state.config.ui {
        return Response::error(
            404,
            "not_found",
            "the dashboard bootstrap endpoint requires `apm ui`",
        );
    }
    if request.method != "POST" {
        return Response::error(
            405,
            "method_not_allowed",
            format!(
                "{} is not allowed on /api/bootstrap (use POST)",
                request.method
            ),
        );
    }
    let from_body = serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|body| {
            body.get("ticket")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });
    let from_header = request
        .headers
        .get("authorization")
        .and_then(|raw| raw.strip_prefix("Bearer "))
        .map(|token| token.trim().to_string());
    let Some(ticket) = from_body.or(from_header) else {
        return Response::error(
            401,
            "invalid_ticket",
            "missing bootstrap ticket (send {\"ticket\": \"…\"})",
        );
    };

    let mut guard = match state.bootstrap.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.ticket.is_empty() || !constant_time_eq(ticket.as_bytes(), guard.ticket.as_bytes()) {
        return Response::error(401, "invalid_ticket", "unknown bootstrap ticket");
    }
    if guard.consumed {
        return Response::error(
            409,
            "ticket_consumed",
            "this bootstrap ticket was already exchanged; re-run `apm ui` for a new URL",
        );
    }
    if state.now() >= guard.expires_at {
        return Response::error(
            401,
            "ticket_expired",
            "this bootstrap ticket expired; re-run `apm ui` for a new URL",
        );
    }
    guard.consumed = true;
    Response::json(
        200,
        json!({
            "ok": true,
            "token": state.config.token,
            "token_type": "Bearer",
        }),
    )
}

fn index_response() -> Response {
    let index = crate::ui::index();
    Response::ui_bytes(200, index.mime, Cache::NoStore, index.bytes.to_vec())
}

fn host_names_loopback(host: &str) -> bool {
    let name = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
    let name = name.trim_start_matches('[').trim_end_matches(']');
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

/// Compare without an early exit, so a caller cannot learn the token prefix by
/// timing the rejection.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn actor_of(state: &State, session: &str) -> Actor {
    Actor::agent(state.config.agent.clone())
        .with_session(Some(session.to_string()))
        .with_run(state.config.run.clone())
}

impl State {
    fn now(&self) -> i64 {
        self.config.now.unwrap_or_else(now_ms)
    }
}

fn with_store<T>(state: &State, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
    let mut guard = state
        .store
        .lock()
        .map_err(|_| Error::Internal("store lock poisoned".into()))?;
    f(&mut guard)
}

fn dashboard(request: &Request, state: &State) -> Result<Response> {
    let project = request
        .query
        .get("project")
        .cloned()
        .or_else(|| state.config.project.clone());
    let opts = DashboardOptions {
        no_progress_after_seconds: int_param(&request.query, "no_progress_after_seconds")?,
        done_recent_seconds: int_param(&request.query, "done_recent_seconds")?,
    };
    let at = state.now();
    let board = with_store(state, |store| store.dashboard(project.as_deref(), at, opts))?;
    Ok(Response::ok(json!({ "ok": true, "dashboard": board })))
}

fn list_items(request: &Request, state: &State) -> Result<Response> {
    let statuses = match request.query.get("status") {
        Some(raw) => raw
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .map(Status::parse)
            .collect::<Result<Vec<_>>>()?,
        None => Vec::new(),
    };
    let filter = ListFilter {
        project: request
            .query
            .get("project")
            .cloned()
            .or_else(|| state.config.project.clone()),
        status: statuses,
        include_closed: bool_param(&request.query, "all")?.unwrap_or(false),
        label: request.query.get("label").cloned(),
        parent: request.query.get("parent").cloned(),
        session: request.query.get("session").cloned(),
        agent: request.query.get("agent").cloned(),
        keywords: request.query.get("keywords").cloned(),
        attention_only: bool_param(&request.query, "attention")?.unwrap_or(false),
        limit: int_param(&request.query, "limit")?.unwrap_or(50),
    };
    let at = state.now();
    let items = with_store(state, |store| store.list_items(filter, at))?;
    Ok(Response::ok(json!({
        "ok": true,
        "count": items.len(),
        "items": items,
    })))
}

fn get_item(id: &str, state: &State) -> Result<Response> {
    let at = state.now();
    let detail = with_store(state, |store| store.get_item(id, at))?;
    Ok(Response::ok(json!({ "ok": true, "item": detail })))
}

/// Recent audit entries for one item. Read-only, newest window last (the same
/// order `apm events <item>` prints), capped so a UI refresh cannot pull the
/// whole history.
fn item_events(id: &str, request: &Request, state: &State) -> Result<Response> {
    let limit = int_param(&request.query, "limit")?
        .unwrap_or(20)
        .clamp(1, 200);
    let events = with_store(state, |store| store.item_events(id, limit))?;
    Ok(Response::ok(json!({
        "ok": true,
        "count": events.len(),
        "events": events,
    })))
}

fn claims(request: &Request, state: &State, default_session: &str) -> Result<Response> {
    let session = request
        .query
        .get("session")
        .cloned()
        .unwrap_or_else(|| default_session.to_string());
    let include_expired = bool_param(&request.query, "include_expired")?.unwrap_or(false);
    let at = state.now();
    let claims = with_store(state, |store| {
        store.session_claims(&session, include_expired, at)
    })?;
    Ok(Response::ok(json!({
        "ok": true,
        "session": session,
        "count": claims.len(),
        "claims": claims,
        // Deliberately no token map: the REST surface never returns one.
        "tokens_suppressed": true,
    })))
}

fn claim(id: &str, request: &Request, state: &State, session: &str) -> Result<Response> {
    let body = json_body(request)?;
    check_fields(
        &body,
        &["lease_seconds", "takeover", "reason", "idempotency_key"],
    )?;
    let lease_seconds = body_i64(&body, "lease_seconds")?;
    let takeover = body_bool(&body, "takeover")?.unwrap_or(false);
    let reason = body_str(&body, "reason")?;
    let idempotency_key = idempotency_key(request, &body)?;
    let actor = actor_of(state, session);
    let request_payload = ClaimRequest {
        lease_seconds,
        takeover,
        reason,
        policy: ClaimPolicy::Normal,
    };
    let at = state.now();
    let issued = with_store(state, |store| {
        store.claim(id, &actor, request_payload, at, idempotency_key.as_deref())
    })?;

    // The plaintext token stops here (memory + 0600 session file).
    let token_source = persist_token(state, session, &issued)?;
    let item_version = with_store(state, |store| {
        store.get_item(&issued.claim.work_item_id, at)
    })?
    .item
    .version;
    Ok(Response::ok(json!({
        "ok": true,
        "claim": issued.claim,
        "token": Value::Null,
        "token_suppressed": true,
        "token_source": token_source,
        "replayed": issued.replayed,
        "takeover": issued.takeover,
        "item_version": item_version,
    })))
}

fn persist_token(state: &State, session: &str, issued: &ClaimIssued) -> Result<&'static str> {
    if issued.token.is_empty() {
        return Ok("unavailable");
    }
    state
        .held_tokens
        .lock()
        .map_err(|_| Error::Internal("token lock poisoned".into()))?
        .insert(
            (session.to_string(), issued.claim.work_item_id.clone()),
            issued.token.clone(),
        );
    let record = SessionToken {
        session_id: session.to_string(),
        claim_id: issued.claim.id.clone(),
        item_id: issued.claim.work_item_id.clone(),
        identifier: issued.claim.identifier.clone(),
        token: issued.token.clone(),
        expires_at: issued.claim.expires_at.clone(),
        saved_at: ts(state.now()),
    };
    match state.tokens.save(&record) {
        Ok(()) => Ok("session_file"),
        Err(err) => {
            eprintln!(
                "apm-serve: warning: could not persist the claim token to the session file ({err}); \
                 this process still holds it in memory"
            );
            Ok("memory_only")
        }
    }
}

fn release(id: &str, request: &Request, state: &State, session: &str) -> Result<Response> {
    let body = json_body(request)?;
    check_fields(&body, &["reason"])?;
    let reason = body_str(&body, "reason")?;
    let token = token_for(state, session, id)?.ok_or_else(|| Error::ClaimRequired {
        item: id.to_string(),
    })?;
    let actor = actor_of(state, session);
    let at = state.now();
    let claim = with_store(state, |store| {
        store.release(id, &token, reason.as_deref(), &actor, at, None)
    })?;
    if let Ok(mut held) = state.held_tokens.lock() {
        held.remove(&(session.to_string(), claim.work_item_id.clone()));
    }
    state.tokens.forget(session, &claim.work_item_id);
    Ok(Response::ok(json!({ "ok": true, "claim": claim })))
}

fn progress(id: &str, request: &Request, state: &State, session: &str) -> Result<Response> {
    let body = json_body(request)?;
    check_fields(
        &body,
        &[
            "summary",
            "next_action",
            "blocker",
            "waiting_on_human",
            "waiting_reason",
            "expect_version",
            "idempotency_key",
        ],
    )?;
    let summary = body_str(&body, "summary")?
        .ok_or_else(|| Error::invalid("`summary` is required (use sync for liveness only)"))?;
    let expect_version = body_i64(&body, "expect_version")?;
    let idempotency_key = idempotency_key(request, &body)?;
    let next_action = body_nullable_str(&body, "next_action")?;
    let blocker = body_nullable_str(&body, "blocker")?;
    let waiting_on_human = body_bool(&body, "waiting_on_human")?;
    let waiting_reason = body_nullable_str(&body, "waiting_reason")?;
    let token = token_for(state, session, id)?;
    let actor = actor_of(state, session);
    let at = state.now();
    let outcome = with_store(state, |store| {
        store.record_progress(
            id,
            token.as_deref(),
            expect_version,
            &summary,
            next_action,
            blocker,
            waiting_on_human,
            waiting_reason,
            &actor,
            at,
            idempotency_key.as_deref(),
        )
    })?;
    Ok(Response::ok(json!({
        "ok": true,
        "no_op": outcome.no_op,
        "substantive": outcome.substantive,
        "changed_fields": outcome.changed_fields,
        "item": outcome.item,
    })))
}

fn transition(id: &str, request: &Request, state: &State, session: &str) -> Result<Response> {
    let body = json_body(request)?;
    check_fields(
        &body,
        &["to", "reason", "note", "expect_version", "idempotency_key"],
    )?;
    let to = body_str(&body, "to")?
        .ok_or_else(|| Error::invalid("`to` is required (target state key)"))?;
    let status = Status::parse(&to)?;
    let reason = body_str(&body, "reason")?;
    let note = body_str(&body, "note")?;
    let expect_version = body_i64(&body, "expect_version")?;
    let idempotency_key = idempotency_key(request, &body)?;
    let token = token_for(state, session, id)?;
    let actor = actor_of(state, session);
    let at = state.now();
    let item = with_store(state, |store| {
        store.set_status(
            id,
            token.as_deref(),
            expect_version,
            status,
            reason.as_deref(),
            note.as_deref(),
            &actor,
            at,
            idempotency_key.as_deref(),
        )
    })?;
    Ok(Response::ok(json!({ "ok": true, "item": item })))
}

fn token_for(state: &State, session: &str, id: &str) -> Result<Option<String>> {
    let (item_id, identifier) = with_store(state, |store| store.item_ref(id))?;
    if let Some(token) = state
        .held_tokens
        .lock()
        .map_err(|_| Error::Internal("token lock poisoned".into()))?
        .get(&(session.to_string(), item_id.clone()))
    {
        return Ok(Some(token.clone()));
    }
    Ok(state
        .tokens
        .find(session, &item_id, &identifier)
        .map(|t| t.token))
}

// ---------------------------------------------------------------------------
// Body/query helpers
// ---------------------------------------------------------------------------

fn json_body(request: &Request) -> Result<Map<String, Value>> {
    if request.body.is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_slice(&request.body)
        .map_err(|e| Error::invalid(format!("request body is not valid JSON: {e}")))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(Error::invalid("request body must be a JSON object")),
    }
}

/// Reject fields the endpoint does not understand.
///
/// The MCP adapter is strict for the same reason and it matters more here: a
/// typo like `expect_versoin` would otherwise be dropped on the floor and the
/// write would silently lose its optimistic lock. `idempotency_key` is accepted
/// everywhere because the header form is documented per route.
fn check_fields(body: &Map<String, Value>, allowed: &[&str]) -> Result<()> {
    let unknown: Vec<&String> = body
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let unknown: Vec<&str> = unknown.iter().map(|key| key.as_str()).collect();
    Err(Error::invalid(format!(
        "unknown field(s) {}; allowed fields: {}",
        unknown.join(", "),
        allowed.join(", ")
    )))
}

fn idempotency_key(request: &Request, body: &Map<String, Value>) -> Result<Option<String>> {
    if let Some(raw) = request.headers.get("idempotency-key")
        && !raw.trim().is_empty()
    {
        return Ok(Some(raw.trim().to_string()));
    }
    body_str(body, "idempotency_key")
}

fn body_str(body: &Map<String, Value>, key: &str) -> Result<Option<String>> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` must be a string, got {other}"
        ))),
    }
}

/// `null` clears the field, a string sets it, absent keeps it.
fn body_nullable_str(body: &Map<String, Value>, key: &str) -> Result<Option<Option<String>>> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(text)) => Ok(Some(Some(text.clone()))),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` must be a string or null, got {other}"
        ))),
    }
}

fn body_i64(body: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_i64()
            .map(Some)
            .ok_or_else(|| Error::invalid(format!("`{key}` must be an integer"))),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` must be an integer, got {other}"
        ))),
    }
}

fn body_bool(body: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(other) => Err(Error::invalid(format!(
            "`{key}` must be a boolean, got {other}"
        ))),
    }
}

fn int_param(query: &BTreeMap<String, String>, key: &str) -> Result<Option<i64>> {
    match query.get(key) {
        None => Ok(None),
        Some(raw) => raw
            .parse::<i64>()
            .map(Some)
            .map_err(|_| Error::invalid(format!("`{key}` must be an integer"))),
    }
}

fn bool_param(query: &BTreeMap<String, String>, key: &str) -> Result<Option<bool>> {
    match query.get(key) {
        None => Ok(None),
        Some(raw) => match raw.as_str() {
            "1" | "true" | "yes" => Ok(Some(true)),
            "0" | "false" | "no" => Ok(Some(false)),
            other => Err(Error::invalid(format!(
                "`{key}` must be 1/0, true/false or yes/no (got {other})"
            ))),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_defaults_to_loopback_and_remote_needs_opt_in() {
        let loopback = resolve_listen("127.0.0.1:0", false).unwrap();
        assert!(loopback.ip().is_loopback());

        let refused = resolve_listen("0.0.0.0:0", false).unwrap_err();
        assert_eq!(refused.code(), "invalid_input");
        assert!(refused.to_string().contains("--allow-remote"));

        let allowed = resolve_listen("0.0.0.0:0", true).unwrap();
        assert!(!allowed.ip().is_loopback());
    }

    #[test]
    fn host_header_must_name_loopback() {
        assert!(host_names_loopback("127.0.0.1:7717"));
        assert!(host_names_loopback("localhost"));
        assert!(host_names_loopback("[::1]:7717"));
        assert!(!host_names_loopback("evil.example.com"));
        assert!(!host_names_loopback("192.168.1.5:7717"));
    }

    #[test]
    fn bearer_comparison_is_exact() {
        assert!(constant_time_eq(b"apm_rest_abc", b"apm_rest_abc"));
        assert!(!constant_time_eq(b"apm_rest_abc", b"apm_rest_abd"));
        assert!(!constant_time_eq(b"apm_rest_abc", b"apm_rest_abcd"));
    }

    #[test]
    fn percent_decoding_handles_queries_and_plus() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("APM%2D1"), "APM-1");
    }

    #[test]
    fn domain_errors_map_to_http_statuses_without_losing_codes() {
        assert_eq!(
            status_for(&Error::NotFound {
                entity: "item",
                id: "X".into()
            }),
            404
        );
        assert_eq!(
            status_for(&Error::VersionConflict {
                expected: 1,
                actual: 2
            }),
            409
        );
        assert_eq!(
            status_for(&Error::ClaimConflict {
                item: "X".into(),
                holder: "a".into(),
                expires_at: "t".into()
            }),
            409
        );
        assert_eq!(status_for(&Error::WorkflowViolation("nope".into())), 422);
        assert_eq!(status_for(&Error::invalid("bad")), 400);
        let response = Response::domain(&Error::invalid("bad"));
        assert_eq!(response.status, 400);
        let body: Value = serde_json::from_slice(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], json!("invalid_input"));
    }

    #[test]
    fn token_file_is_created_once_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rest-token");
        let first = load_or_create_token(&path).unwrap();
        assert!(first.starts_with("apm_rest_"));
        let second = load_or_create_token(&path).unwrap();
        assert_eq!(first, second, "the token must be stable for a given path");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn concurrent_bootstraps_converge_on_one_token() {
        // The regression this guards: a fixed temp path plus `rename` let two
        // starters each keep their own token while the file matched only one.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rest-token");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    load_or_create_token(&path).expect("bootstrap")
                })
            })
            .collect();
        let tokens: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = tokens[0].clone();
        assert!(first.starts_with("apm_rest_"));
        for token in &tokens {
            assert_eq!(token, &first, "every starter must share one token");
        }
        let on_disk = std::fs::read_to_string(&path).unwrap().trim().to_string();
        assert_eq!(on_disk, first);
        assert_eq!(on_disk.lines().count(), 1);
    }

    #[test]
    fn an_existing_token_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rest-token");
        std::fs::write(&path, "apm_rest_existing\n").unwrap();
        assert_eq!(load_or_create_token(&path).unwrap(), "apm_rest_existing");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "apm_rest_existing"
        );
        // No stray temp files left behind.
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name != "rest-token")
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn strict_field_check_lists_the_allowed_keys() {
        let body = serde_json::from_value::<Map<String, Value>>(json!({
            "summary": "x",
            "expect_versoin": 3,
        }))
        .unwrap();
        let err = check_fields(&body, &["summary", "expect_version"]).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("expect_versoin"), "{message}");
        assert!(
            message.contains("allowed fields: summary, expect_version"),
            "{message}"
        );

        let ok = serde_json::from_value::<Map<String, Value>>(json!({ "summary": "x" })).unwrap();
        check_fields(&ok, &["summary"]).unwrap();
    }

    #[test]
    fn binding_requires_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = RestConfig::new(dir.path().join("apm.db"));
        cfg.listen = "127.0.0.1:0".into();
        cfg.token = String::new();
        let err = RestServer::bind(cfg).err().expect("bind must fail");
        assert_eq!(err.code(), "invalid_input");
    }
}
