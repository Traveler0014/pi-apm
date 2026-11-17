//! MCP (Model Context Protocol) adapter over the ledger.
//!
//! The adapter is deliberately thin: every tool is a *translation* of one
//! `Store` call, so the invariants stay where they are documented
//! (`src/lib.rs`): claims, optimistic versions, workflow guards, idempotency and
//! the human/agent split are enforced by the same code the CLI uses. Nothing
//! here re-implements a state machine, a claim rule or a permission check.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout (MCP stdio). One
//! process serves one client session; stdout carries protocol frames only and
//! every diagnostic goes to stderr.
//!
//! Secrets: a claim token is a bearer secret that the database stores hashed.
//! `Store::claim` returns the plaintext exactly once. This adapter hands it
//! straight to the 0600 [`TokenStore`] (the same session file the CLI uses),
//! keeps a process-local copy so a failed disk write cannot strand a live lease,
//! and never puts it in a tool result, a log line or an environment variable.
//! Tool results additionally pass through [`redact_tokens`] as a last line of
//! defence, the same scrub the Pi extension applies.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::dashboard::DashboardOptions;
use crate::error::{Error, Result};
use crate::model::*;
use crate::session_store::{SessionToken, TokenStore};
use crate::store::Store;
use crate::store::claims::{ClaimPolicy, ClaimRequest};
use crate::store::items::RunOutcome;

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// Version advertised when the client asks for something we do not know.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol versions this server understands. `initialize` echoes the client's
/// version when it is one of these, which is what the MCP specification asks a
/// server to do.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

pub const SERVER_NAME: &str = "apm-mcp";

/// Shown to the client as `instructions`; a short form of
/// `docs/agent-protocol.md` so a fresh model does not invent the workflow.
const INSTRUCTIONS: &str = "\
AgentPM is a local-first work ledger. The order that keeps you honest:
1. `apm_dashboard` — read the board (ready/unclaimed, in progress, blocked, waiting on a human).
2. `apm_get` — read the item's handover context (goal/current_state/constraints/references),
   acceptance criteria, dependencies and current claim before touching anything.
3. `apm_claim` — take the lease, then `apm_status` to `doing`. On `claim_conflict`, stop
   and report who holds it; never work around a live claim.
4. While working: `apm_progress` records what concretely changed plus `next_action`
   (one call per meaningful unit of work). `apm_sync` is liveness only, never progress.
5. Finishing: evidence, then acceptance criteria met/waived, then `apm_status` to `done`.
   `done` is refused while acceptance criteria are open.
6. Handing off: `apm_context` (action=revise) with the current state, then `apm_release`.
Versions are optimistic: read the item, pass `expect_version`, and on
`version_conflict` re-read and retry once. Claim tokens are never returned by this
server: they live in a 0600 session file next to the database and are resolved
automatically for every write.";

// ---------------------------------------------------------------------------
// JSON-RPC
// ---------------------------------------------------------------------------

/// A protocol-level failure (bad envelope, unknown method, unknown tool).
///
/// Domain failures are *not* protocol failures: MCP wants them inside the tool
/// result (`isError: true`) so the calling model can read and recover from them.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> RpcError {
        RpcError {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn parse_error(message: impl Into<String>) -> RpcError {
        RpcError::new(-32700, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> RpcError {
        RpcError::new(-32600, message)
    }

    pub fn method_not_found(method: &str) -> RpcError {
        RpcError::new(-32601, format!("method not found: {method}"))
    }

    pub fn invalid_params(message: impl Into<String>) -> RpcError {
        RpcError::new(-32602, message)
    }

    pub fn internal(message: impl Into<String>) -> RpcError {
        RpcError::new(-32603, message)
    }

    fn into_response(self, id: Value) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": self.code,
                "message": redact_tokens(&self.message),
                "data": self.data,
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// What the binary needs to start a server.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub db: PathBuf,
    /// Default project for `apm_dashboard` when the database holds several.
    pub project: Option<String>,
    /// Actor name recorded in the audit trail.
    pub agent: String,
    /// Session id; one MCP server process is one session. Defaults to a fresh
    /// `mcp-<uuid>` so claims are scoped and never collide with a Pi session.
    pub session: Option<String>,
    pub run: Option<String>,
}

impl McpConfig {
    pub fn new(db: impl Into<PathBuf>) -> McpConfig {
        McpConfig {
            db: db.into(),
            project: None,
            agent: "apm-mcp".to_string(),
            session: None,
            run: None,
        }
    }
}

/// One MCP server bound to one database and one session.
pub struct McpServer {
    store: Store,
    tokens: TokenStore,
    session: String,
    agent: String,
    project: Option<String>,
    run: Option<String>,
    /// Process-local copy of claim tokens: the source of truth for this
    /// connection, so a failed session-file write still leaves the lease usable.
    held_tokens: BTreeMap<String, String>,
    negotiated: Option<Negotiated>,
    /// Injected clock for tests; `None` means the wall clock.
    at_override: Option<i64>,
}

/// What `initialize` settled on. Exposed so a host can log the version it is
/// actually speaking instead of guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// The version the client asked for, if it asked for one.
    pub requested: Option<String>,
    /// The version this server will speak for the rest of the connection.
    pub used: String,
}

impl McpServer {
    pub fn open(cfg: McpConfig) -> Result<McpServer> {
        let store = Store::open(&cfg.db)?;
        let tokens = TokenStore::new(store.data_dir());
        let session = cfg
            .session
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("mcp-{}", uuid::Uuid::new_v4().simple()));
        Ok(McpServer {
            store,
            tokens,
            session,
            agent: cfg.agent,
            project: cfg.project,
            run: cfg.run,
            held_tokens: BTreeMap::new(),
            negotiated: None,
            at_override: None,
        })
    }

    /// The session id this server writes to the audit trail.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Database this server serves (for startup diagnostics).
    pub fn db_path(&self) -> &std::path::Path {
        self.store.path()
    }

    /// The negotiated protocol version, once `initialize` has run.
    pub fn negotiated(&self) -> Option<&Negotiated> {
        self.negotiated.as_ref()
    }

    /// Deterministic clock for tests.
    pub fn with_clock(mut self, at: i64) -> McpServer {
        self.at_override = Some(at);
        self
    }

    fn now(&self) -> i64 {
        self.at_override.unwrap_or_else(now_ms)
    }

    fn actor(&self) -> Actor {
        Actor::agent(self.agent.clone())
            .with_session(Some(self.session.clone()))
            .with_run(self.run.clone())
    }

    // -- transport ----------------------------------------------------------

    /// Serve until stdin closes. One JSON-RPC message per line.
    pub fn serve_stdio(&mut self) -> Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        self.serve(stdin.lock(), stdout.lock())
    }

    /// Serve a stream. Used by the binary and by the end-to-end tests.
    pub fn serve<R: BufRead, W: Write>(&mut self, input: R, mut out: W) -> Result<()> {
        for line in input.lines() {
            let line = line.map_err(|e| Error::Internal(format!("cannot read stdin: {e}")))?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(response) = self.handle_line(&line) {
                writeln!(out, "{response}")
                    .and_then(|()| out.flush())
                    .map_err(|e| Error::Internal(format!("cannot write stdout: {e}")))?;
            }
        }
        Ok(())
    }

    /// Handle one raw line; `None` means "nothing to send back" (notification).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                let error = RpcError::parse_error(format!("invalid JSON: {err}"));
                return Some(error.into_response(Value::Null).to_string());
            }
        };
        self.handle_message(value).map(|v| v.to_string())
    }

    /// Handle one decoded message. `None` for notifications and client
    /// responses, which never get a reply.
    pub fn handle_message(&mut self, message: Value) -> Option<Value> {
        if message.is_array() {
            let error = RpcError::invalid_request(
                "JSON-RPC batches are not supported by this transport; send one message per line",
            );
            return Some(error.into_response(Value::Null));
        }
        let Some(object) = message.as_object() else {
            let error = RpcError::invalid_request("message must be a JSON object");
            return Some(error.into_response(Value::Null));
        };
        let id = object.get("id").cloned();
        let Some(method) = object.get("method").and_then(|m| m.as_str()) else {
            // A response from the client (we never send requests) or garbage.
            // Only bother with something that claims to be a request.
            let error = RpcError::invalid_request("missing `method`");
            return Some(error.into_response(id.unwrap_or(Value::Null)));
        };
        let method = method.to_string();
        let params = object.get("params").cloned().unwrap_or(Value::Null);

        let outcome = self.handle_request(&method, params);
        let Some(id) = id else {
            // Notification: never answered, even when it fails. Diagnostics are
            // for humans reading stderr, not for the client protocol stream.
            if let Err(err) = outcome {
                eprintln!("apm-mcp: notification {method} failed: {}", err.message);
            }
            return None;
        };
        Some(match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => error.into_response(id),
        })
    }

    fn handle_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcError> {
        // Notifications are fire-and-forget: acknowledge the known families so a
        // client's `initialized`/`cancelled` is not reported as a failure.
        if method.starts_with("notifications/") {
            return Ok(json!({}));
        }
        match method {
            "initialize" => self.initialize(params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_defs() })),
            "tools/call" => {
                let object = params
                    .as_object()
                    .ok_or_else(|| RpcError::invalid_params("params must be an object"))?;
                let name = object
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| RpcError::invalid_params("`name` is required"))?
                    .to_string();
                let arguments = object
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if !arguments.is_object() {
                    return Err(RpcError::invalid_params("`arguments` must be an object"));
                }
                Ok(self.dispatch(&name, arguments)?.into_json())
            }
            other => Err(RpcError::method_not_found(other)),
        }
    }

    fn initialize(&mut self, params: Value) -> std::result::Result<Value, RpcError> {
        let requested = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .map(|v| v.to_string());
        let used = match requested.as_deref() {
            Some(version) if SUPPORTED_PROTOCOL_VERSIONS.contains(&version) => version.to_string(),
            _ => PROTOCOL_VERSION.to_string(),
        };
        let client = params
            .get("clientInfo")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        eprintln!(
            "apm-mcp: initialize from {client} (requested {}, using {used}); session {}",
            requested.as_deref().unwrap_or("-"),
            self.session
        );
        let client_version = requested.clone();
        self.negotiated = Some(Negotiated {
            requested: client_version,
            used: used.clone(),
        });
        Ok(json!({
            "protocolVersion": used,
            "capabilities": {
                "tools": { "listChanged": false }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "title": "AgentPM",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": INSTRUCTIONS,
        }))
    }

    // -- token handling ----------------------------------------------------

    /// Current token for an item: process-local copy first, then the 0600
    /// session file (which is how a restarted server recovers its lease).
    fn token(&self, selector: &str) -> Result<Option<String>> {
        let (item_id, identifier) = self.store.item_ref(selector)?;
        if let Some(token) = self.held_tokens.get(&item_id) {
            return Ok(Some(token.clone()));
        }
        Ok(self
            .tokens
            .find(&self.session, &item_id, &identifier)
            .map(|t| t.token))
    }

    // -- tool dispatch -----------------------------------------------------

    fn dispatch(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> std::result::Result<ToolOutcome, RpcError> {
        let result = match name {
            "apm_dashboard" => self.tool_dashboard(arguments),
            "apm_get" => self.tool_get(arguments),
            "apm_claim" => self.tool_claim(arguments),
            "apm_progress" => self.tool_progress(arguments),
            "apm_status" => self.tool_status(arguments),
            "apm_context" => self.tool_context(arguments),
            "apm_release" => self.tool_release(arguments),
            "apm_sync" => self.tool_sync(arguments),
            other => {
                return Err(RpcError::invalid_params(format!(
                    "unknown tool '{other}'; call tools/list for the supported set"
                )));
            }
        };
        Ok(result.unwrap_or_else(|err| ToolOutcome::failure(&err)))
    }

    // -- tools -------------------------------------------------------------

    fn tool_dashboard(&mut self, arguments: Value) -> Result<ToolOutcome> {
        let mut args = Args::new("apm_dashboard", arguments)?;
        let project = args
            .optional_str("project")?
            .or_else(|| self.project.clone());
        let no_progress = args.optional_i64("no_progress_after_seconds")?;
        let done_recent = args.optional_i64("done_recent_seconds")?;
        args.finish()?;

        let opts = DashboardOptions {
            no_progress_after_seconds: no_progress,
            done_recent_seconds: done_recent,
        };
        let dashboard = self.store.dashboard(project.as_deref(), self.now(), opts)?;
        let text = render_board(&dashboard)?;
        Ok(ToolOutcome::success(&dashboard, text))
    }

    fn tool_get(&mut self, arguments: Value) -> Result<ToolOutcome> {
        let mut args = Args::new("apm_get", arguments)?;
        let item = args.required_str("item")?;
        args.finish()?;

        let detail = self.store.get_item(&item, self.now())?;
        let text = render_detail(&detail);
        Ok(ToolOutcome::success(&detail, text))
    }

    fn tool_claim(&mut self, arguments: Value) -> Result<ToolOutcome> {
        let mut args = Args::new("apm_claim", arguments)?;
        let item = args.required_str("item")?;
        let lease_seconds = args.optional_i64("lease_seconds")?;
        let takeover = args.optional_bool("takeover")?.unwrap_or(false);
        let reason = args.optional_str("reason")?;
        let idempotency_key = args.optional_str("idempotency_key")?;
        args.finish()?;

        let actor = self.actor();
        let request = ClaimRequest {
            lease_seconds,
            takeover,
            reason,
            policy: ClaimPolicy::Normal,
        };
        let issued = self.store.claim(
            &item,
            &actor,
            request,
            self.now(),
            idempotency_key.as_deref(),
        )?;

        // The plaintext token stops here: process memory + the 0600 session file.
        let token_source = self.persist_token(&issued);
        // The item version is what the next write must echo back as expect_version.
        let item_version = self
            .store
            .get_item(&issued.claim.work_item_id, self.now())?
            .item
            .version;

        let text = format!(
            "{} {} (lease {}s, expires {}){}\n  token: never returned over MCP (stored in {})\n  version: {}",
            if issued.takeover {
                "took over"
            } else {
                "claimed"
            },
            issued.claim.identifier,
            issued.claim.lease_seconds,
            issued.claim.expires_at,
            if issued.replayed { " [replayed]" } else { "" },
            token_source,
            item_version,
        );
        let structured = json!({
            "claim": issued.claim,
            "token": Value::Null,
            "token_suppressed": true,
            "token_source": token_source,
            "replayed": issued.replayed,
            "takeover": issued.takeover,
            "item_version": item_version,
        });
        Ok(ToolOutcome {
            content: vec![text_content(text)],
            structured: Some(structured),
            is_error: false,
        })
    }

    /// Store the token for later writes and report where it went — never the
    /// token itself.
    fn persist_token(&mut self, issued: &ClaimIssued) -> &'static str {
        if issued.token.is_empty() {
            // Idempotent replay: the database never kept the plaintext, so the
            // only possible source is a copy this session already holds.
            let known = self.held_tokens.contains_key(&issued.claim.work_item_id)
                || self
                    .tokens
                    .find(
                        &self.session,
                        &issued.claim.work_item_id,
                        &issued.claim.identifier,
                    )
                    .is_some();
            return if known { "session_file" } else { "unavailable" };
        }
        self.held_tokens
            .insert(issued.claim.work_item_id.clone(), issued.token.clone());
        let record = SessionToken {
            session_id: self.session.clone(),
            claim_id: issued.claim.id.clone(),
            item_id: issued.claim.work_item_id.clone(),
            identifier: issued.claim.identifier.clone(),
            token: issued.token.clone(),
            expires_at: issued.claim.expires_at.clone(),
            saved_at: ts(self.now()),
        };
        match self.tokens.save(&record) {
            Ok(()) => "session_file",
            Err(err) => {
                // Never fatal: the lease is live and this process can use it.
                // Do not echo the error text blindly — it is a path, not a secret.
                eprintln!(
                    "apm-mcp: warning: could not persist the claim token to the session file ({err}); \
                     this process still holds it in memory"
                );
                "memory_only"
            }
        }
    }

    fn tool_progress(&mut self, arguments: Value) -> Result<ToolOutcome> {
        let mut args = Args::new("apm_progress", arguments)?;
        let item = args.required_str("item")?;
        let summary = args.required_str("summary")?;
        let next_action = args.optional_nullable_str("next_action")?;
        let blocker = args.optional_nullable_str("blocker")?;
        let waiting_on_human = args.optional_bool("waiting_on_human")?;
        let waiting_reason = args.optional_nullable_str("waiting_reason")?;
        let expect_version = args.optional_i64("expect_version")?;
        let idempotency_key = args.optional_str("idempotency_key")?;
        args.finish()?;

        let token = self.token(&item)?;
        let actor = self.actor();
        let outcome = self.store.record_progress(
            &item,
            token.as_deref(),
            expect_version,
            &summary,
            next_action,
            blocker,
            waiting_on_human,
            waiting_reason,
            &actor,
            self.now(),
            idempotency_key.as_deref(),
        )?;

        let text = if outcome.no_op {
            format!(
                "{} no substantive change: the request matches the current state, nothing written\n  \
                 (use apm_sync for liveness; version stays {})",
                outcome.item.identifier, outcome.item.version
            )
        } else {
            format!(
                "{} progress recorded ({}) (version {})\n  {}\n  next: {}",
                outcome.item.identifier,
                if outcome.substantive {
                    "substantive"
                } else {
                    "bookkeeping only"
                },
                outcome.item.version,
                outcome.item.progress_summary.clone().unwrap_or_default(),
                outcome
                    .item
                    .next_action
                    .clone()
                    .unwrap_or_else(|| "-".into())
            )
        };
        Ok(ToolOutcome {
            content: vec![text_content(text)],
            structured: Some(json!(outcome)),
            is_error: false,
        })
    }

    fn tool_status(&mut self, arguments: Value) -> Result<ToolOutcome> {
        let mut args = Args::new("apm_status", arguments)?;
        let item = args.required_str("item")?;
        let to = args.required_str("to")?;
        let reason = args.optional_str("reason")?;
        let note = args.optional_str("note")?;
        let expect_version = args.optional_i64("expect_version")?;
        let idempotency_key = args.optional_str("idempotency_key")?;
        args.finish()?;

        // Parsed here (not in the adapter) so the vocabulary stays the domain's.
        let status = Status::parse(&to)?;
        let token = self.token(&item)?;
        let actor = self.actor();
        let updated = self.store.set_status(
            &item,
            token.as_deref(),
            expect_version,
            status,
            reason.as_deref(),
            note.as_deref(),
            &actor,
            self.now(),
            idempotency_key.as_deref(),
        )?;
        let text = format!(
            "{} → {} (version {})",
            updated.identifier,
            updated.status.as_str(),
            updated.version
        );
        Ok(ToolOutcome {
            content: vec![text_content(text)],
            structured: Some(json!({ "item": updated })),
            is_error: false,
        })
    }

    fn tool_context(&mut self, arguments: Value) -> Result<ToolOutcome> {
        let mut args = Args::new("apm_context", arguments)?;
        let item = args.required_str("item")?;
        let action = args.required_str("action")?;
        let version = args.optional_i64("version")?;
        let reason = args.optional_str("reason")?;
        let goal = args.optional_str("goal")?;
        let current_state = args.optional_str("current_state")?;
        let constraints = args.optional_str_list("constraints")?;
        let references = args.optional_str_list("references")?;
        let notes = args.optional_nullable_str("notes")?;
        let expect_version = args.optional_i64("expect_version")?;
        let idempotency_key = args.optional_str("idempotency_key")?;
        args.finish()?;

        match action.as_str() {
            "show" => {
                let (_, identifier) = self.store.item_ref(&item)?;
                let context = self.store.context_at(&item, version)?;
                let text = render_context(&identifier, &context);
                Ok(ToolOutcome {
                    content: vec![text_content(text)],
                    structured: Some(json!({ "context": context })),
                    is_error: false,
                })
            }
            "revise" => {
                let reason = reason.ok_or_else(|| {
                    Error::invalid("action=revise requires `reason` (what changed and why)")
                })?;
                // Partial revisions merge with the current document, exactly like
                // the Pi extension: a missing field keeps its current value.
                let current = self.store.current_context(&item)?;
                let doc = ContextDoc {
                    goal: goal.unwrap_or(current.doc.goal),
                    current_state: current_state.unwrap_or(current.doc.current_state),
                    constraints: constraints.unwrap_or(current.doc.constraints),
                    references: references.unwrap_or(current.doc.references),
                    notes: match notes {
                        Some(notes) => notes,
                        None => current.doc.notes,
                    },
                };
                let token = self.token(&item)?;
                let actor = self.actor();
                let revision = self.store.revise_context(
                    &item,
                    token.as_deref(),
                    expect_version,
                    Some(current.version),
                    doc,
                    &reason,
                    &actor,
                    self.now(),
                    idempotency_key.as_deref(),
                )?;
                let text = format!(
                    "{} context → v{} (item version {})\n  changed: {}\n  reason: {}",
                    revision.item.identifier,
                    revision.context.version,
                    revision.item.version,
                    if revision.changed_fields.is_empty() {
                        "-".to_string()
                    } else {
                        revision.changed_fields.join(", ")
                    },
                    revision.context.reason
                );
                Ok(ToolOutcome {
                    content: vec![text_content(text)],
                    structured: Some(json!(revision)),
                    is_error: false,
                })
            }
            other => Err(Error::invalid(format!(
                "unknown action '{other}' (expected show or revise)"
            ))),
        }
    }

    fn tool_release(&mut self, arguments: Value) -> Result<ToolOutcome> {
        let mut args = Args::new("apm_release", arguments)?;
        let item = args.required_str("item")?;
        let reason = args.optional_str("reason")?;
        args.finish()?;

        let token = self
            .token(&item)?
            .ok_or_else(|| Error::ClaimRequired { item: item.clone() })?;
        let actor = self.actor();
        let claim =
            self.store
                .release(&item, &token, reason.as_deref(), &actor, self.now(), None)?;
        self.held_tokens.remove(&claim.work_item_id);
        self.tokens.forget(&self.session, &claim.work_item_id);
        let text = format!(
            "released {} (was held by {})",
            claim.identifier, claim.holder
        );
        Ok(ToolOutcome {
            content: vec![text_content(text)],
            structured: Some(json!({ "claim": claim })),
            is_error: false,
        })
    }

    fn tool_sync(&mut self, arguments: Value) -> Result<ToolOutcome> {
        let mut args = Args::new("apm_sync", arguments)?;
        let item = args.required_str("item")?;
        let outcome_raw = args
            .optional_str("outcome")?
            .unwrap_or_else(|| "completed".to_string());
        let note = args.optional_str("note")?;
        let idempotency_key = args.optional_str("idempotency_key")?;
        args.finish()?;

        let outcome = RunOutcome::parse(&outcome_raw)?;
        let token = self.token(&item)?;
        let actor = self.actor();
        let sync = self.store.sync_run(
            &item,
            token.as_deref(),
            outcome,
            note.as_deref(),
            &actor,
            self.now(),
            idempotency_key.as_deref(),
        )?;
        let text = format!(
            "{} run sync: {} ({}); last_progress_at unchanged",
            sync.identifier, sync.run.outcome, sync.run.at
        );
        Ok(ToolOutcome {
            content: vec![text_content(text)],
            structured: Some(json!(sync)),
            is_error: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Tool results
// ---------------------------------------------------------------------------

/// One `tools/call` result.
#[derive(Debug, Clone)]
pub struct ToolOutcome {
    content: Vec<Value>,
    structured: Option<Value>,
    is_error: bool,
}

impl ToolOutcome {
    pub fn success<T: serde::Serialize>(structured: &T, text: impl Into<String>) -> ToolOutcome {
        ToolOutcome {
            content: vec![text_content(text)],
            structured: Some(serde_json::to_value(structured).unwrap_or(Value::Null)),
            is_error: false,
        }
    }

    /// A domain error, shaped like the CLI's `--json` envelope plus a recovery
    /// hint. Reported with `isError: true` so the model can act on it.
    pub fn failure(err: &Error) -> ToolOutcome {
        let code = err.code();
        let hint = hint_for(code);
        let mut payload = err.to_json();
        if let Some(error) = payload.get_mut("error").and_then(|e| e.as_object_mut()) {
            error.insert("hint".into(), json!(hint));
            error.insert("tool_surface".into(), json!("mcp"));
        }
        let text = format!(
            "apm error [{code}]: {}\n  hint: {hint}",
            err.to_string().trim_end()
        );
        ToolOutcome {
            content: vec![text_content(text)],
            structured: Some(payload),
            is_error: true,
        }
    }

    pub fn is_error(&self) -> bool {
        self.is_error
    }

    fn into_json(self) -> Value {
        let mut value = json!({ "content": self.content });
        if let Some(structured) = self.structured {
            value["structuredContent"] = scrub_value(structured);
        }
        if self.is_error {
            value["isError"] = json!(true);
        }
        value
    }
}

fn text_content(text: impl Into<String>) -> Value {
    let text: String = text.into();
    json!({ "type": "text", "text": redact_tokens(&text) })
}

/// Recovery advice per error code — the same guidance the Pi extension gives,
/// kept short enough to fit in a tool result.
fn hint_for(code: &str) -> &'static str {
    match code {
        "claim_required" => "claim the item first with apm_claim",
        "claim_invalid" => {
            "the lease is expired or the token is stale: call apm_claim with takeover=true, or stop and report"
        }
        "claim_conflict" => "another agent holds a live claim: do not write; report who holds it",
        "version_conflict" => {
            "re-read the item (apm_get) and retry once with the current version as expect_version"
        }
        "workflow_violation" => {
            "the transition is not allowed by the project workflow: check the legal states before retrying"
        }
        "completion_evidence_required" => {
            "mark every acceptance criterion met/waived, or add verification evidence, before done"
        }
        "not_found" => "check the identifier with apm_dashboard",
        "idempotency_conflict" => {
            "that idempotency_key was used for a different request: use a new key per operation"
        }
        "invalid_input" | "missing_context" => {
            "fix the arguments (see tools/list) and retry; nothing was written"
        }
        _ => "inspect the ledger with apm_dashboard and report the error if it persists",
    }
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Strict argument reader: unknown keys are rejected instead of silently
/// ignored, because a mistyped `expect_version` is a lost race waiting to happen.
struct Args {
    tool: &'static str,
    map: Map<String, Value>,
    used: BTreeSet<String>,
}

impl Args {
    fn new(tool: &'static str, value: Value) -> Result<Args> {
        match value {
            Value::Object(map) => Ok(Args {
                tool,
                map,
                used: BTreeSet::new(),
            }),
            Value::Null => Ok(Args {
                tool,
                map: Map::new(),
                used: BTreeSet::new(),
            }),
            _ => Err(Error::invalid(format!(
                "{}: arguments must be an object",
                tool
            ))),
        }
    }

    fn take(&mut self, key: &str) -> Option<Value> {
        self.used.insert(key.to_string());
        self.map.remove(key)
    }

    fn required_str(&mut self, key: &str) -> Result<String> {
        let value = self
            .take(key)
            .ok_or_else(|| Error::invalid(format!("{}: `{key}` is required", self.tool)))?;
        match value {
            Value::String(text) if !text.trim().is_empty() => Ok(text),
            Value::String(_) => Err(Error::invalid(format!(
                "{}: `{key}` must be a non-empty string",
                self.tool
            ))),
            other => Err(Error::invalid(format!(
                "{}: `{key}` must be a string, got {other}",
                self.tool
            ))),
        }
    }

    fn optional_str(&mut self, key: &str) -> Result<Option<String>> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(other) => Err(Error::invalid(format!(
                "{}: `{key}` must be a string, got {other}",
                self.tool
            ))),
        }
    }

    /// `None` (absent) keeps the field, `null` clears it, a string sets it.
    fn optional_nullable_str(&mut self, key: &str) -> Result<Option<Option<String>>> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::String(text)) => Ok(Some(Some(text))),
            Some(other) => Err(Error::invalid(format!(
                "{}: `{key}` must be a string or null, got {other}",
                self.tool
            ))),
        }
    }

    fn optional_i64(&mut self, key: &str) -> Result<Option<i64>> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => number.as_i64().map(Some).ok_or_else(|| {
                Error::invalid(format!("{}: `{key}` must be an integer", self.tool))
            }),
            Some(other) => Err(Error::invalid(format!(
                "{}: `{key}` must be an integer, got {other}",
                self.tool
            ))),
        }
    }

    fn optional_bool(&mut self, key: &str) -> Result<Option<bool>> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(flag)) => Ok(Some(flag)),
            Some(other) => Err(Error::invalid(format!(
                "{}: `{key}` must be a boolean, got {other}",
                self.tool
            ))),
        }
    }

    fn optional_str_list(&mut self, key: &str) -> Result<Option<Vec<String>>> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(values)) => {
                let mut out = Vec::with_capacity(values.len());
                for value in values {
                    match value {
                        Value::String(text) => out.push(text),
                        other => {
                            return Err(Error::invalid(format!(
                                "{}: `{key}` must be an array of strings, got {other} inside",
                                self.tool
                            )));
                        }
                    }
                }
                Ok(Some(out))
            }
            Some(other) => Err(Error::invalid(format!(
                "{}: `{key}` must be an array of strings, got {other}",
                self.tool
            ))),
        }
    }

    /// Reject leftovers so a typo cannot look like a successful write.
    fn finish(self) -> Result<()> {
        if self.map.is_empty() {
            return Ok(());
        }
        let unknown: Vec<String> = self.map.keys().cloned().collect();
        Err(Error::invalid(format!(
            "{}: unknown argument(s) {}",
            self.tool,
            unknown.join(", ")
        )))
    }
}

// ---------------------------------------------------------------------------
// Tool catalogue
// ---------------------------------------------------------------------------

/// The MCP `tools/list` result body.
pub fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "apm_dashboard",
            "title": "APM Dashboard",
            "description": "Show the AgentPM status board: waiting for a human, in progress, no substantive progress, lost leases, blocked dependencies, recently done, ready and unclaimed. Use it at the start of work or when asked what is going on.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": "Project name or id when the database holds several" },
                    "no_progress_after_seconds": { "type": "integer", "minimum": 1, "description": "Override the staleness window used for the no-substantive-progress bucket" },
                    "done_recent_seconds": { "type": "integer", "minimum": 1, "description": "Override the window used for the recently-done bucket" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apm_get",
            "title": "APM Get Item",
            "description": "Read one work item: handover context (goal, current_state, constraints, references), acceptance criteria, evidence, dependencies, attention requests and the current claim. Read it before claiming.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item": { "type": "string", "description": "Work item identifier (e.g. APM-3), uuid, or number" }
                },
                "required": ["item"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apm_claim",
            "title": "APM Claim",
            "description": "Take the exclusive lease on a work item before writing to it. Every other apm write needs this claim. Fails with claim_conflict if another agent holds a live claim. The plaintext token is never returned; it is stored in a 0600 session file next to the database.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item": { "type": "string", "description": "Work item identifier, uuid, or number" },
                    "lease_seconds": { "type": "integer", "minimum": 30, "maximum": 86400, "description": "Lease length; defaults to the project setting (usually 900)" },
                    "takeover": { "type": "boolean", "description": "Recover an expired lease, or this session's own lease" },
                    "reason": { "type": "string", "description": "Why you are taking over" },
                    "idempotency_key": { "type": "string", "description": "Replay-safe key; a replay returns the original response without a second claim" }
                },
                "required": ["item"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apm_progress",
            "title": "APM Progress",
            "description": "Record substantive progress on a claimed item, with the next concrete step, an optional blocker and an optional waiting-on-human flag. A request that matches the current state is rejected as a no-op; use apm_sync for liveness only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item": { "type": "string" },
                    "summary": { "type": "string", "description": "What concretely changed (non-empty; never a token)" },
                    "next_action": { "type": "string", "description": "The next concrete step; null clears it" },
                    "blocker": { "type": "string", "description": "What is blocking the item; null clears it" },
                    "waiting_on_human": { "type": "boolean", "description": "True when progress needs a human decision" },
                    "waiting_reason": { "type": "string", "description": "Why a human is needed; null clears it" },
                    "expect_version": { "type": "integer", "minimum": 1, "description": "Optimistic-lock version you just read; a stale value fails with version_conflict" },
                    "idempotency_key": { "type": "string" }
                },
                "required": ["item", "summary"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apm_status",
            "title": "APM Status",
            "description": "Move a claimed item through its declarative workflow. The legal target states are decided by the item's workflow snapshot, not by this schema: the default workflow offers ready/doing/blocked/review/done/cancelled, and a project may bind a custom workflow with extra states (e.g. qa) to an item type. done is refused while acceptance criteria are open, or without verification evidence when the item defines none.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item": { "type": "string" },
                    "to": { "type": "string", "description": "Target state key. Valid keys come from the item's workflow (apm_get does not list them; an illegal key or transition fails with workflow_violation). Always lowercase slug form." },
                    "reason": { "type": "string", "description": "Required by the workflow for blocked/reopen/cancel" },
                    "note": { "type": "string", "description": "Completion note or transition context" },
                    "expect_version": { "type": "integer", "minimum": 1 },
                    "idempotency_key": { "type": "string" }
                },
                "required": ["item", "to"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apm_context",
            "title": "APM Handover Context",
            "description": "Read or revise the handover document of a work item. A successor has no chat history, so goal and current_state must stand alone. A revise merges into the current document: fields you omit keep their current value.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item": { "type": "string" },
                    "action": { "type": "string", "enum": ["show", "revise"] },
                    "version": { "type": "integer", "minimum": 1, "description": "action=show: which context version (default: current)" },
                    "reason": { "type": "string", "description": "action=revise: what changed and why (required)" },
                    "goal": { "type": "string" },
                    "current_state": { "type": "string" },
                    "constraints": { "type": "array", "items": { "type": "string" } },
                    "references": { "type": "array", "items": { "type": "string" } },
                    "notes": { "type": "string", "description": "null clears the notes" },
                    "expect_version": { "type": "integer", "minimum": 1, "description": "action=revise: optimistic-lock version" },
                    "idempotency_key": { "type": "string" }
                },
                "required": ["item", "action"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apm_release",
            "title": "APM Release",
            "description": "Release the claim on a work item when the work is finished or handed off. The token is taken from this session's 0600 session file, never from the caller.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item": { "type": "string" },
                    "reason": { "type": "string", "description": "Why the lease is being given up (handoff notes go in apm_context)" }
                },
                "required": ["item"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "apm_sync",
            "title": "APM Run Sync",
            "description": "Record that a run happened (liveness plus lease renewal) WITHOUT recording progress. Use it when a run produced no verifiable change; it never moves the progress clock.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item": { "type": "string" },
                    "outcome": { "type": "string", "enum": ["completed", "aborted", "unknown"] },
                    "note": { "type": "string" },
                    "idempotency_key": { "type": "string" }
                },
                "required": ["item"],
                "additionalProperties": false
            }
        }),
    ]
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Same bucket order and labels as the Pi extension, so a human sees one board
/// regardless of which surface produced it.
const BOARD_ORDER: [(&str, &str); 8] = [
    ("waiting_human", "WAITING FOR HUMAN"),
    ("lost_lease", "LOST LEASE / AT RISK"),
    ("recovery_failed", "RECOVERY FAILED"),
    ("stalled_no_progress", "NO SUBSTANTIVE PROGRESS"),
    ("waiting_dependencies", "WAITING ON DEPENDENCIES"),
    ("in_progress", "IN PROGRESS"),
    ("recently_done", "RECENTLY DONE"),
    ("ready_unclaimed", "READY / UNCLAIMED"),
];

fn render_board(dashboard: &Dashboard) -> Result<String> {
    let value = serde_json::to_value(dashboard)?;
    let buckets = value
        .get("buckets")
        .and_then(|b| b.as_object())
        .cloned()
        .unwrap_or_default();
    let mut lines = Vec::new();
    for (key, title) in BOARD_ORDER {
        let items = buckets
            .get(key)
            .and_then(|b| b.as_array())
            .cloned()
            .unwrap_or_default();
        lines.push(format!("{title} ({})", items.len()));
        for item in items.iter().take(12) {
            let identifier = item
                .get("identifier")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("?");
            let health = item.get("health").and_then(|v| v.as_str()).unwrap_or("?");
            let title_text = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let version = item.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
            lines.push(format!(
                "  {identifier} {status} {health} v{version} {title_text}"
            ));
            if let Some(progress) = item.get("progress_summary").and_then(|v| v.as_str()) {
                lines.push(format!("      progress: {progress}"));
            }
            if let Some(next) = item.get("next_action").and_then(|v| v.as_str()) {
                lines.push(format!("      next: {next}"));
            }
        }
    }
    Ok(lines.join("\n"))
}

fn render_detail(detail: &ItemDetail) -> String {
    let item = &detail.item;
    let mut lines = vec![
        format!(
            "{} {}  [{} / {}] v{} (context v{})",
            item.identifier,
            item.title,
            item.status.as_str(),
            item.health.as_str(),
            item.version,
            item.context_version
        ),
        format!("  id: {}", item.id),
        format!(
            "  parent: {}   priority: {}   labels: {}",
            item.parent_identifier.clone().unwrap_or_else(|| "-".into()),
            item.priority,
            if item.labels.is_empty() {
                "-".to_string()
            } else {
                item.labels.join(", ")
            }
        ),
        format!(
            "  claim: {}",
            match &item.claim {
                Some(claim) => format!(
                    "{} ({}) expires {}",
                    claim.holder, claim.status, claim.expires_at
                ),
                None => "none".to_string(),
            }
        ),
    ];
    if !item.blocked_by.is_empty() {
        lines.push(format!(
            "  blocked by: {}",
            item.blocked_by
                .iter()
                .map(|d| format!("{} {}", d.identifier, d.title))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    lines.push(format!("  goal: {}", detail.context.doc.goal));
    lines.push(format!(
        "  current_state: {}",
        detail.context.doc.current_state
    ));
    for constraint in &detail.context.doc.constraints {
        lines.push(format!("  constraint: {constraint}"));
    }
    for reference in &detail.context.doc.references {
        lines.push(format!("  reference: {reference}"));
    }
    if let Some(notes) = &detail.context.doc.notes {
        lines.push(format!("  notes: {notes}"));
    }
    if let Some(next) = &item.next_action {
        lines.push(format!("  next_action: {next}"));
    }
    if let Some(blocker) = &item.blocker {
        lines.push(format!("  blocker: {blocker}"));
    }
    if item.waiting_on_human {
        lines.push(format!(
            "  waiting_on_human: {}",
            item.waiting_reason.clone().unwrap_or_else(|| "-".into())
        ));
    }
    if detail.acceptance.is_empty() {
        lines.push("  acceptance: (none defined)".to_string());
    } else {
        lines.push("  acceptance:".to_string());
        for criterion in &detail.acceptance {
            lines.push(format!(
                "    {}. [{}] {}{}",
                criterion.seq,
                criterion.status,
                criterion.text,
                criterion
                    .note
                    .as_ref()
                    .map(|n| format!(" — {n}"))
                    .unwrap_or_default()
            ));
        }
    }
    for evidence in &detail.evidence {
        lines.push(format!(
            "  evidence: {} {}{}",
            evidence.kind,
            evidence.value,
            evidence
                .note
                .as_ref()
                .map(|n| format!(" — {n}"))
                .unwrap_or_default()
        ));
    }
    for attention in &detail.attention {
        lines.push(format!(
            "  attention ({}): {}",
            attention.status, attention.question
        ));
    }
    lines.join("\n")
}

fn render_context(identifier: &str, context: &ContextVersionView) -> String {
    let mut lines = vec![
        format!(
            "{identifier} context v{} (author {}, created {}, reason: {})",
            context.version, context.author, context.created_at, context.reason
        ),
        format!("  goal: {}", context.doc.goal),
        format!("  current_state: {}", context.doc.current_state),
    ];
    for constraint in &context.doc.constraints {
        lines.push(format!("  constraint: {constraint}"));
    }
    for reference in &context.doc.references {
        lines.push(format!("  reference: {reference}"));
    }
    if let Some(notes) = &context.doc.notes {
        lines.push(format!("  notes: {notes}"));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Token redaction
// ---------------------------------------------------------------------------

/// Scrub anything shaped like an APM claim token (`apm_` + 32+ hex chars).
///
/// The adapter never puts a token in a result on purpose; this is the same
/// belt-and-braces scrub the Pi extension applies to digests and prompts, in
/// case a token reaches a ledger field from some other process.
pub fn redact_tokens(text: &str) -> String {
    if !text.contains("apm_") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(position) = rest.find("apm_") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..position]);
        let after = &rest[position + 4..];
        let hex_len = after
            .bytes()
            .take_while(|byte| byte.is_ascii_hexdigit())
            .count();
        if hex_len >= 32 {
            out.push_str("apm_[redacted]");
            rest = &after[hex_len..];
        } else {
            out.push_str("apm_");
            rest = after;
        }
    }
    out
}

fn scrub_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_tokens(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(scrub_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, scrub_value(value)))
                .collect(),
        ),
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct Harness {
        server: McpServer,
        _dir: tempfile::TempDir,
    }

    fn harness() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = McpConfig::new(dir.path().join(".apm").join("apm.db"));
        cfg.session = Some("mcp-test-session".to_string());
        let mut server = McpServer::open(cfg).unwrap();
        server
            .initialize(json!({ "protocolVersion": "2025-06-18" }))
            .unwrap();
        server
            .store
            .init_project(
                "MCP Test",
                Some("MCP"),
                Some("/tmp/mcp-test"),
                Settings::default(),
                &Actor::human("setup"),
            )
            .unwrap();
        server
            .store
            .create_item(
                None,
                crate::store::items::NewItem {
                    title: "wire the adapter".into(),
                    context: ContextDoc {
                        goal: "expose the ledger over stdio".into(),
                        current_state: "nothing yet".into(),
                        constraints: vec!["no daemon".into()],
                        references: vec!["src/mcp.rs".into()],
                        notes: None,
                    },
                    item_type: Some("task".into()),
                    priority: Some("normal".into()),
                    parent: None,
                    depends_on: Vec::new(),
                    labels: Vec::new(),
                    acceptance: vec!["initialize works".into(), "tools/call round-trips".into()],
                    actor: Actor::human("setup"),
                },
                now_ms(),
                None,
            )
            .unwrap();
        Harness { server, _dir: dir }
    }

    fn call(server: &mut McpServer, id: i64, name: &str, arguments: Value) -> Value {
        server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }))
            .expect("tool calls always answer")
    }

    fn structured(response: &Value) -> Value {
        response["result"]["structuredContent"].clone()
    }

    fn result_text(response: &Value) -> String {
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    fn claim(server: &mut McpServer) -> String {
        let response = call(
            server,
            1,
            "apm_claim",
            json!({ "item": "MCP-1", "lease_seconds": 600 }),
        );
        assert!(response["error"].is_null(), "claim failed: {response}");
        assert!(
            response["result"]["isError"].is_null(),
            "claim error: {response}"
        );
        server
            .held_tokens
            .values()
            .next()
            .cloned()
            .expect("claim token is held in memory")
    }

    #[test]
    fn initialize_negotiates_a_supported_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut server = McpServer::open(McpConfig::new(dir.path().join("apm.db"))).unwrap();

        let echo = server
            .initialize(json!({
                "protocolVersion": "2024-11-05",
                "clientInfo": { "name": "test-client", "version": "1" }
            }))
            .unwrap();
        assert_eq!(echo["protocolVersion"], json!("2024-11-05"));
        assert_eq!(echo["serverInfo"]["name"], json!(SERVER_NAME));
        assert_eq!(echo["capabilities"]["tools"]["listChanged"], json!(false));
        assert!(echo["instructions"].as_str().unwrap().contains("apm_claim"));

        let fallback = server
            .initialize(json!({ "protocolVersion": "1999-01-01" }))
            .unwrap();
        assert_eq!(fallback["protocolVersion"], json!(PROTOCOL_VERSION));
        let negotiated = server.negotiated().expect("negotiated");
        assert_eq!(negotiated.requested.as_deref(), Some("1999-01-01"));
        assert_eq!(negotiated.used, PROTOCOL_VERSION);
    }

    #[test]
    fn handshake_and_tools_list_are_well_formed() {
        let mut h = harness();
        let init = h
            .server
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-06-18" }
            }))
            .unwrap();
        assert_eq!(init["jsonrpc"], json!("2.0"));
        assert_eq!(init["id"], json!(1));

        // A notification must not produce a frame.
        assert!(
            h.server
                .handle_message(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .is_none()
        );
        assert!(
            h.server
                .handle_message(json!({ "jsonrpc": "2.0", "id": 9, "method": "ping" }))
                .is_some()
        );

        let listed = h
            .server
            .handle_message(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for required in [
            "apm_dashboard",
            "apm_get",
            "apm_claim",
            "apm_progress",
            "apm_status",
            "apm_context",
            "apm_release",
            "apm_sync",
        ] {
            assert!(names.contains(&required), "missing tool {required}");
        }
        for tool in tools {
            assert_eq!(tool["inputSchema"]["type"], json!("object"));
            assert_eq!(tool["inputSchema"]["additionalProperties"], json!(false));
            assert!(tool["description"].as_str().unwrap().len() > 20);
        }

        // `to` must stay an open string: the legal states belong to the item's
        // workflow, and a fixed enum here would hide custom states from clients.
        let status = tools
            .iter()
            .find(|t| t["name"] == json!("apm_status"))
            .unwrap();
        let to = &status["inputSchema"]["properties"]["to"];
        assert_eq!(to["type"], json!("string"));
        assert!(
            to.get("enum").is_none(),
            "apm_status.to must not pin an enum"
        );
        assert!(
            status["description"].as_str().unwrap().contains("workflow"),
            "the description must point at the workflow as the source of truth"
        );
        // Enumerations that ARE closed vocabularies stay enumerated.
        let context = tools
            .iter()
            .find(|t| t["name"] == json!("apm_context"))
            .unwrap();
        assert_eq!(
            context["inputSchema"]["properties"]["action"]["enum"],
            json!(["show", "revise"])
        );
    }

    #[test]
    fn dashboard_and_get_return_the_ledger() {
        let mut h = harness();
        let board = call(&mut h.server, 1, "apm_dashboard", json!({}));
        assert!(board["error"].is_null(), "{board}");
        let text = result_text(&board);
        assert!(text.contains("READY / UNCLAIMED (1)"), "{text}");
        assert!(text.contains("MCP-1"), "{text}");

        let detail = call(&mut h.server, 2, "apm_get", json!({ "item": "MCP-1" }));
        let structured = structured(&detail);
        assert_eq!(structured["item"]["identifier"], json!("MCP-1"));
        assert_eq!(structured["acceptance"].as_array().unwrap().len(), 2);
        let text = result_text(&detail);
        assert!(
            text.contains("goal: expose the ledger over stdio"),
            "{text}"
        );
        assert!(text.contains("1. [open] initialize works"), "{text}");
    }

    #[test]
    fn claim_never_exposes_the_token_and_persists_it() {
        let mut h = harness();
        let response = call(
            &mut h.server,
            1,
            "apm_claim",
            json!({ "item": "MCP-1", "lease_seconds": 600, "idempotency_key": "claim-1" }),
        );
        let body = response.to_string();
        let structured_value = structured(&response);
        assert_eq!(structured_value["token"], Value::Null);
        assert_eq!(structured_value["token_suppressed"], json!(true));
        assert_eq!(structured_value["token_source"], json!("session_file"));
        assert!(structured_value["claim"].get("token").is_none());

        let token = h.server.held_tokens.values().next().unwrap().clone();
        assert!(token.starts_with("apm_"), "unexpected token shape");
        assert!(!body.contains(&token), "token leaked into the tool result");
        assert!(!result_text(&response).contains(&token));

        // The 0600 session file has it, and the file is not world-readable.
        let saved = h
            .server
            .tokens
            .find("mcp-test-session", "x", "MCP-1")
            .expect("token persisted to the session file");
        assert_eq!(saved.token, token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(h.server.tokens.dir())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .collect();
            files.retain(|path| path.extension().map(|e| e == "json").unwrap_or(false));
            assert_eq!(files.len(), 1, "one session file expected");
            let mode = std::fs::metadata(&files[0]).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "session file must be 0600");
        }

        // Replaying the same idempotency key does not issue a second lease, and
        // the replay still carries no plaintext token.
        let replay = call(
            &mut h.server,
            2,
            "apm_claim",
            json!({ "item": "MCP-1", "lease_seconds": 600, "idempotency_key": "claim-1" }),
        );
        assert_eq!(structured(&replay)["replayed"], json!(true));
        assert_eq!(structured(&replay)["token"], Value::Null);
        // The token is still known to this session (memory + session file).
        assert_eq!(structured(&replay)["token_source"], json!("session_file"));

        // Re-claiming from the *same* session supersedes its own lease (that is
        // how a crashed agent recovers) — it does not deadlock against itself.
        let renewed = call(&mut h.server, 3, "apm_claim", json!({ "item": "MCP-1" }));
        assert!(renewed["result"]["isError"].is_null(), "{renewed}");
        assert_eq!(structured(&renewed)["takeover"], json!(true));
        assert_eq!(structured(&renewed)["token"], Value::Null);
        assert_ne!(
            structured(&renewed)["claim"]["id"],
            structured_value["claim"]["id"]
        );
    }

    #[test]
    fn progress_status_and_release_run_through_the_domain_rules() {
        let mut h = harness();
        claim(&mut h.server);

        let doing = call(
            &mut h.server,
            2,
            "apm_status",
            json!({ "item": "MCP-1", "to": "doing", "expect_version": 1 }),
        );
        assert!(doing["error"].is_null(), "{doing}");
        assert_eq!(structured(&doing)["item"]["status"], json!("doing"));
        assert_eq!(structured(&doing)["item"]["version"], json!(2));

        let progress = call(
            &mut h.server,
            3,
            "apm_progress",
            json!({
                "item": "MCP-1",
                "summary": "implemented the stdio loop",
                "next_action": "write the transport test",
                "expect_version": 2
            }),
        );
        assert!(progress["error"].is_null(), "{progress}");
        let body = structured(&progress);
        assert_eq!(body["substantive"], json!(true));
        assert_eq!(
            body["item"]["progress_summary"],
            json!("implemented the stdio loop")
        );
        assert!(result_text(&progress).contains("progress recorded"));

        // Repeating the same request is a no-op, not a second progress event.
        let repeat = call(
            &mut h.server,
            4,
            "apm_progress",
            json!({ "item": "MCP-1", "summary": "implemented the stdio loop" }),
        );
        assert_eq!(structured(&repeat)["no_op"], json!(true));
        assert!(result_text(&repeat).contains("no substantive change"));

        let release = call(&mut h.server, 5, "apm_release", json!({ "item": "MCP-1" }));
        assert!(release["error"].is_null(), "{release}");
        assert_eq!(structured(&release)["claim"]["status"], json!("released"));
        assert!(h.server.held_tokens.is_empty());
        assert!(
            h.server
                .tokens
                .find("mcp-test-session", "x", "MCP-1")
                .is_none()
        );
    }

    #[test]
    fn writes_without_a_claim_fail_with_a_structured_hint() {
        let mut h = harness();
        let response = call(
            &mut h.server,
            1,
            "apm_progress",
            json!({ "item": "MCP-1", "summary": "sneaky" }),
        );
        // Domain errors live in the result, not in a JSON-RPC error object.
        assert!(response["error"].is_null());
        assert_eq!(response["result"]["isError"], json!(true));
        let body = structured(&response);
        assert_eq!(body["error"]["code"], json!("claim_required"));
        assert_eq!(body["error"]["tool_surface"], json!("mcp"));
        assert!(
            body["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("apm_claim")
        );
        assert!(result_text(&response).contains("hint:"));
    }

    #[test]
    fn stale_expect_version_is_a_version_conflict() {
        let mut h = harness();
        claim(&mut h.server);
        let response = call(
            &mut h.server,
            1,
            "apm_progress",
            json!({
                "item": "MCP-1",
                "summary": "raced",
                "expect_version": 99
            }),
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let body = structured(&response);
        assert_eq!(body["error"]["code"], json!("version_conflict"));
        assert_eq!(body["error"]["details"]["expected"], json!(99));
        assert_eq!(body["error"]["details"]["actual"], json!(1));
    }

    #[test]
    fn done_requires_acceptance_and_evidence_guards() {
        let mut h = harness();
        claim(&mut h.server);
        let response = call(
            &mut h.server,
            1,
            "apm_status",
            json!({ "item": "MCP-1", "to": "done", "note": "trust me" }),
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let body = structured(&response);
        let code = body["error"]["code"].as_str().unwrap();
        // Either guard is legitimate for a bare done; both are enforced by the store.
        assert!(
            code == "completion_evidence_required" || code == "workflow_violation",
            "unexpected code {code}"
        );
    }

    #[test]
    fn context_show_and_revise_merge() {
        let mut h = harness();
        claim(&mut h.server);
        let shown = call(
            &mut h.server,
            1,
            "apm_context",
            json!({ "item": "MCP-1", "action": "show" }),
        );
        assert_eq!(structured(&shown)["context"]["version"], json!(1));

        let missing_reason = call(
            &mut h.server,
            2,
            "apm_context",
            json!({ "item": "MCP-1", "action": "revise", "goal": "x" }),
        );
        assert_eq!(missing_reason["result"]["isError"], json!(true));
        assert_eq!(
            structured(&missing_reason)["error"]["code"],
            json!("invalid_input")
        );

        let revised = call(
            &mut h.server,
            3,
            "apm_context",
            json!({
                "item": "MCP-1",
                "action": "revise",
                "reason": "plan changed",
                "current_state": "stdio loop written"
            }),
        );
        assert!(revised["error"].is_null(), "{revised}");
        let body = structured(&revised);
        assert_eq!(body["context"]["version"], json!(2));
        // The omitted goal and constraints survive the merge.
        assert_eq!(
            body["context"]["doc"]["goal"],
            json!("expose the ledger over stdio")
        );
        assert_eq!(
            body["context"]["doc"]["current_state"],
            json!("stdio loop written")
        );
        assert_eq!(body["context"]["doc"]["constraints"], json!(["no daemon"]));
    }

    #[test]
    fn sync_moves_liveness_not_progress() {
        let mut h = harness();
        claim(&mut h.server);
        let response = call(
            &mut h.server,
            1,
            "apm_sync",
            json!({ "item": "MCP-1", "outcome": "completed", "note": "no change" }),
        );
        assert!(response["error"].is_null(), "{response}");
        let body = structured(&response);
        assert_eq!(body["last_progress_at"], Value::Null);
        assert!(body["last_run_sync_at"].is_string());
        assert!(result_text(&response).contains("last_progress_at unchanged"));

        let bad = call(
            &mut h.server,
            2,
            "apm_sync",
            json!({ "item": "MCP-1", "outcome": "sideways" }),
        );
        assert_eq!(structured(&bad)["error"]["code"], json!("invalid_input"));
    }

    #[test]
    fn protocol_errors_are_shaped_like_jsonrpc() {
        let mut h = harness();
        let unknown = h
            .server
            .handle_message(json!({ "jsonrpc": "2.0", "id": 1, "method": "resources/list" }))
            .unwrap();
        assert_eq!(unknown["error"]["code"], json!(-32601));

        let unknown_tool = h
            .server
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "apm_nuke", "arguments": {} }
            }))
            .unwrap();
        assert_eq!(unknown_tool["error"]["code"], json!(-32602));

        let bad_args = h
            .server
            .handle_message(json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "apm_get", "arguments": { "item": 5 } }
            }))
            .unwrap();
        assert_eq!(
            structured(&bad_args)["error"]["code"],
            json!("invalid_input")
        );

        let unknown_arg = call(
            &mut h.server,
            4,
            "apm_get",
            json!({ "item": "MCP-1", "typo": 1 }),
        );
        assert_eq!(
            structured(&unknown_arg)["error"]["code"],
            json!("invalid_input")
        );
        assert!(
            structured(&unknown_arg)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("typo")
        );

        let parse = h.server.handle_line("{not json").unwrap();
        assert!(parse.contains("-32700"), "{parse}");
        let batch = h.server.handle_line("[]").unwrap();
        assert!(batch.contains("-32600"), "{batch}");
        let missing_method = h
            .server
            .handle_line("{\"jsonrpc\":\"2.0\",\"id\":7}")
            .unwrap();
        assert!(missing_method.contains("-32600"), "{missing_method}");
    }

    #[test]
    fn stdout_frames_are_line_delimited() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = McpConfig::new(dir.path().join("apm.db"));
        cfg.session = Some("stream".into());
        let mut server = McpServer::open(cfg).unwrap();
        server
            .store
            .init_project(
                "Stream",
                Some("STR"),
                None,
                Settings::default(),
                &Actor::human("setup"),
            )
            .unwrap();

        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
        );
        let mut output = Vec::new();
        server
            .serve(std::io::Cursor::new(input.as_bytes()), &mut output)
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "notifications must not produce a frame");
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(first["id"], json!(1));
        assert_eq!(second["id"], json!(2));
        assert!(second["result"]["tools"].as_array().unwrap().len() >= 6);
    }

    #[test]
    fn an_expired_lease_rejects_writes_until_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = McpConfig::new(dir.path().join("apm.db"));
        cfg.session = Some("expiry".into());
        let mut server = McpServer::open(cfg).unwrap();
        server
            .store
            .init_project(
                "Expiry",
                Some("EXP"),
                None,
                Settings {
                    lease_seconds: 30,
                    stale_after_seconds: 10,
                    ..Settings::default()
                },
                &Actor::human("setup"),
            )
            .unwrap();
        server
            .store
            .create_item(
                None,
                crate::store::items::NewItem {
                    title: "expiring lease".into(),
                    context: ContextDoc {
                        goal: "g".into(),
                        current_state: "s".into(),
                        constraints: Vec::new(),
                        references: Vec::new(),
                        notes: None,
                    },
                    item_type: None,
                    priority: None,
                    parent: None,
                    depends_on: Vec::new(),
                    labels: Vec::new(),
                    acceptance: Vec::new(),
                    actor: Actor::human("setup"),
                },
                1_700_000_000_000,
                None,
            )
            .unwrap();

        server = server.with_clock(1_700_000_000_000);
        let claimed = call(&mut server, 1, "apm_claim", json!({ "item": "EXP-1" }));
        assert_eq!(structured(&claimed)["claim"]["lease_seconds"], json!(30));

        // One second past the lease: the token is right but the lease is dead.
        server = server.with_clock(1_700_000_031_000);
        let expired = call(
            &mut server,
            2,
            "apm_progress",
            json!({ "item": "EXP-1", "summary": "too late" }),
        );
        assert_eq!(expired["result"]["isError"], json!(true));
        let error = &structured(&expired)["error"];
        assert_eq!(error["code"], json!("claim_invalid"));
        assert!(error["message"].as_str().unwrap().contains("expired"));

        // Recovery is explicit: takeover, from the same session.
        let recovered = call(
            &mut server,
            3,
            "apm_claim",
            json!({ "item": "EXP-1", "takeover": true, "reason": "lease expired while working" }),
        );
        assert!(recovered["result"]["isError"].is_null(), "{recovered}");
        assert_eq!(structured(&recovered)["takeover"], json!(true));
        let progress = call(
            &mut server,
            4,
            "apm_progress",
            json!({ "item": "EXP-1", "summary": "back after recovery" }),
        );
        assert!(progress["result"]["isError"].is_null(), "{progress}");
    }

    #[test]
    fn token_redaction_catches_the_apm_shape() {
        let token = format!("apm_{}", "ab".repeat(32));
        let text = format!("token {token} leaked");
        assert_eq!(redact_tokens(&text), "token apm_[redacted] leaked");
        // Short prefixes and unrelated words are untouched.
        assert_eq!(
            redact_tokens("apm_1234 and apm_dashboard"),
            "apm_1234 and apm_dashboard"
        );
        let scrubbed = scrub_value(json!({ "nested": [text.clone(), "plain"] }));
        assert!(!scrubbed.to_string().contains(&token));
    }

    #[test]
    fn injected_clock_makes_leases_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = McpConfig::new(dir.path().join("apm.db"));
        cfg.session = Some("clock".into());
        let mut server = McpServer::open(cfg).unwrap().with_clock(1_700_000_000_000);
        server
            .store
            .init_project(
                "Clock",
                Some("CLK"),
                None,
                Settings {
                    lease_seconds: 60,
                    stale_after_seconds: 10,
                    ..Settings::default()
                },
                &Actor::human("setup"),
            )
            .unwrap();
        server
            .store
            .create_item(
                None,
                crate::store::items::NewItem {
                    title: "clocked".into(),
                    context: ContextDoc {
                        goal: "g".into(),
                        current_state: "s".into(),
                        constraints: Vec::new(),
                        references: Vec::new(),
                        notes: None,
                    },
                    item_type: None,
                    priority: None,
                    parent: None,
                    depends_on: Vec::new(),
                    labels: Vec::new(),
                    acceptance: Vec::new(),
                    actor: Actor::human("setup"),
                },
                1_700_000_000_000,
                None,
            )
            .unwrap();
        let response = call(&mut server, 1, "apm_claim", json!({ "item": "CLK-1" }));
        let claim = structured(&response);
        assert_eq!(
            claim["claim"]["leased_at"],
            json!("2023-11-14T22:13:20.000Z")
        );
        assert_eq!(claim["claim"]["lease_seconds"], json!(60));
    }
}
