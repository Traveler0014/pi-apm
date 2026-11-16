//! CLI contract tests: exit codes, JSON error envelope, session token reuse, and
//! the side-run guard. These spawn the real binary (`CARGO_BIN_EXE_apm`).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

struct Cli {
    db: PathBuf,
    dir: tempfile::TempDir,
}

struct Out {
    code: i32,
    json: Value,
    stdout: String,
    stderr: String,
}

impl Cli {
    fn new() -> Cli {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(".apm").join("apm.db");
        Cli { db, dir }
    }

    fn dir(&self) -> &Path {
        self.dir.path()
    }

    fn run_with(&self, args: &[&str], env: &[(&str, &str)]) -> Out {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_apm"));
        cmd.args(args);
        cmd.env("APM_DB", &self.db);
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
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let code = out.status.code().unwrap_or(-1);
        let payload = stdout.lines().next().map(|_| stdout.as_str()).unwrap_or("");
        let json = serde_json::from_str::<Value>(payload).unwrap_or_else(|_| {
            // Error envelopes go to stderr.
            serde_json::from_str::<Value>(&stderr).unwrap_or(Value::Null)
        });
        Out {
            code,
            json,
            stdout,
            stderr,
        }
    }

    fn run(&self, args: &[&str]) -> Out {
        self.run_with(args, &[])
    }

    fn init(&self) {
        let out = self.run(&["init", "--name", "CLI Test", "--prefix", "TST", "--json"]);
        assert_eq!(out.code, 0, "init failed: {}", out.stderr);
    }

    fn create(&self, title: &str, extra: &[&str]) -> String {
        let mut args = vec![
            "create",
            "--title",
            title,
            "--goal",
            "ship it",
            "--current-state",
            "nothing done yet",
            "--json",
        ];
        args.extend_from_slice(extra);
        let out = self.run(&args);
        assert_eq!(out.code, 0, "create failed: {} {}", out.stdout, out.stderr);
        out.json["item"]["identifier"].as_str().unwrap().to_string()
    }
}

#[test]
fn end_to_end_agent_flow_with_session_token_reuse() {
    let cli = Cli::new();
    cli.init();

    let out = cli.run(&[
        "create",
        "--title",
        "Implement CLI",
        "--goal",
        "agents drive apm from bash",
        "--current-state",
        "library done",
        "--acceptance",
        "e2e passes",
        "--json",
    ]);
    assert_eq!(out.code, 0);
    let identifier = out.json["item"]["identifier"].as_str().unwrap().to_string();
    assert_eq!(out.json["item"]["version"], 1);
    assert_eq!(out.json["item"]["health"], "unclaimed");

    // Claim persists the token for the session, so later writes need no --token.
    let out = cli.run_with(
        &["claim", &identifier, "--json"],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-1")],
    );
    assert_eq!(out.code, 0);
    assert!(out.json["token"].as_str().unwrap().starts_with("apm_"));
    assert_eq!(out.json["session_token_saved"], true);

    let out = cli.run_with(
        &[
            "status",
            &identifier,
            "--to",
            "doing",
            "--expect-version",
            "1",
            "--json",
        ],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-1")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["item"]["status"], "doing");

    let out = cli.run_with(
        &[
            "progress",
            &identifier,
            "--expect-version",
            "2",
            "--summary",
            "CLI wired",
            "--next-action",
            "write docs",
            "--json",
        ],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-1")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["item"]["progress_summary"], "CLI wired");

    // A different session cannot write.
    let out = cli.run_with(
        &[
            "progress",
            &identifier,
            "--expect-version",
            "3",
            "--summary",
            "hijack",
            "--json",
        ],
        &[("APM_AGENT", "other"), ("APM_SESSION_ID", "sess-2")],
    );
    // Session 2 has no token for this item and no claim: claim_required.
    assert_eq!(out.code, 5);
    assert_eq!(out.json["error"]["code"], "claim_required");

    // A live claim by session 1 blocks session 2 from claiming at all.
    let out = cli.run_with(
        &["claim", &identifier, "--json"],
        &[("APM_AGENT", "other"), ("APM_SESSION_ID", "sess-2")],
    );
    assert_eq!(out.code, 5);
    assert_eq!(out.json["error"]["code"], "claim_conflict");

    let out = cli.run_with(&["dashboard", "--json"], &[("APM_AGENT", "codex")]);
    assert_eq!(out.code, 0);
    assert!(
        out.json["buckets"]["in_progress"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["identifier"] == identifier)
    );

    let out = cli.run(&["events", &identifier, "--json"]);
    let kinds: Vec<String> = out.json["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"item_created".to_string()));
    assert!(kinds.contains(&"claimed".to_string()));
    assert!(kinds.contains(&"status_changed".to_string()));
    assert!(kinds.contains(&"progress_recorded".to_string()));
}

#[test]
fn missing_context_and_title_are_rejected_with_codes() {
    let cli = Cli::new();
    cli.init();

    let out = cli.run(&["create", "--title", "no context", "--json"]);
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "missing_context");

    let out = cli.run(&[
        "create",
        "--title",
        " ",
        "--goal",
        "g",
        "--current-state",
        "c",
        "--json",
    ]);
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "invalid_input");
}

#[test]
fn double_claim_and_forged_token_are_rejected() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Mutex", &[]);

    let out = cli.run_with(
        &["claim", &id, "--json"],
        &[("APM_AGENT", "agent-a"), ("APM_SESSION_ID", "sess-a")],
    );
    assert_eq!(out.code, 0);

    let out = cli.run_with(
        &["claim", &id, "--json"],
        &[("APM_AGENT", "agent-b"), ("APM_SESSION_ID", "sess-b")],
    );
    assert_eq!(out.code, 5);
    assert_eq!(out.json["error"]["code"], "claim_conflict");

    let out = cli.run_with(
        &[
            "progress",
            &id,
            "--expect-version",
            "1",
            "--summary",
            "forged",
            "--token",
            "apm_0000000000000000",
            "--json",
        ],
        &[("APM_AGENT", "agent-b"), ("APM_SESSION_ID", "sess-b")],
    );
    assert_eq!(out.code, 5);
    assert_eq!(out.json["error"]["code"], "claim_invalid");
}

#[test]
fn expired_lease_blocks_cli_writes_and_humans_can_force_release() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Lease", &[]);

    let out = cli.run_with(
        &[
            "claim",
            &id,
            "--lease",
            "60",
            "--now",
            "1700000000000",
            "--json",
        ],
        &[("APM_AGENT", "agent-a"), ("APM_SESSION_ID", "sess-a")],
    );
    assert_eq!(out.code, 0);

    let out = cli.run_with(
        &[
            "progress",
            &id,
            "--expect-version",
            "1",
            "--summary",
            "too late",
            "--now",
            "1700000061000",
            "--json",
        ],
        &[("APM_AGENT", "agent-a"), ("APM_SESSION_ID", "sess-a")],
    );
    assert_eq!(out.code, 5);
    assert_eq!(out.json["error"]["code"], "claim_invalid");
    assert!(
        out.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("lease expired")
    );

    let out = cli.run_with(
        &[
            "release",
            &id,
            "--force",
            "--reason",
            "holder is gone",
            "--now",
            "1700000062000",
            "--json",
        ],
        &[("APM_AGENT", "helper"), ("APM_SESSION_ID", "sess-helper")],
    );
    assert_eq!(out.code, 7, "agents may not force-release");
    assert_eq!(out.json["error"]["code"], "forbidden");

    let out = cli.run_with(
        &[
            "--human",
            "release",
            &id,
            "--force",
            "--reason",
            "holder is gone",
            "--now",
            "1700000062000",
            "--json",
        ],
        &[("APM_AGENT", "alice")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["claim"]["status"], "released");
}

#[test]
fn side_run_env_cannot_claim_new_work() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Recovery only", &[]);

    let out = cli.run_with(
        &["claim", &id, "--json"],
        &[
            ("APM_AGENT", "side-run"),
            ("APM_SESSION_ID", "sess-main"),
            ("APM_SIDE_RUN", "1"),
        ],
    );
    assert_eq!(out.code, 7);
    assert_eq!(out.json["error"]["code"], "forbidden");
    assert!(
        out.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("side-run may only recover")
    );
}

#[test]
fn aborted_run_sync_is_visible_and_idempotent() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Aborted", &[]);

    let out = cli.run_with(
        &["claim", &id, "--json"],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-abort")],
    );
    assert_eq!(out.code, 0);

    let args = [
        "sync",
        &id,
        "--outcome",
        "aborted",
        "--note",
        "interrupted by user",
        "--idempotency-key",
        "sr:sess-abort:run-7:item:sync",
        "--json",
    ];
    let first = cli.run_with(
        &args,
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-abort")],
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.json["run"]["outcome"], "aborted");
    assert!(first.json["last_progress_at"].is_null());
    assert_eq!(first.json["version"], 1, "sync must not bump the version");

    let second = cli.run_with(
        &args,
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-abort")],
    );
    assert_eq!(second.code, 0);
    assert_eq!(second.json["run"]["at"], first.json["run"]["at"]);

    let out = cli.run(&["events", &id, "--json"]);
    let aborted = out.json["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "run_aborted")
        .count();
    assert_eq!(aborted, 1, "retry must not duplicate the event");

    let out = cli.run(&["get", &id, "--json"]);
    assert_eq!(out.json["item"]["last_run"]["outcome"], "aborted");
    assert_eq!(out.json["item"]["status"], "ready");
    assert!(out.json["item"]["last_progress_at"].is_null());
}

#[test]
fn attention_request_and_reply_via_cli() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Ask a human", &[]);

    cli.run_with(
        &["claim", &id, "--json"],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-attn")],
    );

    let out = cli.run_with(
        &[
            "attention",
            "request",
            "--item",
            &id,
            "--question",
            "Deploy to prod?",
            "--context",
            "Staging is green; prod requires an approval.",
            "--option",
            "yes",
            "--option",
            "no",
            "--recommend",
            "yes",
            "--json",
        ],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-attn")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    let attention_id = out.json["attention"]["id"].as_str().unwrap().to_string();
    assert_eq!(out.json["attention"]["blocking"], true);

    let out = cli.run(&["dashboard", "--json"]);
    assert!(
        out.json["buckets"]["waiting_human"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["identifier"] == id)
    );

    let out = cli.run_with(
        &[
            "--human",
            "attention",
            "reply",
            &attention_id,
            "--response",
            "yes, go ahead",
            "--json",
        ],
        &[("APM_AGENT", "alice")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["attention"]["status"], "resolved");
    assert_eq!(out.json["attention"]["response"], "yes, go ahead");

    let out = cli.run(&["attention", "list", "--json"]);
    assert_eq!(out.json["attention"].as_array().unwrap().len(), 1);
}

#[test]
fn dashboard_and_doctor_json_are_parseable() {
    let cli = Cli::new();
    cli.init();
    cli.create("Something", &["--label", "pipeline"]);

    let out = cli.run(&["dashboard", "--json"]);
    assert_eq!(out.code, 0);
    for key in [
        "waiting_human",
        "in_progress",
        "stalled_no_progress",
        "lost_lease",
        "waiting_dependencies",
        "recently_done",
        "ready_unclaimed",
        "recovery_failed",
    ] {
        assert!(out.json["buckets"][key].is_array(), "missing bucket {key}");
    }

    let out = cli.run(&["doctor", "--json"]);
    assert_eq!(out.code, 0);
    assert_eq!(out.json["workflow_valid"], true);

    // Discovery from a subdirectory works via the .apm directory walk.
    let sub = cli.dir().join("nested").join("deeper");
    std::fs::create_dir_all(&sub).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_apm"))
        .args(["list", "--json"])
        .current_dir(&sub)
        .env_remove("APM_DB")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["count"], 1);
}

#[test]
fn repeated_progress_is_a_noop_via_cli() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("No fake progress", &[]);
    cli.run_with(
        &["claim", &id, "--lease", "3600", "--json"],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-progress")],
    );

    let args = [
        "progress",
        &id,
        "--expect-version",
        "1",
        "--summary",
        "implemented the parser",
        "--next-action",
        "write tests",
        "--json",
    ];
    let first = cli.run_with(
        &args,
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-progress")],
    );
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.json["substantive"], true);
    assert_eq!(first.json["no_op"], false);
    let progress_at = first.json["item"]["last_progress_at"].clone();

    let second = cli.run_with(
        &[
            "progress",
            &id,
            "--expect-version",
            "2",
            "--summary",
            "implemented the parser",
            "--next-action",
            "write tests",
            "--json",
        ],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-progress")],
    );
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.json["no_op"], true);
    assert_eq!(second.json["substantive"], false);
    assert_eq!(second.json["item"]["version"], 2, "no version churn");
    assert_eq!(second.json["item"]["last_progress_at"], progress_at);

    let events = cli.run(&["events", &id, "--json"]);
    let recorded = events.json["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "progress_recorded")
        .count();
    assert_eq!(recorded, 1);

    // Bookkeeping-only change: writes, but does not refresh the progress clock.
    let third = cli.run_with(
        &[
            "progress",
            &id,
            "--expect-version",
            "2",
            "--summary",
            "implemented the parser",
            "--next-action",
            "write tests and docs",
            "--json",
        ],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-progress")],
    );
    assert_eq!(third.code, 0, "{}", third.stderr);
    assert_eq!(third.json["no_op"], false);
    assert_eq!(third.json["substantive"], false);
    assert_eq!(third.json["item"]["last_progress_at"], progress_at);
}

#[test]
fn claim_idempotency_never_returns_a_second_token() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Idempotent claim", &[]);
    let args = ["claim", &id, "--idempotency-key", "claim-once", "--json"];
    let first = cli.run_with(&args, &[("APM_AGENT", "codex")]);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert!(first.json["token"].as_str().unwrap().starts_with("apm_"));
    assert_eq!(first.json["replayed"], false);

    let second = cli.run_with(&args, &[("APM_AGENT", "codex")]);
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.json["replayed"], true);
    assert!(
        second.json["token"].is_null(),
        "replay must not fabricate a token"
    );
    assert_eq!(second.json["claim"]["id"], first.json["claim"]["id"]);
}

#[test]
fn idempotency_keys_are_bound_to_the_item_via_cli() {
    let cli = Cli::new();
    cli.init();
    let a = cli.create("First", &[]);
    let b = cli.create("Second", &[]);
    let out = cli.run_with(
        &["claim", &a, "--idempotency-key", "shared", "--json"],
        &[("APM_AGENT", "codex")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    let out = cli.run_with(
        &["claim", &b, "--idempotency-key", "shared", "--json"],
        &[("APM_AGENT", "codex")],
    );
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "idempotency_conflict");
}

#[test]
fn empty_update_is_rejected_instead_of_burning_a_version() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Nothing to change", &[]);
    cli.run_with(
        &["claim", &id, "--json"],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-empty")],
    );
    let out = cli.run_with(
        &[
            "update",
            &id,
            "--expect-version",
            "1",
            "--priority",
            "normal",
            "--json",
        ],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-empty")],
    );
    assert_eq!(out.code, 2);
    assert_eq!(out.json["error"]["code"], "invalid_input");
    let out = cli.run(&["get", &id, "--json"]);
    assert_eq!(out.json["item"]["version"], 1);
}

#[test]
fn agents_cannot_answer_attention_requests() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Human decision", &[]);
    cli.run_with(
        &["claim", &id, "--json"],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-attn2")],
    );
    let out = cli.run_with(
        &[
            "attention",
            "request",
            "--item",
            &id,
            "--question",
            "Ship tonight?",
            "--context",
            "CI is green; on-call is staffed.",
            "--json",
        ],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-attn2")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    let attention_id = out.json["attention"]["id"].as_str().unwrap().to_string();

    let out = cli.run_with(
        &[
            "attention",
            "reply",
            &attention_id,
            "--response",
            "yes",
            "--json",
        ],
        &[("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-attn2")],
    );
    assert_eq!(out.code, 7);
    assert_eq!(out.json["error"]["code"], "forbidden");
}

/// True when the text contains a claim token (`apm_` + 32+ hex chars).
fn contains_claim_token(text: &str) -> bool {
    let mut rest = text;
    while let Some(pos) = rest.find("apm_") {
        let after = &rest[pos + 4..];
        let hex_len = after.chars().take_while(|c| c.is_ascii_hexdigit()).count();
        if hex_len >= 32 {
            return true;
        }
        rest = after;
    }
    false
}

#[test]
fn side_run_claim_never_prints_a_token_but_still_authorises_writes() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Side-run claim", &[]);
    let env = [
        ("APM_AGENT", "side-run"),
        ("APM_SESSION_ID", "sess-sr"),
        ("APM_SIDE_RUN", "1"),
    ];

    // A fresh item may not be claimed by a side-run at all (no new work), and
    // even the refusal is token-free.
    let err = cli.run_with(&["claim", &id, "--json"], &env);
    assert_eq!(err.code, 7);
    assert_eq!(err.json["error"]["code"], "forbidden");
    assert!(!contains_claim_token(&err.stdout) && !contains_claim_token(&err.stderr));

    // The legitimate case: the session already holds the item and re-claims to
    // replace a lost token. That is allowed, and stdout must stay token-free.
    let normal = [("APM_AGENT", "side-run"), ("APM_SESSION_ID", "sess-sr")];
    let first = cli.run_with(&["claim", &id, "--json"], &normal);
    assert_eq!(first.code, 0, "{}", first.stderr);
    let out = cli.run_with(&["claim", &id, "--takeover", "--json"], &env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.json["token"].is_null(),
        "side-run stdout must not carry a token"
    );
    assert_eq!(out.json["token_source"], "session_file");
    assert_eq!(out.json["token_suppressed"], true);
    assert_eq!(out.json["session_token_saved"], true);
    assert!(!contains_claim_token(&out.stdout));
    assert!(!contains_claim_token(&out.stderr));

    // Text mode is equally silent.
    let out = cli.run_with(&["claim", &id, "--takeover"], &env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!contains_claim_token(&out.stdout));
    assert!(out.stdout.contains("APM_SIDE_RUN=1"));

    // The write still works without an explicit --token: the CLI reads the file.
    let out = cli.run_with(
        &[
            "progress",
            &id,
            "--expect-version",
            "1",
            "--summary",
            "wrote the ledger from the side-run",
            "--json",
        ],
        &[("APM_AGENT", "side-run"), ("APM_SESSION_ID", "sess-sr")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        out.json["item"]["progress_summary"],
        "wrote the ledger from the side-run"
    );
}

#[test]
fn side_run_recovery_never_prints_a_token() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Recover quietly", &[]);
    let base = [("APM_AGENT", "side-run"), ("APM_SESSION_ID", "sess-rec")];

    let out = cli.run_with(
        &[
            "claim",
            &id,
            "--lease",
            "30",
            "--now",
            "1700000000000",
            "--json",
        ],
        &base,
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.json["token"].as_str().unwrap().starts_with("apm_"));

    let mut env = base.to_vec();
    env.push(("APM_SIDE_RUN", "1"));
    let out = cli.run_with(
        &[
            "claim",
            &id,
            "--recover-only",
            "--takeover",
            "--reason",
            "run-end recovery after lease expiry",
            "--now",
            "1700000061000",
            "--json",
        ],
        &env,
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.json["token"].is_null());
    assert_eq!(out.json["token_suppressed"], true);
    assert!(!contains_claim_token(&out.stdout));
    assert!(!contains_claim_token(&out.stderr));

    // Recovery persisted the new token for the session.
    let out = cli.run_with(
        &[
            "progress",
            &id,
            "--expect-version",
            "1",
            "--summary",
            "recovered and wrote",
            "--now",
            "1700000062000",
            "--json",
        ],
        &base,
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(out.json["item"]["progress_summary"], "recovered and wrote");
}

#[test]
fn side_run_idempotent_claim_replay_never_prints_a_token() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Replay quietly", &[]);
    let env = [
        ("APM_AGENT", "side-run"),
        ("APM_SESSION_ID", "sess-replay"),
        ("APM_SIDE_RUN", "1"),
    ];
    // The session must already own the item (side-runs recover, never discover).
    let normal = [("APM_AGENT", "side-run"), ("APM_SESSION_ID", "sess-replay")];
    let seeded = cli.run_with(&["claim", &id, "--json"], &normal);
    assert_eq!(seeded.code, 0, "{}", seeded.stderr);

    let args = [
        "claim",
        &id,
        "--takeover",
        "--idempotency-key",
        "sr-claim",
        "--json",
    ];

    let first = cli.run_with(&args, &env);
    assert_eq!(first.code, 0, "{}", first.stderr);
    assert_eq!(first.json["replayed"], false);
    assert!(first.json["token"].is_null());
    assert!(!contains_claim_token(&first.stdout));

    let second = cli.run_with(&args, &env);
    assert_eq!(second.code, 0, "{}", second.stderr);
    assert_eq!(second.json["replayed"], true);
    assert!(second.json["token"].is_null());
    assert!(!contains_claim_token(&second.stdout));
    assert!(!contains_claim_token(&second.stderr));
    assert_eq!(second.json["claim"]["id"], first.json["claim"]["id"]);
}

#[test]
fn with_tokens_is_suppressed_for_side_runs_only() {
    let cli = Cli::new();
    cli.init();
    let id = cli.create("Token listing", &[]);
    let session_env = [("APM_AGENT", "codex"), ("APM_SESSION_ID", "sess-list")];

    let out = cli.run_with(&["claim", &id, "--json"], &session_env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    // Ordinary CLI: printing the token once is the documented contract.
    assert!(out.json["token"].as_str().unwrap().starts_with("apm_"));
    let item_id = out.json["claim"]["work_item_id"]
        .as_str()
        .unwrap()
        .to_string();

    let out = cli.run_with(&["claims", "--with-tokens", "--json"], &session_env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.json["tokens"][&item_id]
            .as_str()
            .unwrap()
            .starts_with("apm_")
    );

    let mut side_env = session_env.to_vec();
    side_env.push(("APM_SIDE_RUN", "1"));
    let out = cli.run_with(&["claims", "--with-tokens", "--json"], &side_env);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.json["tokens"].is_null());
    assert_eq!(out.json["tokens_suppressed"], true);
    assert!(!contains_claim_token(&out.stdout));
    assert!(!contains_claim_token(&out.stderr));
}
