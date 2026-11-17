//! `apm-mcp` — an MCP (Model Context Protocol) server over the AgentPM ledger.
//!
//! Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout, so an MCP client can
//! launch it as a stdio server:
//!
//! ```text
//! {"command": "apm-mcp", "args": [], "env": {"APM_DB": "/repo/.apm/apm.db"}}
//! ```
//!
//! There is no daemon and no socket: one client process is one short-lived
//! server, and the database stays the single authority.

use std::path::{Path, PathBuf};

use apm::Error;
use apm::mcp::{McpConfig, McpServer};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "apm-mcp",
    version,
    about = "MCP stdio server over the AgentPM work ledger",
    long_about = "Expose the AgentPM ledger to MCP clients over stdio.\n\n\
                  Tools: apm_dashboard, apm_get, apm_claim, apm_progress, apm_status, \
                  apm_context, apm_release, apm_sync. Every tool is a translation of one \
                  ledger operation, so claims, optimistic versions, workflow guards and \
                  idempotency behave exactly as they do in the CLI.\n\n\
                  stdout carries JSON-RPC frames only; diagnostics go to stderr."
)]
struct Cli {
    /// Database path (default: nearest .apm/apm.db walking up from cwd).
    #[arg(long, env = "APM_DB")]
    db: Option<PathBuf>,
    /// Project name or id when the database holds several.
    #[arg(long, env = "APM_PROJECT")]
    project: Option<String>,
    /// Agent name recorded in the audit trail.
    #[arg(long, env = "APM_AGENT", default_value = "apm-mcp")]
    agent: String,
    /// Session id. Defaults to a fresh `mcp-<uuid>`: one server process is one
    /// session, and the claim tokens it acquires live in that session's file.
    #[arg(long, env = "APM_SESSION_ID")]
    session: Option<String>,
    /// Run id recorded with writes (optional).
    #[arg(long, env = "APM_RUN_ID")]
    run: Option<String>,
    /// Override the clock (epoch millis or RFC3339). Test/demo seam, same as
    /// the `apm` CLI's hidden `--now`: leases can be expired deterministically
    /// instead of by sleeping.
    #[arg(long, hide = true)]
    now: Option<String>,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let db = match cli.db.clone() {
        Some(path) => path,
        None => match resolve_db() {
            Ok(path) => path,
            Err(err) => {
                eprintln!("apm-mcp: {err}");
                return std::process::ExitCode::from(err.exit_code() as u8);
            }
        },
    };
    let config = McpConfig {
        db,
        project: cli.project,
        agent: cli.agent,
        session: cli.session,
        run: cli.run,
    };
    // A misconfigured client must not silently create an empty ledger that then
    // answers every tool with "no project". Only `apm init` creates databases.
    if !config.db.exists() {
        eprintln!(
            "apm-mcp: no ledger at {}; run `apm init --name <project>` first or point APM_DB at an existing database",
            config.db.display()
        );
        return std::process::ExitCode::from(
            Error::NotFound {
                entity: "database",
                id: config.db.display().to_string(),
            }
            .exit_code() as u8,
        );
    }
    let mut server = match McpServer::open(config) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("apm-mcp: cannot open the ledger: {err}");
            return std::process::ExitCode::from(err.exit_code() as u8);
        }
    };
    if let Some(raw) = cli.now.as_deref() {
        match parse_clock(raw) {
            Ok(at) => server = server.with_clock(at),
            Err(err) => {
                eprintln!("apm-mcp: {err}");
                return std::process::ExitCode::from(err.exit_code() as u8);
            }
        }
    }
    eprintln!(
        "apm-mcp: serving {} (session {})",
        server.db_path().display(),
        server.session()
    );
    match server.serve_stdio() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("apm-mcp: transport error: {err}");
            std::process::ExitCode::from(err.exit_code() as u8)
        }
    }
}

/// Same lookup as the CLI: nearest `.apm/apm.db` from the working directory
/// upwards. Unlike `apm init`, this binary never creates a database — an MCP
/// client should be pointed at an existing ledger (or `APM_DB`).
fn resolve_db() -> apm::Result<PathBuf> {
    let cwd =
        std::env::current_dir().map_err(|e| Error::Internal(format!("cannot read cwd: {e}")))?;
    let mut dir: Option<&Path> = Some(&cwd);
    while let Some(current) = dir {
        let candidate = current.join(".apm").join("apm.db");
        if candidate.exists() {
            return Ok(candidate);
        }
        dir = current.parent();
    }
    Err(Error::NotFound {
        entity: "database",
        id: format!(
            "no .apm/apm.db from {} upwards; run `apm init --name <project>` or pass --db / APM_DB",
            cwd.display()
        ),
    })
}

/// Parse `--now`: epoch milliseconds or RFC 3339, exactly like the CLI.
fn parse_clock(raw: &str) -> apm::Result<i64> {
    if let Ok(ms) = raw.parse::<i64>() {
        return Ok(ms);
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp_millis())
        .map_err(|e| Error::invalid(format!("--now must be epoch millis or RFC3339: {e}")))
}
