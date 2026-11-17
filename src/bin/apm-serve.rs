//! `apm-serve` — the optional loopback REST adapter for the AgentPM ledger.
//!
//! Foreground only: no pid file, no auto-start, no daemon. Stop it with Ctrl-C
//! (or your process supervisor). While it is down the CLI and the MCP server
//! keep working, because SQLite is the only source of truth.
//!
//! ```text
//! apm-serve --db .apm/apm.db                 # http://127.0.0.1:7717
//! apm-serve --listen 127.0.0.1:9000 --print-token
//! ```
//!
//! Every non-`/health` request needs `Authorization: Bearer <token>`; the token
//! lives in `<data_dir>/rest-token` (mode 0600) and is printed only with
//! `--print-token`. See `docs/rest.md`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use apm::Error;
use apm::rest::{DEFAULT_LISTEN, RestConfig, RestServer, load_or_create_token};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "apm-serve",
    version,
    about = "Optional loopback REST adapter over the AgentPM work ledger",
    long_about = "Serve the ledger over HTTP on loopback for local tools.\n\n\
                  Foreground process, loopback-only by default, bearer-token protected. \
                  The CLI and the MCP server keep working without it: this is an adapter, \
                  not a service the ledger depends on.\n\n\
                  Routes: GET /health, GET /dashboard, GET /items, GET /items/{id}, \
                  GET /claims, POST /items/{id}/claim, POST /items/{id}/release, \
                  POST /items/{id}/progress, POST /items/{id}/status."
)]
struct Cli {
    /// Database path (default: nearest .apm/apm.db walking up from cwd).
    #[arg(long, env = "APM_DB")]
    db: Option<PathBuf>,
    /// Project name or id when the database holds several.
    #[arg(long, env = "APM_PROJECT")]
    project: Option<String>,
    /// Actor name recorded in the audit trail.
    #[arg(long, env = "APM_AGENT", default_value = "apm-rest")]
    agent: String,
    /// Address to bind. Loopback only unless --allow-remote is given.
    #[arg(long, env = "APM_REST_LISTEN", default_value = DEFAULT_LISTEN)]
    listen: String,
    /// Bind a non-loopback address. Read docs/rest.md before using this.
    #[arg(long)]
    allow_remote: bool,
    /// Session used when the client sends no `X-APM-Session` header.
    #[arg(long, env = "APM_SESSION_ID")]
    session: Option<String>,
    /// Run id recorded with writes.
    #[arg(long, env = "APM_RUN_ID")]
    run: Option<String>,
    /// Bearer token. Default: read (or create) `<data_dir>/rest-token`.
    #[arg(long, env = "APM_REST_TOKEN")]
    token: Option<String>,
    /// Token file to read/create (default: rest-token beside the database).
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Print the bearer token to stdout on startup (it is written to the 0600
    /// token file either way).
    #[arg(long)]
    print_token: bool,
    /// Per-connection read/write timeout in seconds.
    #[arg(long, default_value_t = 10)]
    timeout: u64,
    /// Override the clock (epoch millis or RFC3339). Test/demo seam, same as the
    /// CLI's hidden `--now`: leases can expire deterministically.
    #[arg(long, hide = true)]
    now: Option<String>,
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("apm-serve: {}", err);
            std::process::ExitCode::from(err.exit_code() as u8)
        }
    }
}

fn run(cli: Cli) -> apm::Result<()> {
    let db = match cli.db.clone() {
        Some(path) => path,
        None => resolve_db()?,
    };
    let data_dir = db
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let token_path = cli
        .token_file
        .clone()
        .unwrap_or_else(|| data_dir.join("rest-token"));
    let token = match cli.token.clone() {
        Some(token) if !token.trim().is_empty() => token,
        _ => load_or_create_token(&token_path)?,
    };

    let config = RestConfig {
        db,
        project: cli.project,
        agent: cli.agent,
        listen: cli.listen,
        allow_remote: cli.allow_remote,
        session: cli.session,
        run: cli.run,
        token: token.clone(),
        read_timeout: Duration::from_secs(cli.timeout.max(1)),
        now: match cli.now.as_deref() {
            Some(raw) => Some(parse_clock(raw)?),
            None => None,
        },
        ui: false,
        bootstrap_ticket: None,
        bootstrap_ttl: apm::rest::DEFAULT_BOOTSTRAP_TTL,
    };
    let server = RestServer::bind(config)?;
    let addr = server.local_addr()?;
    println!(
        "apm-serve listening on http://{addr} (loopback: {})",
        addr.ip().is_loopback()
    );
    if addr.ip().is_loopback() {
        println!(
            "  auth: Authorization: Bearer <token from {}>",
            token_path.display()
        );
    } else {
        eprintln!(
            "apm-serve: WARNING: bound to {} (--allow-remote). The ledger is now reachable \
             from the network; the bearer token is the only protection.",
            addr.ip()
        );
        println!(
            "  auth: Authorization: Bearer <token from {}>",
            token_path.display()
        );
    }
    if cli.print_token {
        println!("token: {token}");
    }
    // Make the address machine-readable for scripts and tests even when the
    // token is not printed.
    println!("listening on {addr}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    server.serve()
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

/// Same lookup as the CLI: nearest `.apm/apm.db` from the working directory
/// upwards. Never creates one.
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
