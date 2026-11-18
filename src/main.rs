//! `apm` — the CLI. Thin shell over the `apm` library; all invariants live
//! there. Text output is for humans, `--json` is the agent contract.

use std::path::{Path, PathBuf};

use apm::store::claims::{ClaimListing, ClaimPolicy, ClaimRequest};
use apm::store::items::{ItemPatch, ListFilter, NewItem, RunOutcome};
use apm::store::misc::AttentionRequest;
use apm::*;
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "apm",
    version,
    about = "AgentPM — local-first, agent-native work ledger",
    long_about = "A local SQLite work ledger for AI agents: claims, handover context, \
                  progress vs liveness, attention requests, and an audit trail.\n\n\
                  Exit codes: 0 ok, 2 invalid input, 3 not found, 4 version conflict, \
                  5 claim problem, 6 workflow/evidence rule, 7 forbidden, 1 internal."
)]
struct Cli {
    #[command(flatten)]
    g: Globals,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Args, Debug, Clone)]
struct Globals {
    /// Database path (default: nearest .apm/apm.db walking up from cwd).
    #[arg(long, global = true, env = "APM_DB")]
    db: Option<PathBuf>,
    /// Project name or id when the database holds several projects.
    #[arg(long, global = true, env = "APM_PROJECT")]
    project: Option<String>,
    /// Agent name recorded in events (default: $APM_AGENT or "agent").
    #[arg(long, global = true, env = "APM_AGENT")]
    agent: Option<String>,
    /// Current session id (default: $APM_SESSION_ID). Claims are scoped to it.
    #[arg(long, global = true, env = "APM_SESSION_ID")]
    session: Option<String>,
    /// Current run id (default: $APM_RUN_ID).
    #[arg(long, global = true, env = "APM_RUN_ID")]
    run: Option<String>,
    /// Act as a human: work-item writes no longer require a claim.
    #[arg(long, global = true)]
    human: bool,
    /// Machine-readable output (single JSON document on stdout).
    #[arg(long, global = true)]
    json: bool,
    /// Override the clock (epoch millis or RFC3339). For tests and demos.
    #[arg(long, global = true, hide = true)]
    now: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Initialise a project in .apm/apm.db (idempotent).
    Init(InitArgs),
    /// Show or edit project metadata.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// Create a work item. A non-empty goal and current_state are mandatory.
    Create(CreateArgs),
    /// Show one work item with context, acceptance, evidence and attention.
    Get { item: String },
    /// List work items (open by default).
    List(ListArgs),
    /// Patch non-status fields (title, priority, next action, waiting flag...).
    Update(UpdateArgs),
    /// Move a work item through the declarative workflow.
    Status(StatusArgs),
    /// Record substantive progress (bumps last_progress_at and the version).
    Progress(ProgressArgs),
    /// End-of-run liveness sync: renews the lease, never touches progress.
    Sync(SyncArgs),
    /// Extend the claim lease without recording progress.
    Heartbeat(HeartbeatArgs),
    /// Acquire the exclusive claim for a work item.
    Claim(ClaimArgs),
    /// Release a claim (token required; humans may force-release).
    Release(ReleaseArgs),
    /// Show the claims held by a session.
    Claims(ClaimsArgs),
    /// Read or revise the handover context.
    Context {
        #[command(subcommand)]
        cmd: ContextCmd,
    },
    /// Add or remove labels.
    Label {
        #[command(subcommand)]
        cmd: LabelCmd,
    },
    /// Manage dependencies.
    Dep {
        #[command(subcommand)]
        cmd: DepCmd,
    },
    /// Manage acceptance criteria.
    Accept {
        #[command(subcommand)]
        cmd: AcceptCmd,
    },
    /// Attach verification evidence.
    Evidence {
        #[command(subcommand)]
        cmd: EvidenceCmd,
    },
    /// Structured "waiting for a human" requests.
    Attention {
        #[command(subcommand)]
        cmd: AttentionCmd,
    },
    /// Status overview: waiting-human, in-progress, stalled, lost lease, deps.
    Dashboard(DashboardArgs),
    /// Audit trail (per item, or the whole project).
    Events(EventsArgs),
    /// Inspect, validate, register, bind and upgrade declarative workflows.
    ///
    /// `apm workflow` prints the project default document (legacy behaviour);
    /// use the subcommands for configuration and upgrades.
    Workflow {
        #[command(subcommand)]
        cmd: Option<WorkflowCmd>,
    },
    /// Mark expired claims as expired (safe to run repeatedly).
    Expire,
    /// Health check of the database, claims, workflow and side-run diagnostics.
    Doctor,
    /// Append a diagnostic note (no claim required; never changes item state).
    Diag(DiagArgs),
    /// Create a WAL-consistent, self-contained backup (refuses to overwrite).
    ///
    /// The snapshot includes committed WAL frames and every business table. It is
    /// a copy of the ledger, so it keeps the same trust boundary: do not commit it.
    Backup(BackupArgs),
    /// Export a redacted, versioned NDJSON audit view of the project.
    ///
    /// Claims, sessions, tokens and idempotency keys are excluded by construction;
    /// token-shaped strings are redacted defensively. This is for audit/archival,
    /// not for live sync (there is no importer).
    Export(ExportArgs),
    /// Serve the embedded read-only dashboard on loopback and open a browser.
    ///
    /// Same process and same guards as `apm-serve`, plus `/ui` assets, a strict
    /// CSP and a token delivered in the URL fragment (never written to disk,
    /// the HTML or a log file).
    Ui(UiArgs),
}

#[derive(Args, Debug)]
struct UiArgs {
    /// Loopback port. 0 (the default) picks a free one.
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Full bind address, overriding --port.
    #[arg(long)]
    listen: Option<String>,
    /// Do not open a browser; print the URL instead.
    #[arg(long)]
    no_open: bool,
    /// Bind a non-loopback address. Read docs/rest.md before using this.
    #[arg(long)]
    allow_remote: bool,
    /// Per-connection read/write timeout in seconds.
    #[arg(long, default_value_t = 10)]
    timeout: u64,
    /// Seconds the one-time bootstrap ticket in the URL stays valid.
    #[arg(long, default_value_t = 60, hide = true)]
    ticket_ttl: u64,
}

#[derive(Args, Debug)]
struct InitArgs {
    #[arg(long)]
    name: String,
    #[arg(long)]
    prefix: Option<String>,
    #[arg(long)]
    root: Option<PathBuf>,
    #[arg(long, default_value_t = 900)]
    lease_seconds: i64,
    #[arg(long, default_value_t = 300)]
    stale_after: i64,
    #[arg(long, default_value_t = 86_400)]
    no_progress_after: i64,
    #[arg(long, default_value_t = 86_400)]
    done_recent: i64,
}

#[derive(Subcommand, Debug)]
enum ProjectCmd {
    Show,
    List,
    Settings(SettingsArgs),
}

#[derive(Args, Debug)]
struct SettingsArgs {
    #[arg(long)]
    lease_seconds: Option<i64>,
    #[arg(long)]
    stale_after: Option<i64>,
    #[arg(long)]
    no_progress_after: Option<i64>,
    #[arg(long)]
    done_recent: Option<i64>,
}

#[derive(Args, Debug)]
struct CreateArgs {
    #[arg(long)]
    title: Option<String>,
    #[arg(long)]
    goal: Option<String>,
    #[arg(long = "current-state")]
    current_state: Option<String>,
    #[arg(long = "constraint")]
    constraints: Vec<String>,
    #[arg(long = "reference")]
    references: Vec<String>,
    #[arg(long)]
    notes: Option<String>,
    /// Full structured context document as JSON (alternative to the flags).
    #[arg(long = "context-json")]
    context_json: Option<String>,
    #[arg(long = "context-file")]
    context_file: Option<PathBuf>,
    #[arg(long = "type", default_value = "task")]
    item_type: String,
    #[arg(long, default_value = "normal")]
    priority: String,
    #[arg(long)]
    parent: Option<String>,
    #[arg(long = "depends-on")]
    depends_on: Vec<String>,
    #[arg(long = "label")]
    labels: Vec<String>,
    #[arg(long = "acceptance")]
    acceptance: Vec<String>,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct ListArgs {
    #[arg(long = "status", value_delimiter = ',')]
    status: Vec<String>,
    /// Include done/cancelled items.
    #[arg(long)]
    all: bool,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    parent: Option<String>,
    /// Only items currently claimed by this session.
    #[arg(long = "held-by")]
    held_by: Option<String>,
    /// Only items currently claimed by this agent.
    #[arg(long = "by")]
    by: Option<String>,
    /// Substring match on title or identifier.
    #[arg(long = "match")]
    keywords: Option<String>,
    /// Only items with an open attention request.
    #[arg(long)]
    attention: bool,
    #[arg(long, default_value_t = 50)]
    limit: i64,
}

#[derive(Args, Debug)]
struct UpdateArgs {
    item: String,
    #[arg(long)]
    token: Option<String>,
    #[arg(long = "expect-version")]
    expect_version: Option<i64>,
    #[arg(long)]
    title: Option<String>,
    #[arg(long = "type")]
    item_type: Option<String>,
    #[arg(long)]
    priority: Option<String>,
    #[arg(long = "next-action")]
    next_action: Option<String>,
    #[arg(long)]
    blocker: Option<String>,
    #[arg(long = "waiting-human", conflicts_with = "not_waiting_human")]
    waiting_human: bool,
    #[arg(long = "not-waiting-human")]
    not_waiting_human: bool,
    #[arg(long = "waiting-reason")]
    waiting_reason: Option<String>,
    /// New parent selector, or "-" to detach.
    #[arg(long)]
    parent: Option<String>,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct StatusArgs {
    item: String,
    #[arg(long = "to")]
    to: String,
    #[arg(long)]
    token: Option<String>,
    #[arg(long = "expect-version")]
    expect_version: Option<i64>,
    #[arg(long)]
    reason: Option<String>,
    #[arg(long)]
    note: Option<String>,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct ProgressArgs {
    item: String,
    #[arg(long)]
    token: Option<String>,
    #[arg(long = "expect-version")]
    expect_version: Option<i64>,
    #[arg(long)]
    summary: String,
    #[arg(long = "next-action")]
    next_action: Option<String>,
    #[arg(long)]
    blocker: Option<String>,
    #[arg(long = "waiting-human")]
    waiting_human: bool,
    #[arg(long = "waiting-reason")]
    waiting_reason: Option<String>,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct SyncArgs {
    item: String,
    #[arg(long)]
    token: Option<String>,
    /// completed | aborted | unknown
    #[arg(long, default_value = "completed")]
    outcome: String,
    #[arg(long)]
    note: Option<String>,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct HeartbeatArgs {
    item: String,
    #[arg(long)]
    token: Option<String>,
    #[arg(long)]
    extend: Option<i64>,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct ClaimArgs {
    item: String,
    #[arg(long)]
    lease: Option<i64>,
    /// Take over an expired (or own) claim. Humans may force a live takeover.
    #[arg(long)]
    takeover: bool,
    #[arg(long)]
    reason: Option<String>,
    /// Side-run recovery mode: only re-acquire a lease this session held.
    #[arg(long = "recover-only")]
    recover_only: bool,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Args, Debug)]
struct ReleaseArgs {
    item: String,
    #[arg(long)]
    token: Option<String>,
    #[arg(long)]
    reason: Option<String>,
    /// Human only: release someone else's claim without a token.
    #[arg(long)]
    force: bool,
}

#[derive(Args, Debug)]
struct ClaimsArgs {
    #[arg(long = "held-by")]
    held_by: Option<String>,
    /// Include expired claims this session held (recovery candidates).
    #[arg(long = "include-expired")]
    include_expired: bool,
    /// Include claim tokens from the session token file (used by the run-end
    /// side-run, which must reuse the existing claim). Local secrets only.
    #[arg(long = "with-tokens")]
    with_tokens: bool,
}

#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)]
enum ContextCmd {
    Show {
        item: String,
        #[arg(long)]
        version: Option<i64>,
    },
    History {
        item: String,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    Revise(ReviseArgs),
}

#[derive(Args, Debug)]
struct ReviseArgs {
    item: String,
    #[arg(long)]
    token: Option<String>,
    #[arg(long = "expect-version")]
    expect_version: Option<i64>,
    #[arg(long = "expect-context-version")]
    expect_context_version: Option<i64>,
    #[arg(long)]
    goal: Option<String>,
    #[arg(long = "current-state")]
    current_state: Option<String>,
    #[arg(long = "constraint")]
    constraints: Vec<String>,
    #[arg(long = "reference")]
    references: Vec<String>,
    #[arg(long)]
    notes: Option<String>,
    #[arg(long = "context-json")]
    context_json: Option<String>,
    #[arg(long = "context-file")]
    context_file: Option<PathBuf>,
    #[arg(long)]
    reason: String,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Subcommand, Debug)]
enum LabelCmd {
    Add {
        item: String,
        label: String,
        #[arg(long)]
        token: Option<String>,
    },
    Remove {
        item: String,
        label: String,
        #[arg(long)]
        token: Option<String>,
    },
    List {
        item: String,
    },
}

#[derive(Subcommand, Debug)]
enum DepCmd {
    Add {
        item: String,
        depends_on: String,
        #[arg(long)]
        token: Option<String>,
    },
    Remove {
        item: String,
        depends_on: String,
        #[arg(long)]
        token: Option<String>,
    },
    List {
        item: String,
    },
}

#[derive(Subcommand, Debug)]
enum AcceptCmd {
    Add {
        item: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        token: Option<String>,
    },
    Set {
        item: String,
        #[arg(long)]
        seq: i64,
        #[arg(long, default_value = "met")]
        status: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    List {
        item: String,
    },
}

#[derive(Subcommand, Debug)]
enum EvidenceCmd {
    Add {
        item: String,
        #[arg(long, default_value = "test")]
        kind: String,
        #[arg(long)]
        value: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long = "idempotency-key")]
        idempotency_key: Option<String>,
    },
    List {
        item: String,
    },
}

#[derive(Subcommand, Debug)]
enum AttentionCmd {
    Request(AttentionRequestArgs),
    List {
        #[arg(long)]
        item: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    Reply {
        id: String,
        #[arg(long)]
        response: String,
        #[arg(long = "idempotency-key")]
        idempotency_key: Option<String>,
    },
    Withdraw {
        id: String,
        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Args, Debug)]
struct AttentionRequestArgs {
    #[arg(long)]
    item: Option<String>,
    #[arg(long)]
    question: String,
    #[arg(long)]
    context: String,
    #[arg(long = "option")]
    options: Vec<String>,
    #[arg(long)]
    recommend: Option<String>,
    #[arg(long)]
    blocking: bool,
    #[arg(long = "non-blocking", conflicts_with = "blocking")]
    non_blocking: bool,
    #[arg(long)]
    token: Option<String>,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

#[derive(Subcommand, Debug)]
enum WorkflowCmd {
    /// Show a workflow document (default: the project default workflow).
    Show(WorkflowShowArgs),
    /// List registered workflow versions.
    List,
    /// List item-type bindings and the project default.
    Types,
    /// Validate a workflow document without touching the database.
    Validate(WorkflowDefArgs),
    /// Validate and register a versioned workflow, optionally binding types.
    Set(WorkflowSetArgs),
    /// Bind an item type to an existing workflow version.
    Bind(WorkflowBindArgs),
    /// Move an item type (and optionally its open items) to a newer version.
    Upgrade(WorkflowUpgradeArgs),
}

#[derive(Args, Debug)]
struct WorkflowShowArgs {
    /// Show the workflow bound to this item type (falls back to the default).
    #[arg(long = "type")]
    item_type: Option<String>,
    /// Workflow id (requires --version).
    #[arg(long)]
    id: Option<String>,
    /// Workflow version (requires --id).
    #[arg(long)]
    version: Option<i64>,
}

#[derive(Args, Debug)]
struct WorkflowDefArgs {
    /// Path to a workflow JSON document.
    #[arg(long, conflicts_with = "definition_json")]
    file: Option<PathBuf>,
    /// Workflow JSON document as a string.
    #[arg(long = "definition-json", conflicts_with = "file")]
    definition_json: Option<String>,
}

#[derive(Args, Debug)]
struct WorkflowSetArgs {
    #[command(flatten)]
    def: WorkflowDefArgs,
    /// Bind these comma-separated item types to the new version.
    #[arg(long = "bind", value_delimiter = ',')]
    bind: Vec<String>,
    /// Also make this version the project default workflow.
    #[arg(long)]
    default: bool,
    /// Validate and show what would change without writing.
    #[arg(long = "dry-run")]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct WorkflowBindArgs {
    #[arg(long = "type")]
    item_type: String,
    #[arg(long)]
    id: String,
    #[arg(long)]
    version: i64,
}

#[derive(Args, Debug)]
struct WorkflowUpgradeArgs {
    #[arg(long = "type")]
    item_type: String,
    /// Target workflow id (defaults to the type's current binding).
    #[arg(long)]
    id: Option<String>,
    #[arg(long = "to-version")]
    to_version: i64,
    /// Re-snapshot existing open items onto the target version.
    #[arg(long = "migrate-items")]
    migrate_items: bool,
    /// State mapping for items whose state is not declared by the target, e.g. --map done=shipped.
    #[arg(long = "map", value_delimiter = ',')]
    maps: Vec<String>,
    #[arg(long = "dry-run")]
    dry_run: bool,
}

#[derive(Args, Debug)]
struct DashboardArgs {
    #[arg(long = "no-progress-after")]
    no_progress_after: Option<i64>,
    #[arg(long = "done-recent")]
    done_recent: Option<i64>,
}

#[derive(Args, Debug)]
struct EventsArgs {
    item: Option<String>,
    #[arg(long, default_value_t = 50)]
    limit: i64,
}

#[derive(Args, Debug)]
struct BackupArgs {
    /// Destination database file (default: apm-backup-<UTC timestamp>.db in cwd).
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct ExportArgs {
    /// Write NDJSON to this file instead of stdout. Refuses to overwrite and is
    /// created with mode 0600 where the platform supports it.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct DiagArgs {
    #[arg(long)]
    item: Option<String>,
    #[arg(long)]
    code: String,
    #[arg(long)]
    message: String,
    #[arg(long = "idempotency-key")]
    idempotency_key: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let json_mode = cli.g.json;
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            if json_mode {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&err.to_json())
                        .unwrap_or_else(|_| format!("{{\"ok\":false,\"error\":\"{}\"}}", err))
                );
            } else {
                eprintln!("apm: error [{}]: {}", err.code(), err);
            }
            std::process::ExitCode::from(err.exit_code() as u8)
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    // `workflow validate` is pure: it must work in CI without a project database.
    if let Cmd::Workflow {
        cmd: Some(WorkflowCmd::Validate(args)),
    } = &cli.cmd
    {
        return cmd_workflow_validate(args, cli.g.json);
    }
    // `apm ui` runs its own loopback server; it does not need a `Store` handle in
    // this process (the server opens one, exactly like `apm-serve`).
    if let Cmd::Ui(args) = &cli.cmd {
        return cmd_ui(&cli.g, args);
    }
    let at = match &cli.g.now {
        Some(raw) => parse_now(raw)?,
        None => now_ms(),
    };
    let init_mode = matches!(&cli.cmd, Cmd::Init(_));
    let db_path = resolve_db(cli.g.db.as_deref(), init_mode)?;
    let store = Store::open(&db_path)?;
    let tokens = session_store::TokenStore::new(store.data_dir());
    let app = App {
        globals: cli.g.clone(),
        at,
        store,
        tokens,
    };
    dispatch(app, cli.cmd)
}

struct App {
    globals: Globals,
    at: i64,
    store: Store,
    tokens: session_store::TokenStore,
}

impl App {
    fn actor(&self) -> Actor {
        let (name, kind) = if self.globals.human {
            (
                self.globals
                    .agent
                    .clone()
                    .or_else(|| std::env::var("USER").ok())
                    .unwrap_or_else(|| "human".to_string()),
                ActorKind::Human,
            )
        } else {
            (
                self.globals
                    .agent
                    .clone()
                    .or_else(|| std::env::var("APM_AGENT").ok())
                    .unwrap_or_else(|| "agent".to_string()),
                ActorKind::Agent,
            )
        };
        Actor {
            name,
            kind,
            session_id: self.globals.session.clone(),
            run_id: self.globals.run.clone(),
        }
    }

    /// Resolve a write token: explicit `--token`, else the session token file.
    fn token(&self, selector: &str, explicit: Option<&str>) -> Result<Option<String>> {
        if let Some(token) = explicit {
            return Ok(Some(token.to_string()));
        }
        // A human acting without an explicit token uses the claim-free human path;
        // a stale session token would otherwise be picked up and rejected.
        if self.globals.human {
            return Ok(None);
        }
        let Some(session) = &self.globals.session else {
            return Ok(None);
        };
        let (id, identifier) = self.store.item_ref(selector)?;
        Ok(self.tokens.find(session, &id, &identifier).map(|t| t.token))
    }

    fn out(&self, value: impl Serialize, text: String) -> Result<()> {
        if self.globals.json {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else if !text.is_empty() {
            println!("{text}");
        }
        Ok(())
    }

    fn out_ok<T: Serialize>(&self, data: T, text: String) -> Result<()> {
        #[derive(Serialize)]
        struct Ok2<T: Serialize> {
            ok: bool,
            #[serde(flatten)]
            data: T,
        }
        self.out(Ok2 { ok: true, data }, text)
    }
}

fn dispatch(app: App, cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Init(args) => cmd_init(app, args),
        Cmd::Project { cmd } => cmd_project(app, cmd),
        Cmd::Create(args) => cmd_create(app, args),
        Cmd::Get { item } => cmd_get(app, &item),
        Cmd::List(args) => cmd_list(app, args),
        Cmd::Update(args) => cmd_update(app, args),
        Cmd::Status(args) => cmd_status(app, args),
        Cmd::Progress(args) => cmd_progress(app, args),
        Cmd::Sync(args) => cmd_sync(app, args),
        Cmd::Heartbeat(args) => cmd_heartbeat(app, args),
        Cmd::Claim(args) => cmd_claim(app, args),
        Cmd::Release(args) => cmd_release(app, args),
        Cmd::Claims(args) => cmd_claims(app, args),
        Cmd::Context { cmd } => cmd_context(app, cmd),
        Cmd::Label { cmd } => cmd_label(app, cmd),
        Cmd::Dep { cmd } => cmd_dep(app, cmd),
        Cmd::Accept { cmd } => cmd_accept(app, cmd),
        Cmd::Evidence { cmd } => cmd_evidence(app, cmd),
        Cmd::Attention { cmd } => cmd_attention(app, cmd),
        Cmd::Dashboard(args) => cmd_dashboard(app, args),
        Cmd::Events(args) => cmd_events(app, args),
        Cmd::Workflow { cmd } => cmd_workflow(app, cmd),
        Cmd::Expire => cmd_expire(app),
        Cmd::Doctor => cmd_doctor(app),
        Cmd::Diag(args) => cmd_diag(app, args),
        Cmd::Backup(args) => cmd_backup(app, args),
        Cmd::Export(args) => cmd_export(app, args),
        // Handled before the Store is opened (see `run`).
        Cmd::Ui(_) => unreachable!("apm ui is dispatched before any Store is opened"),
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_ui(g: &Globals, args: &UiArgs) -> Result<()> {
    let db = resolve_db(g.db.as_deref(), false)?;
    let listen = args
        .listen
        .clone()
        .unwrap_or_else(|| format!("127.0.0.1:{}", args.port));
    let agent = g
        .agent
        .clone()
        .or_else(|| std::env::var("APM_AGENT").ok())
        .unwrap_or_else(|| "agent".to_string());
    let now = match g.now.as_deref() {
        Some(raw) => Some(parse_now(raw)?),
        None => None,
    };
    apm::ui::serve(apm::ui::UiOptions {
        db,
        project: g.project.clone(),
        agent,
        listen,
        allow_remote: args.allow_remote,
        session: g.session.clone(),
        run: g.run.clone(),
        now,
        timeout: std::time::Duration::from_secs(args.timeout.max(1)),
        ticket_ttl: std::time::Duration::from_secs(args.ticket_ttl),
        open: !args.no_open,
    })
}

fn cmd_init(mut app: App, args: InitArgs) -> Result<()> {
    let settings = Settings {
        lease_seconds: args.lease_seconds,
        stale_after_seconds: args.stale_after,
        no_progress_after_seconds: args.no_progress_after,
        done_recent_seconds: args.done_recent,
    };
    let root = args
        .root
        .or_else(|| std::env::current_dir().ok())
        .map(|p| p.display().to_string());
    let (project, created) = app.store.init_project(
        &args.name,
        args.prefix.as_deref(),
        root.as_deref(),
        settings,
        &app.actor(),
    )?;
    let text = format!(
        "{} project {} ({}){}\n  db: {}",
        if created {
            "initialised"
        } else {
            "found existing"
        },
        project.name,
        project.id,
        "",
        project.db_path
    );
    app.out_ok(json!({ "project": project, "created": created }), text)
}

fn cmd_project(mut app: App, cmd: ProjectCmd) -> Result<()> {
    match cmd {
        ProjectCmd::Show => {
            let project = app.store.project_view(app.globals.project.as_deref())?;
            let text = format!(
                "{}\n  id: {}\n  root: {}\n  db: {}\n  lease: {}s  stale: {}s  no-progress: {}s  recent-done: {}s",
                project.name,
                project.id,
                project.root_path.clone().unwrap_or_else(|| "-".into()),
                project.db_path,
                project.settings.lease_seconds,
                project.settings.stale_after_seconds,
                project.settings.no_progress_after_seconds,
                project.settings.done_recent_seconds
            );
            app.out_ok(json!({ "project": project }), text)
        }
        ProjectCmd::List => {
            let projects = app.store.list_projects()?;
            let text = projects
                .iter()
                .map(|p| {
                    format!(
                        "{}  {}  {}",
                        p.name,
                        p.id,
                        p.root_path.clone().unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            app.out_ok(json!({ "projects": projects }), text)
        }
        ProjectCmd::Settings(args) => {
            let current = app
                .store
                .project_view(app.globals.project.as_deref())?
                .settings;
            let mut next = current.clone();
            if let Some(v) = args.lease_seconds {
                next.lease_seconds = v;
            }
            if let Some(v) = args.stale_after {
                next.stale_after_seconds = v;
            }
            if let Some(v) = args.no_progress_after {
                next.no_progress_after_seconds = v;
            }
            if let Some(v) = args.done_recent {
                next.done_recent_seconds = v;
            }
            let actor = app.actor();
            let project =
                app.store
                    .update_settings(app.globals.project.as_deref(), next.clone(), &actor)?;
            let text = format!(
                "settings updated\n  lease: {}s\n  stale: {}s\n  no-progress: {}s\n  recent-done: {}s",
                next.lease_seconds,
                next.stale_after_seconds,
                next.no_progress_after_seconds,
                next.done_recent_seconds
            );
            app.out_ok(
                json!({ "project": project, "previous": current, "settings": next }),
                text,
            )
        }
    }
}

fn build_context(
    goal: Option<String>,
    current_state: Option<String>,
    constraints: Vec<String>,
    references: Vec<String>,
    notes: Option<String>,
    context_json: Option<String>,
    context_file: Option<PathBuf>,
) -> Result<ContextDoc> {
    if context_json.is_some() && context_file.is_some() {
        return Err(Error::invalid(
            "--context-json and --context-file are mutually exclusive",
        ));
    }
    if let Some(raw) = context_json {
        return serde_json::from_str::<ContextDoc>(&raw)
            .map_err(|e| Error::invalid(format!("--context-json is not a context document: {e}")));
    }
    if let Some(path) = context_file {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| Error::invalid(format!("cannot read {}: {e}", path.display())))?;
        return serde_json::from_str::<ContextDoc>(&raw).map_err(|e| {
            Error::invalid(format!("{} is not a context document: {e}", path.display()))
        });
    }
    Ok(ContextDoc {
        goal: goal.unwrap_or_default(),
        current_state: current_state.unwrap_or_default(),
        constraints: constraints
            .into_iter()
            .filter(|c| !c.trim().is_empty())
            .collect(),
        references: references
            .into_iter()
            .filter(|c| !c.trim().is_empty())
            .collect(),
        notes,
    })
}

fn cmd_create(mut app: App, args: CreateArgs) -> Result<()> {
    let title = args
        .title
        .clone()
        .ok_or_else(|| Error::invalid("--title is required"))?;
    let doc = build_context(
        args.goal,
        args.current_state,
        args.constraints,
        args.references,
        args.notes,
        args.context_json,
        args.context_file,
    )?;
    doc.validate()?;
    let new = NewItem {
        title,
        context: doc,
        item_type: Some(args.item_type),
        priority: Some(args.priority),
        parent: args.parent,
        depends_on: args.depends_on,
        labels: args.labels,
        acceptance: args.acceptance,
        actor: app.actor(),
    };
    let outcome = app.store.create_item(
        app.globals.project.as_deref(),
        new,
        app.at,
        args.idempotency_key.as_deref(),
    )?;
    let text = format!(
        "created {} [{}] {}\n  version {}  context v{}\n  next: apm claim {}",
        outcome.item.identifier,
        outcome.item.status.as_str(),
        outcome.item.title,
        outcome.item.version,
        outcome.context.version,
        outcome.item.identifier
    );
    app.out_ok(
        json!({ "item": outcome.item, "context": outcome.context }),
        text,
    )
}

fn cmd_get(app: App, item: &str) -> Result<()> {
    let detail = app.store.get_item(item, app.at)?;
    let text = render_detail(&detail);
    app.out_ok(detail, text)
}

fn cmd_list(app: App, args: ListArgs) -> Result<()> {
    let mut statuses = Vec::new();
    for s in &args.status {
        statuses.push(Status::parse(s)?);
    }
    let filter = ListFilter {
        project: app.globals.project.clone(),
        status: statuses,
        include_closed: args.all,
        label: args.label,
        parent: args.parent,
        session: args.held_by,
        agent: args.by,
        keywords: args.keywords,
        attention_only: args.attention,
        limit: args.limit,
    };
    let items = app.store.list_items(filter, app.at)?;
    let text = if items.is_empty() {
        "no items".to_string()
    } else {
        items.iter().map(render_row).collect::<Vec<_>>().join("\n")
    };
    app.out_ok(json!({ "items": items, "count": items.len() }), text)
}

fn cmd_update(mut app: App, args: UpdateArgs) -> Result<()> {
    let actor = app.actor();
    let token = app.token(&args.item, args.token.as_deref())?;
    let parent = match args.parent {
        Some(p) if p == "-" => Some(None),
        Some(p) => Some(Some(p)),
        None => None,
    };
    let patch = ItemPatch {
        title: args.title,
        item_type: args.item_type,
        priority: args.priority,
        next_action: args.next_action.map(Some),
        blocker: args.blocker.map(Some),
        waiting_on_human: if args.waiting_human {
            Some(true)
        } else if args.not_waiting_human {
            Some(false)
        } else {
            None
        },
        waiting_reason: args.waiting_reason.map(Some),
        parent,
    };
    let item = app.store.update_item(
        &args.item,
        token.as_deref(),
        args.expect_version,
        patch,
        &actor,
        app.at,
        args.idempotency_key.as_deref(),
    )?;
    let text = format!(
        "{} updated → version {}\n{}",
        item.identifier,
        item.version,
        render_row(&item)
    );
    app.out_ok(json!({ "item": item }), text)
}

fn cmd_status(mut app: App, args: StatusArgs) -> Result<()> {
    let actor = app.actor();
    let token = app.token(&args.item, args.token.as_deref())?;
    let to = Status::parse(&args.to)?;
    let item = app.store.set_status(
        &args.item,
        token.as_deref(),
        args.expect_version,
        to,
        args.reason.as_deref(),
        args.note.as_deref(),
        &actor,
        app.at,
        args.idempotency_key.as_deref(),
    )?;
    let text = format!(
        "{} → {} (version {})\n{}",
        item.identifier,
        item.status.as_str(),
        item.version,
        render_row(&item)
    );
    app.out_ok(json!({ "item": item }), text)
}

fn cmd_progress(mut app: App, args: ProgressArgs) -> Result<()> {
    let actor = app.actor();
    let token = app.token(&args.item, args.token.as_deref())?;
    let outcome = app.store.record_progress(
        &args.item,
        token.as_deref(),
        args.expect_version,
        &args.summary,
        args.next_action.map(Some),
        args.blocker.map(Some),
        if args.waiting_human { Some(true) } else { None },
        args.waiting_reason.map(Some),
        &actor,
        app.at,
        args.idempotency_key.as_deref(),
    )?;
    let text = if outcome.no_op {
        format!(
            "{} no substantive change: request matches the current state, nothing written\n               (use `apm sync` to record liveness; version stays {})",
            outcome.item.identifier, outcome.item.version
        )
    } else if !outcome.substantive {
        format!(
            "{} bookkeeping updated ({}) — last_progress_at unchanged (version {})\n  {}\n  next: {}",
            outcome.item.identifier,
            outcome.changed_fields.join(", "),
            outcome.item.version,
            outcome.item.progress_summary.clone().unwrap_or_default(),
            outcome
                .item
                .next_action
                .clone()
                .unwrap_or_else(|| "-".into())
        )
    } else {
        format!(
            "{} progress recorded (version {}, last_progress_at {})\n  {}\n  next: {}",
            outcome.item.identifier,
            outcome.item.version,
            outcome
                .item
                .last_progress_at
                .clone()
                .unwrap_or_else(|| "-".into()),
            outcome.item.progress_summary.clone().unwrap_or_default(),
            outcome
                .item
                .next_action
                .clone()
                .unwrap_or_else(|| "-".into())
        )
    };
    app.out_ok(json!(outcome), text)
}

fn cmd_sync(mut app: App, args: SyncArgs) -> Result<()> {
    let actor = app.actor();
    let token = app.token(&args.item, args.token.as_deref())?;
    let outcome = RunOutcome::parse(&args.outcome)?;
    let out = app.store.sync_run(
        &args.item,
        token.as_deref(),
        outcome,
        args.note.as_deref(),
        &actor,
        app.at,
        args.idempotency_key.as_deref(),
    )?;
    let text = format!(
        "{} run sync: {} ({}), last_progress_at unchanged ({})",
        out.identifier,
        out.run.outcome,
        out.run.at,
        out.last_progress_at.clone().unwrap_or_else(|| "-".into())
    );
    app.out_ok(out, text)
}

fn cmd_heartbeat(mut app: App, args: HeartbeatArgs) -> Result<()> {
    let actor = app.actor();
    let token = app
        .token(&args.item, args.token.as_deref())?
        .ok_or_else(|| Error::ClaimRequired {
            item: args.item.clone(),
        })?;
    let issued = app.store.heartbeat(
        &args.item,
        &token,
        args.extend,
        &actor,
        app.at,
        args.idempotency_key.as_deref(),
    )?;
    let text = format!(
        "lease renewed for {} until {}",
        issued.claim.identifier, issued.claim.expires_at
    );
    app.out_ok(json!({ "claim": issued.claim }), text)
}

fn cmd_claim(mut app: App, args: ClaimArgs) -> Result<()> {
    let actor = app.actor();
    let side_run = std::env::var("APM_SIDE_RUN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let recover_only = args.recover_only || side_run;
    if recover_only && actor.session_id.is_none() {
        return Err(Error::invalid(
            "--recover-only (or APM_SIDE_RUN=1) requires a session id",
        ));
    }
    let req = ClaimRequest {
        lease_seconds: args.lease,
        takeover: args.takeover || recover_only,
        reason: args.reason.clone(),
        policy: if recover_only {
            ClaimPolicy::RecoverOnly
        } else {
            ClaimPolicy::Normal
        },
    };
    let issued = app.store.claim(
        &args.item,
        &actor,
        req,
        app.at,
        args.idempotency_key.as_deref(),
    )?;
    let side_run = side_run_mode();
    let mut saved = false;
    let mut token = issued.token.clone();
    let mut token_source = if token.is_empty() {
        None
    } else {
        Some("issued")
    };
    if token.is_empty() {
        // Idempotent replay: the database never stores the plaintext token, so the
        // only honest answer is the session file copy, if this session saved one.
        if let Some(session) = &app.globals.session {
            token = app
                .tokens
                .find(
                    session,
                    &issued.claim.work_item_id,
                    &issued.claim.identifier,
                )
                .map(|t| t.token)
                .unwrap_or_default();
            if !token.is_empty() {
                // Same local session, same claim: the token comes from the 0600
                // session file, never from the database.
                token_source = Some("session_file");
            }
        }
    }
    if !token.is_empty()
        && let Some(session) = &app.globals.session
    {
        let st = session_store::SessionToken {
            session_id: session.clone(),
            claim_id: issued.claim.id.clone(),
            item_id: issued.claim.work_item_id.clone(),
            identifier: issued.claim.identifier.clone(),
            token: token.clone(),
            expires_at: issued.claim.expires_at.clone(),
            saved_at: ts(app.at),
        };
        match app.tokens.save(&st) {
            Ok(()) => saved = true,
            Err(e) => eprintln!("apm: warning: could not persist session token: {e}"),
        }
    }
    // In side-run mode the plaintext stays only in the 0600 session file. The
    // claim still works, recovery still works, but stdout (text and JSON) is
    // token-free so the child cannot accidentally paste it into progress/context.
    let emitted_token = if side_run {
        String::new()
    } else {
        token.clone()
    };
    if side_run {
        token_source = if saved { Some("session_file") } else { None };
    }
    let text = format!(
        "{} {} by {} (lease {}s, expires {}){}\n  token: {}",
        if issued.takeover {
            "took over"
        } else {
            "claimed"
        },
        issued.claim.identifier,
        issued.claim.holder,
        issued.claim.lease_seconds,
        issued.claim.expires_at,
        if issued.replayed { " [replayed]" } else { "" },
        if side_run {
            "(APM_SIDE_RUN=1: stored in the 0600 session file, never printed)".to_string()
        } else if emitted_token.is_empty() {
            "(replayed; token was issued on the first call — use the session file or --token)"
                .to_string()
        } else {
            emitted_token.clone()
        }
    );
    app.out_ok(
        json!({
            "claim": issued.claim,
            "token": if emitted_token.is_empty() {
                serde_json::Value::Null
            } else {
                json!(emitted_token)
            },
            "token_source": token_source,
            "token_suppressed": side_run,
            "replayed": issued.replayed,
            "takeover": issued.takeover,
            "session_token_saved": saved,
        }),
        text,
    )
}

fn cmd_release(mut app: App, args: ReleaseArgs) -> Result<()> {
    let actor = app.actor();
    let claim = if args.force {
        let reason = args
            .reason
            .clone()
            .ok_or_else(|| Error::invalid("--force requires --reason"))?;
        app.store
            .force_release(&args.item, &reason, &actor, app.at)?
    } else {
        let token = app
            .token(&args.item, args.token.as_deref())?
            .ok_or_else(|| Error::ClaimRequired {
                item: args.item.clone(),
            })?;
        app.store.release(
            &args.item,
            &token,
            args.reason.as_deref(),
            &actor,
            app.at,
            None,
        )?
    };
    if let Some(session) = &app.globals.session {
        app.tokens.forget(session, &claim.work_item_id);
    }
    let text = format!(
        "released {} (was held by {})",
        claim.identifier, claim.holder
    );
    app.out_ok(json!({ "claim": claim }), text)
}

fn cmd_claims(app: App, args: ClaimsArgs) -> Result<()> {
    let session = args
        .held_by
        .clone()
        .or_else(|| app.globals.session.clone())
        .ok_or_else(|| Error::invalid("claims requires --session / --held-by or APM_SESSION_ID"))?;
    let claims = app
        .store
        .session_claims(&session, args.include_expired, app.at)?;
    let side_run = side_run_mode();
    let tokens = if args.with_tokens && !side_run {
        let stored = app.tokens.load(&session);
        let mut map = std::collections::BTreeMap::new();
        for claim in &claims {
            if let Some(t) = stored
                .iter()
                .find(|t| t.item_id == claim.claim.work_item_id && t.claim_id == claim.claim.id)
            {
                map.insert(claim.claim.work_item_id.clone(), t.token.clone());
            }
        }
        Some(map)
    } else {
        None
    };
    let text = if claims.is_empty() {
        format!("no claims for session {session}")
    } else {
        claims
            .iter()
            .map(|c| {
                format!(
                    "{}  {}  expires {}  {}",
                    c.claim.identifier, c.claim.status, c.claim.expires_at, c.item.title
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    app.out_ok(
        ClaimListing {
            session: Some(session),
            claims,
            tokens,
            tokens_suppressed: side_run && args.with_tokens,
        },
        text,
    )
}

fn cmd_context(mut app: App, cmd: ContextCmd) -> Result<()> {
    match cmd {
        ContextCmd::Show { item, version } => {
            let ctx = app.store.context_at(&item, version)?;
            let text = render_context(&ctx);
            app.out_ok(json!({ "context": ctx }), text)
        }
        ContextCmd::History { item, limit } => {
            let versions = app.store.context_history(&item, limit)?;
            let text = versions
                .iter()
                .map(|v| {
                    format!(
                        "v{}  {}  by {}  {}{}",
                        v.version,
                        v.created_at,
                        v.author,
                        v.reason,
                        v.revises_version
                            .map(|p| format!("  (revises v{p})"))
                            .unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            app.out_ok(json!({ "versions": versions }), text)
        }
        ContextCmd::Revise(args) => {
            let actor = app.actor();
            let token = app.token(&args.item, args.token.as_deref())?;
            let doc = build_context(
                args.goal,
                args.current_state,
                args.constraints,
                args.references,
                args.notes,
                args.context_json,
                args.context_file,
            )?;
            let rev = app.store.revise_context(
                &args.item,
                token.as_deref(),
                args.expect_version,
                args.expect_context_version,
                doc,
                &args.reason,
                &actor,
                app.at,
                args.idempotency_key.as_deref(),
            )?;
            let text = format!(
                "{} context → v{} (item version {})\n  changed: {}\n  reason: {}",
                rev.item.identifier,
                rev.context.version,
                rev.item.version,
                if rev.changed_fields.is_empty() {
                    "-".to_string()
                } else {
                    rev.changed_fields.join(", ")
                },
                rev.context.reason
            );
            app.out_ok(rev, text)
        }
    }
}

fn cmd_label(mut app: App, cmd: LabelCmd) -> Result<()> {
    match cmd {
        LabelCmd::Add { item, label, token } => {
            let actor = app.actor();
            let token = app.token(&item, token.as_deref())?;
            let view = app
                .store
                .add_label(&item, &label, token.as_deref(), &actor, app.at)?;
            app.out_ok(
                json!({ "item": view, "label": label }),
                format!("labels: {}", view.labels.join(", ")),
            )
        }
        LabelCmd::Remove { item, label, token } => {
            let actor = app.actor();
            let token = app.token(&item, token.as_deref())?;
            let view = app
                .store
                .remove_label(&item, &label, token.as_deref(), &actor, app.at)?;
            app.out_ok(
                json!({ "item": view, "label": label }),
                format!("labels: {}", view.labels.join(", ")),
            )
        }
        LabelCmd::List { item } => {
            let detail = app.store.get_item(&item, app.at)?;
            let text = detail.item.labels.join("\n");
            app.out_ok(json!({ "labels": detail.item.labels }), text)
        }
    }
}

fn cmd_dep(mut app: App, cmd: DepCmd) -> Result<()> {
    match cmd {
        DepCmd::Add {
            item,
            depends_on,
            token,
        } => {
            let actor = app.actor();
            let token = app.token(&item, token.as_deref())?;
            let view =
                app.store
                    .add_dependency(&item, &depends_on, token.as_deref(), &actor, app.at)?;
            app.out_ok(
                json!({ "item": view }),
                format!(
                    "{} depends on: {}",
                    view.identifier,
                    render_deps(&view.blocked_by)
                ),
            )
        }
        DepCmd::Remove {
            item,
            depends_on,
            token,
        } => {
            let actor = app.actor();
            let token = app.token(&item, token.as_deref())?;
            let view = app.store.remove_dependency(
                &item,
                &depends_on,
                token.as_deref(),
                &actor,
                app.at,
            )?;
            app.out_ok(
                json!({ "item": view }),
                format!(
                    "{} depends on: {}",
                    view.identifier,
                    render_deps(&view.blocked_by)
                ),
            )
        }
        DepCmd::List { item } => {
            let detail = app.store.get_item(&item, app.at)?;
            let text = format!(
                "blocked by: {}\nblocks: {}",
                render_deps(&detail.item.blocked_by),
                render_deps(&detail.item.blocks)
            );
            app.out_ok(
                json!({ "blocked_by": detail.item.blocked_by, "blocks": detail.item.blocks }),
                text,
            )
        }
    }
}

fn cmd_accept(mut app: App, cmd: AcceptCmd) -> Result<()> {
    match cmd {
        AcceptCmd::Add { item, text, token } => {
            let actor = app.actor();
            let token = app.token(&item, token.as_deref())?;
            let list = app
                .store
                .add_acceptance(&item, &text, token.as_deref(), &actor, app.at)?;
            app.out_ok(json!({ "acceptance": list }), render_acceptance(&list))
        }
        AcceptCmd::Set {
            item,
            seq,
            status,
            note,
            token,
        } => {
            let actor = app.actor();
            let token = app.token(&item, token.as_deref())?;
            let list = app.store.set_acceptance(
                &item,
                seq,
                &status,
                note.as_deref(),
                token.as_deref(),
                &actor,
                app.at,
            )?;
            app.out_ok(json!({ "acceptance": list }), render_acceptance(&list))
        }
        AcceptCmd::List { item } => {
            let detail = app.store.get_item(&item, app.at)?;
            app.out_ok(
                json!({ "acceptance": detail.acceptance }),
                render_acceptance(&detail.acceptance),
            )
        }
    }
}

fn cmd_evidence(mut app: App, cmd: EvidenceCmd) -> Result<()> {
    match cmd {
        EvidenceCmd::Add {
            item,
            kind,
            value,
            note,
            token,
            idempotency_key,
        } => {
            let actor = app.actor();
            let token = app.token(&item, token.as_deref())?;
            let ev = app.store.add_evidence(
                &item,
                &kind,
                &value,
                note.as_deref(),
                token.as_deref(),
                &actor,
                app.at,
                idempotency_key.as_deref(),
            )?;
            app.out_ok(
                json!({ "evidence": ev }),
                format!("evidence {} [{}] {}", ev.id, ev.kind, ev.value),
            )
        }
        EvidenceCmd::List { item } => {
            let detail = app.store.get_item(&item, app.at)?;
            let text = render_evidence(&detail.evidence);
            app.out_ok(json!({ "evidence": detail.evidence }), text)
        }
    }
}

fn cmd_attention(mut app: App, cmd: AttentionCmd) -> Result<()> {
    match cmd {
        AttentionCmd::Request(args) => {
            let token = match &args.item {
                Some(item) => app.token(item, args.token.as_deref())?,
                None => args.token.clone(),
            };
            let req = AttentionRequest {
                item: args.item.clone(),
                question: args.question,
                context: args.context,
                options: args.options,
                recommendation: args.recommend,
                blocking: !args.non_blocking,
            };
            let actor = app.actor();
            let view = app.store.request_attention(
                app.globals.project.as_deref(),
                req,
                token.as_deref(),
                &actor,
                app.at,
                args.idempotency_key.as_deref(),
            )?;
            let text = format!(
                "attention request {} created ({}{})\n  Q: {}\n  reply: apm attention reply {} --response \"...\"",
                view.id,
                if view.blocking {
                    "blocking"
                } else {
                    "non-blocking"
                },
                view.identifier
                    .as_ref()
                    .map(|i| format!(" on {i}"))
                    .unwrap_or_default(),
                view.question,
                view.id
            );
            app.out_ok(json!({ "attention": view }), text)
        }
        AttentionCmd::List {
            item,
            status,
            limit,
        } => {
            let list = app.store.list_attention(
                app.globals.project.as_deref(),
                item.as_deref(),
                status.as_deref(),
                limit,
            )?;
            let text = if list.is_empty() {
                "no attention requests".to_string()
            } else {
                list.iter()
                    .map(|a| {
                        format!(
                            "{}  {:<9} {:<3} {}  {}",
                            a.id,
                            a.status,
                            if a.blocking { "!!" } else { "" },
                            a.identifier.clone().unwrap_or_else(|| "(project)".into()),
                            a.question
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            app.out_ok(json!({ "attention": list }), text)
        }
        AttentionCmd::Reply {
            id,
            response,
            idempotency_key,
        } => {
            let view = app.store.reply_attention(
                &id,
                &response,
                &app.actor(),
                app.at,
                idempotency_key.as_deref(),
            )?;
            let text = format!(
                "attention {} resolved by {}\n  response: {}",
                view.id,
                view.resolved_by.clone().unwrap_or_default(),
                view.response.clone().unwrap_or_default()
            );
            app.out_ok(json!({ "attention": view }), text)
        }
        AttentionCmd::Withdraw { id, token } => {
            let actor = app.actor();
            // Resolve the item this request belongs to, then this session's token
            // for it. Project-level requests need no token.
            let resolved =
                match app.store.attention_target(&id)? {
                    Some((item_id, identifier)) => match token.as_deref() {
                        Some(t) => Some(t.to_string()),
                        None => app.globals.session.as_ref().and_then(|s| {
                            app.tokens.find(s, &item_id, &identifier).map(|t| t.token)
                        }),
                    },
                    None => token.clone(),
                };
            let view = app
                .store
                .withdraw_attention(&id, resolved.as_deref(), &actor, app.at)?;
            app.out_ok(
                json!({ "attention": view }),
                format!("attention {} withdrawn", view.id),
            )
        }
    }
}

fn cmd_dashboard(app: App, args: DashboardArgs) -> Result<()> {
    let opts = DashboardOptions {
        no_progress_after_seconds: args.no_progress_after,
        done_recent_seconds: args.done_recent,
    };
    let dashboard = app
        .store
        .dashboard(app.globals.project.as_deref(), app.at, opts)?;
    let text = render_dashboard(&dashboard);
    app.out_ok(dashboard, text)
}

fn cmd_events(app: App, args: EventsArgs) -> Result<()> {
    let events = match &args.item {
        Some(item) => app.store.item_events(item, args.limit)?,
        None => {
            let project = app.store.require_project(app.globals.project.as_deref())?;
            app.store.project_events(&project.id, args.limit)?
        }
    };
    let text = events
        .iter()
        .map(|e| {
            format!(
                "#{:<4} {:<19} {:<20} {}{}",
                e.id,
                e.kind,
                e.actor,
                e.identifier.clone().unwrap_or_else(|| "-".into()),
                if e.payload.is_null() {
                    String::new()
                } else {
                    format!("  {}", e.payload)
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    app.out_ok(json!({ "events": events }), text)
}

fn cmd_workflow(mut app: App, cmd: Option<WorkflowCmd>) -> Result<()> {
    match cmd {
        None => {
            let wf = app.store.workflow(app.globals.project.as_deref())?;
            let text = wf.to_json()?;
            app.out_ok(wf, text)
        }
        Some(WorkflowCmd::Show(args)) => {
            let wf = if let (Some(id), Some(version)) = (&args.id, args.version) {
                app.store
                    .workflow_by_id(app.globals.project.as_deref(), id, version)?
            } else if args.id.is_some() || args.version.is_some() {
                return Err(Error::invalid("--id and --version must be used together"));
            } else {
                app.store
                    .resolve_workflow(app.globals.project.as_deref(), args.item_type.as_deref())?
            };
            let text = wf.to_json()?;
            app.out_ok(wf, text)
        }
        Some(WorkflowCmd::List) => {
            let list = app.store.list_workflows(app.globals.project.as_deref())?;
            let text = list
                .iter()
                .map(|w| {
                    format!(
                        "{:<20} v{:<4} {:<24} initial={:<12} states={}{}",
                        w.id,
                        w.version,
                        w.name,
                        w.initial,
                        w.states,
                        if w.is_project_default {
                            "  [default]"
                        } else {
                            ""
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            app.out_ok(json!({ "workflows": list }), text)
        }
        Some(WorkflowCmd::Types) => {
            let bindings = app.store.type_workflows(app.globals.project.as_deref())?;
            let default = app.store.workflow(app.globals.project.as_deref())?;
            let text = bindings
                .iter()
                .map(|b| {
                    format!(
                        "{:<12} {} v{}",
                        b.item_type, b.workflow_id, b.workflow_version
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = if text.is_empty() {
                format!(
                    "no explicit bindings; default is {} v{}",
                    default.id, default.version
                )
            } else {
                format!("{text}\ndefault: {} v{}", default.id, default.version)
            };
            app.out_ok(
                json!({
                    "bindings": bindings,
                    "default": { "workflow_id": default.id, "workflow_version": default.version },
                }),
                text,
            )
        }
        Some(WorkflowCmd::Validate(_args)) => {
            // Validate is pure: no project or database is required.
            unreachable!("validate is handled before the store is opened")
        }
        Some(WorkflowCmd::Set(args)) => {
            let (raw, source) = read_workflow_definition(&args.def)?;
            let workflow = Workflow::from_json(&raw)?;
            let outcome = app.store.apply_workflow_config(
                app.globals.project.as_deref(),
                workflow,
                &args.bind,
                args.default,
                args.dry_run,
                &app.actor(),
                app.at,
            )?;
            let text = if outcome.dry_run {
                format!(
                    "valid {} v{} ({} states, initial {}); would {} and bind {} type(s){}",
                    outcome.workflow.id,
                    outcome.workflow.version,
                    outcome.workflow.states.len(),
                    outcome.workflow.initial,
                    if outcome.created { "register" } else { "reuse" },
                    outcome.bound.len(),
                    if args.default {
                        " and make default"
                    } else {
                        ""
                    }
                )
            } else {
                format!(
                    "{} {} v{} ({} states, initial {}){}{}",
                    if outcome.created {
                        "registered"
                    } else {
                        "reused"
                    },
                    outcome.workflow.id,
                    outcome.workflow.version,
                    outcome.workflow.states.len(),
                    outcome.workflow.initial,
                    if outcome.bound.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "; bound {}",
                            outcome
                                .bound
                                .iter()
                                .map(|b| b.item_type.clone())
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                    if outcome.made_default {
                        "; project default"
                    } else {
                        ""
                    }
                )
            };
            app.out_ok(
                json!({
                    "workflow": outcome.workflow,
                    "created": outcome.created,
                    "bound": outcome.bound,
                    "made_default": outcome.made_default,
                    "dry_run": outcome.dry_run,
                    "source": source,
                }),
                text,
            )
        }
        Some(WorkflowCmd::Bind(args)) => {
            let binding = app.store.bind_type_workflow(
                app.globals.project.as_deref(),
                &args.item_type,
                &args.id,
                args.version,
                &app.actor(),
                app.at,
            )?;
            let text = format!(
                "{} -> {} v{}",
                binding.item_type, binding.workflow_id, binding.workflow_version
            );
            app.out_ok(json!({ "binding": binding }), text)
        }
        Some(WorkflowCmd::Upgrade(args)) => {
            let target_id = match args.id {
                Some(id) => id,
                None => {
                    let current = app
                        .store
                        .resolve_workflow(app.globals.project.as_deref(), Some(&args.item_type))?;
                    current.id
                }
            };
            let mut mappings = std::collections::BTreeMap::new();
            for raw in &args.maps {
                let Some((from, to)) = raw.split_once('=') else {
                    return Err(Error::invalid(format!(
                        "invalid --map '{raw}': expected old=new"
                    )));
                };
                let from = StateKey::parse(from)?.into_string();
                let to = StateKey::parse(to)?.into_string();
                mappings.insert(from, to);
            }
            let report = app.store.upgrade_type_workflow(
                app.globals.project.as_deref(),
                &args.item_type,
                &target_id,
                args.to_version,
                &mappings,
                args.migrate_items,
                args.dry_run,
                &app.actor(),
                app.at,
            )?;
            let mut lines = vec![format!(
                "{} -> {} v{}{}  migrated={} skipped={} blocked={}",
                report.item_type,
                report.target.workflow_id,
                report.target.workflow_version,
                if report.dry_run { " (dry run)" } else { "" },
                report.migrated,
                report.skipped,
                report.blocked
            )];
            for item in &report.items {
                lines.push(format!(
                    "  {:<10} {:<12} -> {:<12} {}",
                    item.identifier,
                    item.from_state,
                    item.to_state.clone().unwrap_or_else(|| "-".into()),
                    item.action
                ));
            }
            app.out_ok(report, lines.join("\n"))
        }
    }
}

fn read_workflow_definition(def: &WorkflowDefArgs) -> Result<(String, String)> {
    match (&def.file, &def.definition_json) {
        (Some(path), None) => {
            let raw = std::fs::read_to_string(path).map_err(|e| {
                Error::invalid(format!("cannot read workflow file {}: {e}", path.display()))
            })?;
            Ok((raw, path.display().to_string()))
        }
        (None, Some(json)) => Ok((json.clone(), "--definition-json".to_string())),
        (None, None) => Err(Error::invalid(
            "provide the workflow document with --file <path> or --definition-json <json>",
        )),
        (Some(_), Some(_)) => Err(Error::invalid(
            "--file and --definition-json are mutually exclusive",
        )),
    }
}

/// Pure validation: no store, no project, safe to run in CI or a pre-commit hook.
fn cmd_workflow_validate(args: &WorkflowDefArgs, json_mode: bool) -> Result<()> {
    let (raw, source) = read_workflow_definition(args)?;
    let workflow = Workflow::from_json(&raw)?;
    let payload = json!({
        "valid": true,
        "source": source,
        "workflow": workflow,
        "states": workflow.states.len(),
        "transitions": workflow.transitions.len(),
        "guards": workflow.guards.keys().cloned().collect::<Vec<_>>(),
    });
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "valid: {} v{} ({} states, {} transitions, {} guards, initial {})",
            workflow.id,
            workflow.version,
            workflow.states.len(),
            workflow.transitions.len(),
            workflow.guards.len(),
            workflow.initial
        );
    }
    Ok(())
}

fn cmd_expire(mut app: App) -> Result<()> {
    let expired = app.store.expire_claims(app.at)?;
    let text = if expired.is_empty() {
        "no expired claims".to_string()
    } else {
        expired
            .iter()
            .map(|c| format!("expired {} ({})", c.identifier, c.holder))
            .collect::<Vec<_>>()
            .join("\n")
    };
    app.out_ok(json!({ "expired": expired, "count": expired.len() }), text)
}

fn cmd_doctor(app: App) -> Result<()> {
    let report = app.store.doctor(app.globals.project.as_deref(), app.at)?;
    let mut lines = vec![
        format!("db: {}", report.db_path),
        format!(
            "project: {}  open items: {}  active claims: {}  open attention: {}",
            report.project.clone().unwrap_or_default(),
            report.open_items,
            report.active_claims,
            report.open_attention
        ),
        format!("workflow valid: {}", report.workflow_valid),
    ];
    if report.expired_unreleased.is_empty() {
        lines.push("expired claims: none".into());
    } else {
        lines.push(format!(
            "expired claims still marked active: {}",
            report
                .expired_unreleased
                .iter()
                .map(|c| c.identifier.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !report.pending_diagnostics.is_empty() {
        lines.push(format!(
            "unresolved side-run diagnostics: {}",
            report.pending_diagnostics.len()
        ));
        for ev in &report.pending_diagnostics {
            lines.push(format!(
                "  #{} {} {} {}",
                ev.id,
                ev.identifier.clone().unwrap_or_else(|| "-".into()),
                ev.payload
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                ev.payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
            ));
        }
    }
    for w in &report.warnings {
        lines.push(format!("warning: {w}"));
    }
    app.out_ok(report, lines.join("\n"))
}

fn cmd_backup(app: App, args: BackupArgs) -> Result<()> {
    let dest = match args.out {
        Some(path) => path,
        None => {
            let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
            std::env::current_dir()
                .map_err(|e| Error::Internal(format!("cannot read cwd: {e}")))?
                .join(format!("apm-backup-{stamp}.db"))
        }
    };
    let report = app.store.backup_to(&dest)?;
    app.out_ok(
        json!({ "backup": report }),
        format!(
            "backup: {} ({} items, {} events, {} bytes)",
            report.path, report.items, report.events, report.bytes
        ),
    )
}

fn cmd_export(app: App, args: ExportArgs) -> Result<()> {
    let project = app.globals.project.as_deref();
    let Some(path) = args.out else {
        // NDJSON owns stdout: no human/JSON envelope here, or the stream would
        // stop being one JSON document per line.
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        app.store.export_ndjson(project, &mut lock)?;
        return Ok(());
    };
    if path.exists() {
        return Err(Error::invalid(format!(
            "export destination already exists: {} (refusing to overwrite)",
            path.display()
        )));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Internal(format!("cannot create {}: {e}", parent.display())))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|e| Error::Internal(format!("cannot create {}: {e}", path.display())))?;
    let mut writer = std::io::BufWriter::new(file);
    let report = match app.store.export_ndjson(project, &mut writer) {
        Ok(report) => report,
        Err(err) => {
            // Do not leave a half-written audit file behind.
            drop(writer);
            let _ = std::fs::remove_file(&path);
            return Err(err);
        }
    };
    use std::io::Write;
    writer
        .flush()
        .map_err(|e| Error::Internal(format!("cannot flush {}: {e}", path.display())))?;
    app.out_ok(
        json!({ "export": report }),
        format!(
            "export: {} ({} records, {} bytes)",
            path.display(),
            report.records,
            report.bytes
        ),
    )
}

fn cmd_diag(mut app: App, args: DiagArgs) -> Result<()> {
    let ev = app.store.diagnose(
        app.globals.project.as_deref(),
        args.item.as_deref(),
        &app.actor(),
        &args.code,
        &args.message,
        args.idempotency_key.as_deref(),
    )?;
    app.out_ok(
        json!({ "event": ev }),
        format!(
            "diagnostic recorded: {} {}",
            args.code,
            args.item.unwrap_or_else(|| "(project)".into())
        ),
    )
}

// ---------------------------------------------------------------------------
// Paths, clocks, rendering
// ---------------------------------------------------------------------------

fn resolve_db(explicit: Option<&Path>, init_mode: bool) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
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
    if init_mode {
        return Ok(cwd.join(".apm").join("apm.db"));
    }
    Err(Error::NotFound {
        entity: "database",
        id: format!(
            "no .apm/apm.db from {} upwards; run `apm init --name <project>` or pass --db",
            cwd.display()
        ),
    })
}

/// The run-end side-run sets APM_SIDE_RUN=1. In that mode the CLI still stores
/// the claim token in the 0600 session file, but never prints it: the child's
/// stdout is fed to a model, and models paste command output into ledgers.
fn side_run_mode() -> bool {
    std::env::var("APM_SIDE_RUN")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn parse_now(raw: &str) -> Result<i64> {
    if let Ok(ms) = raw.parse::<i64>() {
        return Ok(ms);
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp_millis())
        .map_err(|e| Error::invalid(format!("--now must be epoch millis or RFC3339: {e}")))
}

fn render_row(item: &WorkItemView) -> String {
    format!(
        "{:<8} {:<9} {:<10} {:<38} hb:{:<20} prog:{:<20} next:{}",
        item.identifier,
        item.status.as_str(),
        item.health.as_str(),
        truncate(&item.title, 38),
        item.last_heartbeat_at.clone().unwrap_or_else(|| "-".into()),
        item.last_progress_at.clone().unwrap_or_else(|| "-".into()),
        truncate(item.next_action.as_deref().unwrap_or("-"), 40)
    )
}

fn render_detail(d: &ItemDetail) -> String {
    let i = &d.item;
    let mut out = vec![
        format!(
            "{}  {}  [{} / {}]",
            i.identifier,
            i.title,
            i.status.as_str(),
            i.health.as_str()
        ),
        format!("  id:          {}", i.id),
        format!(
            "  version:     {} (context v{})",
            i.version, i.context_version
        ),
        format!(
            "  parent:      {}",
            i.parent_identifier.clone().unwrap_or_else(|| "-".into())
        ),
        format!("  priority:    {}", i.priority),
        format!("  labels:      {}", i.labels.join(", ")),
        format!("  blocked by:  {}", render_deps(&i.blocked_by)),
        format!("  blocks:      {}", render_deps(&i.blocks)),
        format!(
            "  claim:       {}",
            i.claim
                .as_ref()
                .map(|c| format!(
                    "{} via {} session {} (expires {})",
                    c.holder,
                    short_id(&c.id),
                    c.session_id.clone().unwrap_or_else(|| "-".into()),
                    c.expires_at
                ))
                .unwrap_or_else(|| "unclaimed".into())
        ),
        format!(
            "  heartbeat:   {}",
            i.last_heartbeat_at.clone().unwrap_or_else(|| "-".into())
        ),
        format!(
            "  progress at: {}",
            i.last_progress_at.clone().unwrap_or_else(|| "-".into())
        ),
        format!(
            "  run sync at: {}",
            i.last_run_sync_at.clone().unwrap_or_else(|| "-".into())
        ),
        format!(
            "  last run:    {}",
            i.last_run
                .as_ref()
                .map(|r| format!(
                    "{} at {} (run {})",
                    r.outcome,
                    r.at,
                    r.run_id.clone().unwrap_or_else(|| "-".into())
                ))
                .unwrap_or_else(|| "-".into())
        ),
        format!(
            "  progress:    {}",
            i.progress_summary.clone().unwrap_or_else(|| "-".into())
        ),
        format!(
            "  next:        {}",
            i.next_action.clone().unwrap_or_else(|| "-".into())
        ),
        format!(
            "  blocking:    {}",
            i.blocker.clone().unwrap_or_else(|| "-".into())
        ),
        format!(
            "  waiting:     {}{}",
            if i.waiting_on_human { "human" } else { "-" },
            i.waiting_reason
                .as_ref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ),
        String::new(),
        "context:".to_string(),
        render_context(&d.context),
        String::new(),
        "acceptance:".to_string(),
        render_acceptance(&d.acceptance),
        String::new(),
        "evidence:".to_string(),
        render_evidence(&d.evidence),
    ];
    if !d.attention.is_empty() {
        out.push(String::new());
        out.push("attention:".into());
        for a in &d.attention {
            out.push(format!(
                "  {} {}{}  {}\n     {}",
                a.id,
                a.status,
                if a.blocking { " (blocking)" } else { "" },
                a.question,
                a.context
            ));
        }
    }
    out.push(String::new());
    out.push(format!("audit events: {}", d.history_len));
    out.join("\n")
}

fn render_context(ctx: &ContextVersionView) -> String {
    let doc = &ctx.doc;
    let mut out = vec![
        format!(
            "v{}  by {}  {}  reason: {}",
            ctx.version, ctx.author, ctx.created_at, ctx.reason
        ),
        format!("  goal:          {}", doc.goal),
        format!("  current state: {}", doc.current_state),
    ];
    if !doc.constraints.is_empty() {
        out.push(format!("  constraints:   {}", doc.constraints.join("; ")));
    }
    if !doc.references.is_empty() {
        out.push(format!("  references:    {}", doc.references.join("; ")));
    }
    if let Some(notes) = &doc.notes {
        out.push(format!("  notes:         {notes}"));
    }
    out.push(format!("  hash:          {}", ctx.doc_hash));
    out.join("\n")
}

fn render_deps(deps: &[DependencyRef]) -> String {
    if deps.is_empty() {
        "-".to_string()
    } else {
        deps.iter()
            .map(|d| format!("{} ({})", d.identifier, d.status.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn render_acceptance(list: &[AcceptanceView]) -> String {
    if list.is_empty() {
        return "  (none)".to_string();
    }
    list.iter()
        .map(|a| {
            format!(
                "  {}  {}  {}{}",
                a.seq,
                a.status,
                a.text,
                a.note
                    .as_ref()
                    .map(|n| format!("  [{n}]"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_evidence(list: &[EvidenceView]) -> String {
    if list.is_empty() {
        return "  (none)".to_string();
    }
    list.iter()
        .map(|e| {
            format!(
                "  [{}] {}  {}  {}",
                e.kind, e.value, e.created_by, e.created_at
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_dashboard(d: &Dashboard) -> String {
    let mut out = vec![format!(
        "{}  {}  (stale>{ }s  no-progress>{}s)",
        d.project,
        d.generated_at,
        d.settings.stale_after_seconds,
        d.settings.no_progress_after_seconds
    )];
    let sections: [(&str, &Vec<WorkItemView>); 7] = [
        ("WAITING FOR HUMAN", &d.buckets.waiting_human),
        ("LOST LEASE / AT RISK", &d.buckets.lost_lease),
        ("RECOVERY FAILED", &d.buckets.recovery_failed),
        ("NO SUBSTANTIVE PROGRESS", &d.buckets.stalled_no_progress),
        ("WAITING ON DEPENDENCIES", &d.buckets.waiting_dependencies),
        ("IN PROGRESS", &d.buckets.in_progress),
        ("RECENTLY DONE", &d.buckets.recently_done),
    ];
    for (title, items) in sections {
        out.push(String::new());
        out.push(format!("{title} ({})", items.len()));
        if items.is_empty() {
            out.push("  -".into());
        }
        for item in items {
            out.push(format!(
                "  {:<8} {:<9} {:<10} {}{}",
                item.identifier,
                item.status.as_str(),
                item.health.as_str(),
                truncate(&item.title, 44),
                if item.waiting_on_human || !item.attention.is_empty() {
                    "  [human]"
                } else {
                    ""
                }
            ));
            if let Some(p) = &item.progress_summary {
                out.push(format!("      progress: {}", truncate(p, 90)));
            }
            if let Some(n) = &item.next_action {
                out.push(format!("      next:     {}", truncate(n, 90)));
            }
            out.push(format!(
                "      hb {}  progress {}  run-sync {}{}",
                item.last_heartbeat_at.clone().unwrap_or_else(|| "-".into()),
                item.last_progress_at.clone().unwrap_or_else(|| "-".into()),
                item.last_run_sync_at.clone().unwrap_or_else(|| "-".into()),
                item.last_run
                    .as_ref()
                    .map(|r| format!("  last-run {}({})", r.outcome, r.at))
                    .unwrap_or_default()
            ));
        }
    }
    // ready/unclaimed is the "pick me up" queue; keep it last and terse.
    out.push(String::new());
    out.push(format!(
        "READY / UNCLAIMED ({})",
        d.buckets.ready_unclaimed.len()
    ));
    for item in &d.buckets.ready_unclaimed {
        out.push(format!(
            "  {:<8} {:<9} {}",
            item.identifier,
            item.status.as_str(),
            truncate(&item.title, 60)
        ));
    }
    out.join("\n")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}
