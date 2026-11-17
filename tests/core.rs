//! Core invariants. These are the acceptance tests for AgentPM's contract:
//! claim-before-write, versioned context history, three separate clocks,
//! idempotent run sync, evidence-gated completion, and the dashboard views.

mod common;

use apm::store::claims::{ClaimPolicy, ClaimRequest};
use apm::store::items::{NewItem, RunOutcome};
use apm::store::misc::AttentionRequest;
use apm::*;
use common::Fx;

fn event_kinds(store: &Store, item: &str) -> Vec<String> {
    store
        .item_events(item, 500)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

#[test]
fn create_rejects_missing_context_and_empty_title() {
    let mut fx = Fx::new();
    let actor = fx.agent("a", "s");

    // Empty title.
    let err = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "   ".into(),
                context: fx.doc("g", "c"),
                item_type: None,
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec![],
                actor: actor.clone(),
            },
            fx.t0,
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "invalid_input");

    // No goal.
    let err = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "valid".into(),
                context: ContextDoc {
                    goal: "".into(),
                    current_state: "half way".into(),
                    ..Default::default()
                },
                item_type: None,
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec![],
                actor: actor.clone(),
            },
            fx.t0,
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "missing_context");

    // No current state either.
    let err = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "valid".into(),
                context: ContextDoc::default(),
                item_type: None,
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec![],
                actor,
            },
            fx.t0,
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "missing_context");
}

#[test]
fn agent_writes_require_live_claim_and_humans_are_exempt() {
    let mut fx = Fx::new();
    let item = fx.create("Write needs a claim");
    let actor = fx.agent("agent-a", "sess-a");

    // No claim at all.
    let err = fx
        .store
        .record_progress(
            &item.identifier,
            None,
            Some(1),
            "sneaky",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(1),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "claim_required");

    let (_claim, token) = fx.claim(&item.identifier, &actor, 1);

    // Wrong token.
    let err = fx
        .store
        .record_progress(
            &item.identifier,
            Some("apm_forged"),
            Some(1),
            "sneaky",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(2),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "claim_invalid");

    // Correct token.
    let outcome = fx
        .store
        .record_progress(
            &item.identifier,
            Some(&token),
            Some(1),
            "real work",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(3),
            None,
        )
        .unwrap();
    assert!(outcome.substantive);
    assert_eq!(outcome.item.progress_summary.as_deref(), Some("real work"));
    assert_eq!(outcome.item.version, 2);

    // Humans may write without a claim (audited as human).
    let human = fx.human("alice");
    let view = fx
        .store
        .update_item(
            &item.identifier,
            None,
            Some(2),
            ItemPatch {
                priority: Some("high".into()),
                ..Default::default()
            },
            &human,
            fx.at(4),
            None,
        )
        .unwrap();
    assert_eq!(view.priority, "high");
    let last = fx
        .store
        .item_events(&item.identifier, 10)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(last.actor, "alice");
    assert_eq!(last.actor_kind, ActorKind::Human);
}

#[test]
fn expired_lease_blocks_writes_even_with_the_right_token() {
    let mut fx = Fx::new();
    let item = fx.create("Leases expire");
    let actor = fx.agent("agent-a", "sess-a");
    let (_claim, token) = fx.claim(&item.identifier, &actor, 0); // lease = 60s

    // 61s later the lease is gone.
    let err = fx
        .store
        .record_progress(
            &item.identifier,
            Some(&token),
            Some(1),
            "too late",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(61),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "claim_invalid");

    let view = fx.store.get_item(&item.identifier, fx.at(61)).unwrap();
    assert_eq!(view.item.health, Health::Expired);
    assert_eq!(view.item.version, 1, "rejected write must not bump version");

    // Explicit recovery re-acquires the same item.
    let issued = fx
        .store
        .claim(
            &item.identifier,
            &actor,
            ClaimRequest {
                takeover: true,
                reason: Some("my lease expired".into()),
                ..Default::default()
            },
            fx.at(62),
            None,
        )
        .unwrap();
    assert!(issued.takeover);
    assert_eq!(issued.claim.status, "active");
    assert_ne!(issued.token, token, "recovery issues a fresh token");
}

#[test]
fn two_agents_cannot_hold_the_same_item() {
    let mut fx = Fx::new();
    let item = fx.create("Mutex");
    let a = fx.agent("agent-a", "sess-a");
    let b = fx.agent("agent-b", "sess-b");
    fx.claim(&item.identifier, &a, 0);

    let err = fx
        .store
        .claim(
            &item.identifier,
            &b,
            ClaimRequest::default(),
            fx.at(1),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "claim_conflict");

    // Release hands it over.
    let (_c, token) = fx.claim(&item.identifier, &a, 2);
    fx.store
        .release(
            &item.identifier,
            &token,
            Some("done with it"),
            &a,
            fx.at(3),
            None,
        )
        .unwrap();
    let claim = fx
        .store
        .claim(
            &item.identifier,
            &b,
            ClaimRequest::default(),
            fx.at(4),
            None,
        )
        .unwrap();
    assert_eq!(claim.claim.holder, "agent-b");

    // Human force-takeover overrides a live claim.
    let err = fx
        .store
        .claim(
            &item.identifier,
            &a,
            ClaimRequest {
                takeover: true,
                ..Default::default()
            },
            fx.at(5),
            None,
        )
        .unwrap_err();
    assert_eq!(
        err.code(),
        "claim_conflict",
        "agents cannot force a live takeover"
    );
    let human = fx.human("alice");
    let forced = fx
        .store
        .claim(
            &item.identifier,
            &human,
            ClaimRequest {
                takeover: true,
                reason: Some("handing to another agent".into()),
                ..Default::default()
            },
            fx.at(6),
            None,
        )
        .unwrap();
    assert!(forced.takeover);
}

#[test]
fn context_versions_are_traceable_and_optimistically_locked() {
    let mut fx = Fx::new();
    let item = fx.create("Context");
    let actor = fx.agent("agent-a", "sess-a");
    let (_c, token) = fx.claim(&item.identifier, &actor, 0);

    // v2
    let rev = fx
        .store
        .revise_context(
            &item.identifier,
            Some(&token),
            Some(1),
            Some(1),
            fx.doc("graduate", "half way"),
            "state moved forward",
            &actor,
            fx.at(10),
            None,
        )
        .unwrap();
    assert_eq!(rev.context.version, 2);
    assert_eq!(rev.previous_version, Some(1));
    assert!(rev.changed_fields.contains(&"current_state".to_string()));

    // v3, with a different author
    let other = fx.agent("agent-b", "sess-b");
    fx.store
        .revise_context(
            &item.identifier,
            Some(&token),
            Some(2),
            Some(2),
            fx.doc("graduate", "almost there"),
            "run ended after tests",
            &other,
            fx.at(20),
            None,
        )
        .unwrap();

    let history = fx.store.context_history(&item.identifier, 10).unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].version, 3);
    assert_eq!(history[2].version, 1);
    assert_eq!(history[0].reason, "run ended after tests");
    assert_eq!(history[0].author, "agent-b");
    assert_eq!(history[1].revises_version, Some(1));

    // Old snapshots are still readable: history is real, not a diff side-table.
    let v1 = fx.store.context_at(&item.identifier, Some(1)).unwrap();
    assert_eq!(v1.doc.current_state, "not started");

    // No-op revisions are rejected instead of inflating history.
    let err = fx
        .store
        .revise_context(
            &item.identifier,
            Some(&token),
            None,
            None,
            fx.doc("graduate", "almost there"),
            "nothing changed",
            &actor,
            fx.at(30),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "invalid_input");

    // Wrong expected version fails.
    let err = fx
        .store
        .revise_context(
            &item.identifier,
            Some(&token),
            Some(99),
            None,
            fx.doc("graduate", "final"),
            "stale writer",
            &actor,
            fx.at(40),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "version_conflict");
}

#[test]
fn concurrent_writers_never_lose_an_update() {
    let mut fx = Fx::new();
    let item = fx.create("Race");
    let actor = fx.agent("agent-a", "sess-a");
    let (_c, token) = fx.claim(&item.identifier, &actor, 0);

    // Both writers read version 1, like two agents that fetched in parallel.
    let read_version = fx
        .store
        .get_item(&item.identifier, fx.at(1))
        .unwrap()
        .item
        .version;
    assert_eq!(read_version, 1);

    fx.store
        .record_progress(
            &item.identifier,
            Some(&token),
            Some(read_version),
            "writer one",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(2),
            None,
        )
        .unwrap();

    let err = fx
        .store
        .record_progress(
            &item.identifier,
            Some(&token),
            Some(read_version),
            "writer two",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(3),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "version_conflict");
    let current = fx.store.get_item(&item.identifier, fx.at(3)).unwrap().item;
    assert_eq!(current.progress_summary.as_deref(), Some("writer one"));
    assert_eq!(current.version, 2);
}

#[test]
fn run_sync_is_idempotent_per_item_and_never_fakes_progress() {
    let mut fx = Fx::new();
    let a = fx.create("Item A");
    let b = fx.create("Item B");
    let actor = fx.agent("agent-a", "sess-a");
    let (_ca, token_a) = fx.claim(&a.identifier, &actor, 0);
    let (_cb, token_b) = fx.claim(&b.identifier, &actor, 0);

    // One run touches two claimed items; keys are per (run, item, operation).
    let key_a = format!("sr:sess-a:run-1:{}:sync", a.id);
    let key_b = format!("sr:sess-a:run-1:{}:sync", b.id);

    let first_a = fx
        .store
        .sync_run(
            &a.identifier,
            Some(&token_a),
            RunOutcome::Completed,
            Some("tests green"),
            &actor,
            fx.at(10),
            Some(&key_a),
        )
        .unwrap();
    let _first_b = fx
        .store
        .sync_run(
            &b.identifier,
            Some(&token_b),
            RunOutcome::Completed,
            None,
            &actor,
            fx.at(10),
            Some(&key_b),
        )
        .unwrap();
    assert!(
        first_a.last_progress_at.is_none(),
        "sync must not fake progress"
    );
    assert_eq!(first_a.last_run_sync_at, Some(ts(fx.at(10))));

    // Retry the exact same run (crash, hook re-fire, transport retry).
    for _ in 0..2 {
        let again = fx
            .store
            .sync_run(
                &a.identifier,
                Some(&token_a),
                RunOutcome::Completed,
                Some("tests green"),
                &actor,
                fx.at(20),
                Some(&key_a),
            )
            .unwrap();
        assert_eq!(
            again.run.at, first_a.run.at,
            "replay returns the first outcome"
        );
        assert_eq!(again.last_run_sync_at, first_a.last_run_sync_at);
    }

    assert_eq!(
        event_kinds(&fx.store, &a.identifier)
            .iter()
            .filter(|k| *k == "run_synced")
            .count(),
        1,
        "retries must not duplicate run events"
    );
    assert_eq!(
        event_kinds(&fx.store, &b.identifier)
            .iter()
            .filter(|k| *k == "run_synced")
            .count(),
        1,
        "a single run must sync every claimed item"
    );

    let view = fx.store.get_item(&a.identifier, fx.at(11)).unwrap().item;
    assert_eq!(view.health, Health::Active);
    // 20s later the *heartbeat* is stale although the lease is still valid:
    // liveness and substantive progress are independent clocks.
    let later = fx.store.get_item(&a.identifier, fx.at(31)).unwrap().item;
    assert_eq!(later.health, Health::Stale);
    assert!(later.last_progress_at.is_none());
    assert!(view.last_progress_at.is_none());
    assert_eq!(view.last_run.unwrap().outcome, "completed");
}

#[test]
fn aborted_run_is_recorded_without_progress_or_status_change() {
    let mut fx = Fx::new();
    let item = fx.create("Interrupted");
    let actor = fx.agent("agent-a", "sess-a");
    let (_c, token) = fx.claim(&item.identifier, &actor, 0);
    fx.progress(&item.identifier, &token, &actor, 1, "some real progress");
    let before = fx.store.get_item(&item.identifier, fx.at(2)).unwrap().item;

    let out = fx
        .store
        .sync_run(
            &item.identifier,
            Some(&token),
            RunOutcome::Aborted,
            Some("user pressed Escape"),
            &actor,
            fx.at(30),
            Some("sr:run-abort:1"),
        )
        .unwrap();
    assert_eq!(out.run.outcome, "aborted");

    let after = fx.store.get_item(&item.identifier, fx.at(31)).unwrap().item;
    assert_eq!(
        after.status, before.status,
        "abort must not change business status"
    );
    assert_eq!(after.last_progress_at, before.last_progress_at);
    assert_eq!(after.version, before.version);
    assert_eq!(after.last_run.as_ref().unwrap().outcome, "aborted");
    assert_eq!(after.health, Health::Active, "claim is kept, not released");

    let kinds = event_kinds(&fx.store, &item.identifier);
    assert!(kinds.contains(&"run_aborted".to_string()));
    let ev = fx
        .store
        .item_events(&item.identifier, 50)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "run_aborted")
        .unwrap();
    assert_eq!(ev.payload["progress_untouched"], true);

    // Abort sync is idempotent too.
    fx.store
        .sync_run(
            &item.identifier,
            Some(&token),
            RunOutcome::Aborted,
            Some("user pressed Escape"),
            &actor,
            fx.at(40),
            Some("sr:run-abort:1"),
        )
        .unwrap();
    assert_eq!(
        event_kinds(&fx.store, &item.identifier)
            .iter()
            .filter(|k| *k == "run_aborted")
            .count(),
        1
    );

    // And the dashboard can distinguish it from a normal sync.
    let dash = fx
        .store
        .dashboard(Some(&fx.project), fx.at(41), DashboardOptions::default())
        .unwrap();
    let row = dash
        .buckets
        .in_progress
        .iter()
        .chain(dash.buckets.ready_unclaimed.iter())
        .find(|i| i.id == item.id)
        .expect("item visible");
    assert_eq!(row.last_run.as_ref().unwrap().outcome, "aborted");
}

#[test]
fn completion_requires_all_acceptance_or_evidence() {
    let mut fx = Fx::new();
    let actor = fx.agent("agent-a", "sess-a");

    // Case 1: an item with two acceptance criteria can only be completed when
    // *both* are met/waived. One met criterion does not make it done.
    let item = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "Ship it".into(),
                context: fx.doc("ship", "in progress"),
                item_type: None,
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec!["release".into()],
                acceptance: vec!["tests pass".into(), "docs updated".into()],
                actor: actor.clone(),
            },
            fx.t0,
            None,
        )
        .unwrap()
        .item;
    let (_c, token) = fx.claim(&item.identifier, &actor, 1);
    fx.store
        .set_status(
            &item.identifier,
            Some(&token),
            Some(1),
            StateKey::doing(),
            None,
            None,
            &actor,
            fx.at(2),
            None,
        )
        .unwrap();

    // Evidence + progress alone must not bypass an unmet criterion.
    fx.store
        .add_evidence(
            &item.identifier,
            "test",
            "cargo test --all",
            Some("48 passed"),
            Some(&token),
            &actor,
            fx.at(4),
            None,
        )
        .unwrap();
    fx.progress(&item.identifier, &token, &actor, 6, "all tests green");
    let version = fx
        .store
        .get_item(&item.identifier, fx.at(6))
        .unwrap()
        .item
        .version;
    let err = fx
        .store
        .set_status(
            &item.identifier,
            Some(&token),
            Some(version),
            StateKey::done(),
            None,
            None,
            &actor,
            fx.at(7),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "completion_evidence_required");
    assert!(err.to_string().contains("0/2"), "got: {err}");

    // One of two met: still refused.
    fx.store
        .set_acceptance(
            &item.identifier,
            1,
            "met",
            Some("ci link"),
            Some(&token),
            &actor,
            fx.at(8),
        )
        .unwrap();
    let version = fx
        .store
        .get_item(&item.identifier, fx.at(8))
        .unwrap()
        .item
        .version;
    let err = fx
        .store
        .set_status(
            &item.identifier,
            Some(&token),
            Some(version),
            StateKey::done(),
            None,
            None,
            &actor,
            fx.at(9),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "completion_evidence_required");
    assert!(err.to_string().contains("1/2"), "got: {err}");

    // Two of two (met + waived): allowed, claim auto-released.
    fx.store
        .set_acceptance(
            &item.identifier,
            2,
            "waived",
            Some("docs handled in APM-9"),
            Some(&token),
            &actor,
            fx.at(10),
        )
        .unwrap();
    let version = fx
        .store
        .get_item(&item.identifier, fx.at(10))
        .unwrap()
        .item
        .version;
    let done = fx
        .store
        .set_status(
            &item.identifier,
            Some(&token),
            Some(version),
            StateKey::done(),
            None,
            Some("verified by cargo test"),
            &actor,
            fx.at(11),
            None,
        )
        .unwrap();
    assert_eq!(done.status, StateKey::done());
    assert_eq!(done.health, Health::Ended);
    assert_eq!(done.claim.as_ref().unwrap().status, "released");

    // Closed items cannot be claimed again until reopened.
    let err = fx
        .store
        .claim(
            &item.identifier,
            &actor,
            ClaimRequest::default(),
            fx.at(12),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "claim_invalid");

    // Case 2: no acceptance criteria -> evidence is mandatory.
    let bare = fx.create("No acceptance criteria");
    let (_cb, tb) = fx.claim(&bare.identifier, &actor, 13);
    fx.store
        .set_status(
            &bare.identifier,
            Some(&tb),
            Some(1),
            StateKey::doing(),
            None,
            None,
            &actor,
            fx.at(14),
            None,
        )
        .unwrap();
    fx.progress(&bare.identifier, &tb, &actor, 15, "implemented");
    let version = fx
        .store
        .get_item(&bare.identifier, fx.at(15))
        .unwrap()
        .item
        .version;
    let err = fx
        .store
        .set_status(
            &bare.identifier,
            Some(&tb),
            Some(version),
            StateKey::done(),
            None,
            None,
            &actor,
            fx.at(16),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "completion_evidence_required");

    // Evidence + progress: allowed for an item with zero criteria.
    fx.store
        .add_evidence(
            &bare.identifier,
            "test",
            "cargo test bare",
            None,
            Some(&tb),
            &actor,
            fx.at(17),
            None,
        )
        .unwrap();
    let version = fx
        .store
        .get_item(&bare.identifier, fx.at(17))
        .unwrap()
        .item
        .version;
    let done = fx
        .store
        .set_status(
            &bare.identifier,
            Some(&tb),
            Some(version),
            StateKey::done(),
            None,
            Some("note"),
            &actor,
            fx.at(18),
            None,
        )
        .unwrap();
    assert_eq!(done.status, StateKey::done());
}

#[test]
fn recover_only_cannot_take_new_or_foreign_work() {
    let mut fx = Fx::new();
    let a = fx.create("Never claimed");
    let b = fx.create("Claimed by me, expired");
    let c = fx.create("Claimed by someone else, expired");
    let me = fx.agent("agent-a", "sess-a");
    let other = fx.agent("agent-b", "sess-b");

    // 1. Never claimed: recovery is not a shortcut to new work.
    let err = fx
        .store
        .claim(
            &a.identifier,
            &me,
            ClaimRequest {
                takeover: true,
                policy: ClaimPolicy::RecoverOnly,
                ..Default::default()
            },
            fx.at(1),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");

    // 2. My own expired claim: recovery is allowed.
    fx.claim(&b.identifier, &me, 0);
    let recovered = fx
        .store
        .claim(
            &b.identifier,
            &me,
            ClaimRequest {
                takeover: true,
                policy: ClaimPolicy::RecoverOnly,
                ..Default::default()
            },
            fx.at(61),
            None,
        )
        .unwrap();
    assert!(recovered.takeover);
    assert_eq!(recovered.claim.session_id.as_deref(), Some("sess-a"));

    // 3. Someone else's expired claim: still forbidden.
    fx.claim(&c.identifier, &other, 0);
    let err = fx
        .store
        .claim(
            &c.identifier,
            &me,
            ClaimRequest {
                takeover: true,
                policy: ClaimPolicy::RecoverOnly,
                ..Default::default()
            },
            fx.at(61),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");

    // A normal claim by another agent may of course take expired work.
    let normal = fx
        .store
        .claim(&c.identifier, &me, ClaimRequest::default(), fx.at(62), None)
        .unwrap();
    assert!(normal.takeover);
}

#[test]
fn dashboard_surfaces_every_view() {
    let mut fx = Fx::new();
    let human_wait = fx.create("Needs a human decision");
    let active = fx.create("Being worked on");
    let stalled = fx.create("Forgotten");
    let lost = fx.create("Holder vanished");
    let downstream = fx.create("Depends on forgotten");
    let finished = fx.create("Completed");
    let actor = fx.agent("agent-a", "sess-a");

    // waiting_human via a structured attention request
    let (_c, token) = fx.claim(&human_wait.identifier, &actor, 0);
    fx.store
        .request_attention(
            Some(&fx.project),
            AttentionRequest {
                item: Some(human_wait.identifier.clone()),
                question: "Rotate the staging credentials?".into(),
                context: "The run cannot deploy until credentials are rotated.".into(),
                options: vec!["yes".into(), "no".into()],
                recommendation: Some("yes".into()),
                blocking: true,
            },
            Some(&token),
            &actor,
            fx.at(1),
            None,
        )
        .unwrap();
    fx.store
        .set_status(
            &human_wait.identifier,
            Some(&token),
            Some(1),
            StateKey::blocked(),
            Some("waiting for creds"),
            None,
            &actor,
            fx.at(2),
            None,
        )
        .unwrap();

    // active
    let (_ca, ta) = fx.claim(&active.identifier, &actor, 0);
    fx.store
        .set_status(
            &active.identifier,
            Some(&ta),
            Some(1),
            StateKey::doing(),
            None,
            None,
            &actor,
            fx.at(3),
            None,
        )
        .unwrap();

    // stalled: progress long ago, still doing; long lease so the heartbeat at
    // t+5000s is legitimate and proves heartbeats do not refresh progress.
    let (_cs, ts_) = fx
        .store
        .claim(
            &stalled.identifier,
            &actor,
            ClaimRequest {
                lease_seconds: Some(10_000),
                ..Default::default()
            },
            fx.at(0),
            None,
        )
        .map(|i| (i.claim, i.token))
        .unwrap();
    fx.store
        .set_status(
            &stalled.identifier,
            Some(&ts_),
            Some(1),
            StateKey::doing(),
            None,
            None,
            &actor,
            fx.at(4),
            None,
        )
        .unwrap();
    fx.progress(&stalled.identifier, &ts_, &actor, 5, "started");
    // heartbeat keeps it alive but must not count as progress
    fx.store
        .heartbeat(&stalled.identifier, &ts_, None, &actor, fx.at(5000), None)
        .unwrap();

    // lost lease
    let (_cl, _tl) = fx.claim(&lost.identifier, &actor, 0);
    fx.store
        .set_status(
            &lost.identifier,
            Some(&_tl),
            Some(1),
            StateKey::doing(),
            None,
            None,
            &actor,
            fx.at(6),
            None,
        )
        .unwrap();

    // dependency
    let (_cd, td) = fx.claim(&downstream.identifier, &actor, 0);
    fx.store
        .add_dependency(
            &downstream.identifier,
            &stalled.identifier,
            Some(&td),
            &actor,
            fx.at(7),
        )
        .unwrap();

    // recently done
    let (_cf, tf) = fx.claim(&finished.identifier, &actor, 0);
    fx.store
        .set_status(
            &finished.identifier,
            Some(&tf),
            Some(1),
            StateKey::doing(),
            None,
            None,
            &actor,
            fx.at(8),
            None,
        )
        .unwrap();
    fx.progress(&finished.identifier, &tf, &actor, 9, "finished work");
    fx.store
        .add_evidence(
            &finished.identifier,
            "test",
            "cargo test dashboard",
            None,
            Some(&tf),
            &actor,
            fx.at(9),
            None,
        )
        .unwrap();
    let v = fx
        .store
        .get_item(&finished.identifier, fx.at(9))
        .unwrap()
        .item
        .version;
    fx.store
        .set_status(
            &finished.identifier,
            Some(&tf),
            Some(v),
            StateKey::done(),
            None,
            Some("note"),
            &actor,
            fx.at(10),
            None,
        )
        .unwrap();

    let now = fx.at(7200); // two hours later
    let dash = fx
        .store
        .dashboard(Some(&fx.project), now, DashboardOptions::default())
        .unwrap();

    let ids = |v: &Vec<WorkItemView>| v.iter().map(|i| i.identifier.clone()).collect::<Vec<_>>();
    assert!(ids(&dash.buckets.waiting_human).contains(&human_wait.identifier));
    assert!(ids(&dash.buckets.in_progress).contains(&active.identifier));
    assert!(ids(&dash.buckets.stalled_no_progress).contains(&stalled.identifier));
    assert!(ids(&dash.buckets.lost_lease).contains(&lost.identifier));
    assert!(ids(&dash.buckets.waiting_dependencies).contains(&downstream.identifier));
    assert!(ids(&dash.buckets.recently_done).contains(&finished.identifier));
    assert!(
        dash.buckets
            .ready_unclaimed
            .iter()
            .any(|i| i.identifier == downstream.identifier)
    );

    // The stalled item is alive (heartbeat) but has no substantive progress.
    let row = dash
        .buckets
        .stalled_no_progress
        .iter()
        .find(|i| i.identifier == stalled.identifier)
        .unwrap();
    // Held but not recently heartbeaten: `stale`, yet the lease is still valid,
    // so the item is "in progress" — the two axes disagree, by design.
    assert_eq!(row.health, Health::Stale);
    assert!(row.seconds_since_progress.unwrap() > 3600);
    assert!(ids(&dash.buckets.in_progress).contains(&stalled.identifier));

    // Three clocks are exposed separately for every row.
    for bucket in [
        &dash.buckets.in_progress,
        &dash.buckets.stalled_no_progress,
        &dash.buckets.waiting_human,
    ] {
        for item in bucket {
            assert!(item.last_heartbeat_at.is_some() || item.claim.is_none());
        }
    }
}

#[test]
fn attention_request_and_reply_lifecycle() {
    let mut fx = Fx::new();
    let item = fx.create("Needs approval");
    let actor = fx.agent("agent-a", "sess-a");
    let (_c, token) = fx.claim(&item.identifier, &actor, 0);

    // Agents need a claim for item-scoped attention requests.
    let req = AttentionRequest {
        item: Some(item.identifier.clone()),
        question: "Merge to main?".into(),
        context: "All tests pass; branch is feature/apm.".into(),
        options: vec!["merge".into(), "hold".into()],
        recommendation: Some("merge".into()),
        blocking: true,
    };
    let err = fx
        .store
        .request_attention(Some(&fx.project), req.clone(), None, &actor, fx.at(1), None)
        .unwrap_err();
    assert_eq!(err.code(), "claim_required");

    let attention = fx
        .store
        .request_attention(
            Some(&fx.project),
            req,
            Some(&token),
            &actor,
            fx.at(2),
            Some("attn-1"),
        )
        .unwrap();
    assert_eq!(attention.status, "open");
    assert!(attention.blocking);
    assert_eq!(attention.options.len(), 2);

    // Dashboard exposes it under waiting_human.
    let dash = fx
        .store
        .dashboard(Some(&fx.project), fx.at(3), DashboardOptions::default())
        .unwrap();
    assert!(dash.buckets.waiting_human.iter().any(|i| i.id == item.id));
    assert_eq!(
        dash.buckets.waiting_human[0].attention[0].question,
        "Merge to main?"
    );

    // The human answers; a second answer is rejected.
    let human = fx.human("alice");
    let resolved = fx
        .store
        .reply_attention(&attention.id, "merge after CI", &human, fx.at(4), None)
        .unwrap();
    assert_eq!(resolved.status, "resolved");
    assert_eq!(resolved.response.as_deref(), Some("merge after CI"));
    assert_eq!(resolved.resolved_by.as_deref(), Some("alice"));

    let err = fx
        .store
        .reply_attention(&attention.id, "again", &human, fx.at(5), None)
        .unwrap_err();
    assert_eq!(err.code(), "invalid_input");

    let detail = fx.store.get_item(&item.identifier, fx.at(6)).unwrap();
    assert!(detail.item.attention.is_empty());
    assert_eq!(detail.attention.len(), 1);
    assert_eq!(detail.attention[0].status, "resolved");

    let kinds = event_kinds(&fx.store, &item.identifier);
    assert!(kinds.contains(&"attention_requested".to_string()));
    assert!(kinds.contains(&"attention_replied".to_string()));

    let dash = fx
        .store
        .dashboard(Some(&fx.project), fx.at(7), DashboardOptions::default())
        .unwrap();
    assert!(!dash.buckets.waiting_human.iter().any(|i| i.id == item.id));
}

#[test]
fn dependency_cycles_are_rejected() {
    let mut fx = Fx::new();
    let a = fx.create("A");
    let b = fx.create("B");
    let actor = fx.agent("agent-a", "sess-a");
    let (_ca, ta) = fx.claim(&a.identifier, &actor, 0);
    let (_cb, tb) = fx.claim(&b.identifier, &actor, 0);

    fx.store
        .add_dependency(&a.identifier, &b.identifier, Some(&ta), &actor, fx.at(1))
        .unwrap();
    let err = fx
        .store
        .add_dependency(&b.identifier, &a.identifier, Some(&tb), &actor, fx.at(2))
        .unwrap_err();
    assert_eq!(err.code(), "dependency_invalid");

    let err = fx
        .store
        .add_dependency(&a.identifier, &a.identifier, Some(&ta), &actor, fx.at(3))
        .unwrap_err();
    assert_eq!(err.code(), "dependency_invalid");
}

#[test]
fn diagnostic_does_not_require_a_claim_and_reaches_the_dashboard() {
    let mut fx = Fx::new();
    let item = fx.create("Lost lease recovery");
    let actor = fx.agent("agent-a", "sess-a");
    fx.claim(&item.identifier, &actor, 0);
    // Lease expired; the side-run could not recover.
    let side = Actor::agent("side-run").with_session(Some("sess-a".into()));
    fx.store
        .diagnose(
            Some(&fx.project),
            Some(&item.identifier),
            &side,
            "claim_recovery_failed",
            "lease expired and takeover was refused",
            Some("sr:sess-a:run-9:diag"),
        )
        .unwrap();

    let dash = fx
        .store
        .dashboard(Some(&fx.project), fx.at(120), DashboardOptions::default())
        .unwrap();
    assert!(dash.buckets.recovery_failed.iter().any(|i| i.id == item.id));

    let doctor = fx.store.doctor(Some(&fx.project), fx.at(120)).unwrap();
    assert!(!doctor.pending_diagnostics.is_empty());
    assert!(!doctor.warnings.is_empty());

    // Recovering the lease clears the pending-diagnostic view.
    fx.store
        .claim(
            &item.identifier,
            &side,
            ClaimRequest {
                takeover: true,
                policy: ClaimPolicy::RecoverOnly,
                ..Default::default()
            },
            fx.at(130),
            None,
        )
        .unwrap();
    let dash = fx
        .store
        .dashboard(Some(&fx.project), fx.at(131), DashboardOptions::default())
        .unwrap();
    assert!(dash.buckets.recovery_failed.is_empty());
}

#[test]
fn parent_child_and_labels_round_trip() {
    let mut fx = Fx::new();
    let parent = fx.create("Parent");
    let actor = fx.agent("agent-a", "sess-a");
    let child = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "Child".into(),
                context: fx.doc("child goal", "not started"),
                item_type: Some("bug".into()),
                priority: Some("high".into()),
                parent: Some(parent.identifier.clone()),
                depends_on: vec![parent.identifier.clone()],
                labels: vec!["pipeline".into(), "urgent".into()],
                acceptance: vec!["repro fixed".into()],
                actor: actor.clone(),
            },
            fx.at(1),
            None,
        )
        .unwrap()
        .item;
    assert_eq!(
        child.parent_identifier.as_deref(),
        Some(parent.identifier.as_str())
    );
    assert_eq!(
        child.labels,
        vec!["pipeline".to_string(), "urgent".to_string()]
    );
    assert_eq!(child.blocked_by.len(), 1);

    let listed = fx
        .store
        .list_items(
            ListFilter {
                project: Some(fx.project.clone()),
                parent: Some(parent.identifier.clone()),
                include_closed: true,
                ..Default::default()
            },
            fx.at(2),
        )
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, child.id);
}

// ---------------------------------------------------------------------------
// Regression tests for review findings (v0.1 review round 1)
// ---------------------------------------------------------------------------

#[test]
fn generic_update_bumps_version_and_rejects_a_stale_second_write() {
    let mut fx = Fx::new();
    let item = fx.create("Version discipline");
    let actor = fx.agent("agent-a", "sess-a");
    let (_c, token) = fx.claim(&item.identifier, &actor, 0);

    let first = fx
        .store
        .update_item(
            &item.identifier,
            Some(&token),
            Some(1),
            ItemPatch {
                priority: Some("high".into()),
                ..Default::default()
            },
            &actor,
            fx.at(1),
            None,
        )
        .unwrap();
    assert_eq!(
        first.version, 2,
        "the DB version must advance with the write"
    );

    let err = fx
        .store
        .update_item(
            &item.identifier,
            Some(&token),
            Some(1),
            ItemPatch {
                title: Some("lost update".into()),
                ..Default::default()
            },
            &actor,
            fx.at(2),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "version_conflict");

    let current = fx.store.get_item(&item.identifier, fx.at(2)).unwrap().item;
    assert_eq!(current.version, 2);
    assert_eq!(current.title, "Version discipline");

    // A patch that changes nothing must not burn a version either.
    let err = fx
        .store
        .update_item(
            &item.identifier,
            Some(&token),
            Some(2),
            ItemPatch {
                priority: Some("high".into()),
                ..Default::default()
            },
            &actor,
            fx.at(3),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "invalid_input");

    let ev = fx
        .store
        .item_events(&item.identifier, 50)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "item_updated")
        .unwrap();
    assert_eq!(ev.prev_version, Some(1));
    assert_eq!(
        ev.new_version,
        Some(2),
        "event version must match the DB version"
    );
}

#[test]
fn idempotency_keys_are_bound_to_operation_item_and_payload() {
    let mut fx = Fx::new();
    let a = fx.create("A");
    let b = fx.create("B");
    let actor = fx.agent("agent-a", "sess-a");
    let (_ca, ta) = fx.claim(&a.identifier, &actor, 0);
    let (_cb, tb) = fx.claim(&b.identifier, &actor, 0);

    // Same key on a different item must not replay the first item's response.
    let c = fx.create("C");
    let d = fx.create("D");
    fx.store
        .claim(
            &c.identifier,
            &actor,
            ClaimRequest::default(),
            fx.at(1),
            Some("k-claim"),
        )
        .unwrap();
    let err = fx
        .store
        .claim(
            &d.identifier,
            &actor,
            ClaimRequest::default(),
            fx.at(2),
            Some("k-claim"),
        )
        .unwrap_err();
    assert_eq!(err.code(), "idempotency_conflict");

    // Same key on a different operation.
    fx.store
        .record_progress(
            &a.identifier,
            Some(&ta),
            Some(1),
            "first",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(3),
            Some("k-work"),
        )
        .unwrap();
    let err = fx
        .store
        .sync_run(
            &a.identifier,
            Some(&ta),
            RunOutcome::Completed,
            None,
            &actor,
            fx.at(4),
            Some("k-work"),
        )
        .unwrap_err();
    assert_eq!(err.code(), "idempotency_conflict");

    // Same key, same op, different payload.
    let err = fx
        .store
        .record_progress(
            &a.identifier,
            Some(&ta),
            Some(2),
            "second",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(5),
            Some("k-work"),
        )
        .unwrap_err();
    assert_eq!(err.code(), "idempotency_conflict");

    // Byte-identical retry replays without writing another event.
    let replayed = fx
        .store
        .record_progress(
            &a.identifier,
            Some(&ta),
            Some(1),
            "first",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(9),
            Some("k-work"),
        )
        .unwrap();
    assert_eq!(replayed.item.version, 2);
    assert_eq!(
        event_kinds(&fx.store, &a.identifier)
            .iter()
            .filter(|k| *k == "progress_recorded")
            .count(),
        1
    );
    // An unrelated item was never touched.
    assert_eq!(
        fx.store
            .get_item(&b.identifier, fx.at(9))
            .unwrap()
            .item
            .version,
        1
    );
    let _ = tb;
}

#[test]
fn expire_never_clobbers_a_renewed_claim() {
    let mut fx = Fx::new();
    let item = fx.create("Renewal race");
    let actor = fx.agent("agent-a", "sess-a");
    let (claim, token) = fx.claim(&item.identifier, &actor, 0); // expires at t+60

    // Renew at t+50, so the lease now runs to t+110.
    fx.store
        .heartbeat(&item.identifier, &token, Some(60), &actor, fx.at(50), None)
        .unwrap();

    // A stale sweep that scanned before the renewal must not expire it: the
    // conditional UPDATE matches no row.
    let expired = fx.store.expire_claim(&claim.id, fx.at(61)).unwrap();
    assert!(
        expired.is_none(),
        "renewed claim must survive a stale sweep"
    );
    assert!(fx.store.expire_claims(fx.at(61)).unwrap().is_empty());

    let view = fx.store.get_item(&item.identifier, fx.at(61)).unwrap().item;
    // Still held (stale heartbeat, valid lease), definitely not expired.
    assert_eq!(view.health, Health::Stale);
    assert_eq!(view.claim.unwrap().status, "active");

    // Once the renewed lease really does expire, the sweep works.
    let expired = fx.store.expire_claims(fx.at(111)).unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(
        fx.store
            .get_item(&item.identifier, fx.at(111))
            .unwrap()
            .item
            .health,
        Health::Expired
    );
}

#[test]
fn recover_only_rejects_released_or_superseded_claims() {
    let mut fx = Fx::new();
    let me = fx.agent("agent-a", "sess-a");
    let other = fx.agent("agent-b", "sess-b");

    // Released by the session itself: a side-run may not silently take it back.
    let released = fx.create("Released");
    let (_c, token) = fx.claim(&released.identifier, &me, 0);
    fx.store
        .release(
            &released.identifier,
            &token,
            Some("handoff"),
            &me,
            fx.at(5),
            None,
        )
        .unwrap();
    let err = fx
        .store
        .claim(
            &released.identifier,
            &me,
            ClaimRequest {
                takeover: true,
                policy: ClaimPolicy::RecoverOnly,
                ..Default::default()
            },
            fx.at(10),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");

    // Superseded by a human-authorized takeover: the original session gave up
    // the lease to someone else, so a side-run may not silently recover it.
    let superseded = fx.create("Superseded");
    fx.claim(&superseded.identifier, &me, 0);
    let human = fx.human("alice");
    let takeover = fx
        .store
        .claim(
            &superseded.identifier,
            &human,
            ClaimRequest {
                takeover: true,
                reason: Some("reassign to another agent".into()),
                ..Default::default()
            },
            fx.at(5),
            None,
        )
        .unwrap();
    assert!(takeover.takeover);
    fx.store
        .release(
            &superseded.identifier,
            &takeover.token,
            Some("done"),
            &human,
            fx.at(6),
            None,
        )
        .unwrap();
    let err = fx
        .store
        .claim(
            &superseded.identifier,
            &me,
            ClaimRequest {
                takeover: true,
                policy: ClaimPolicy::RecoverOnly,
                ..Default::default()
            },
            fx.at(10),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");

    // A plain claim (not a side-run) may still pick the item up: the restriction
    // is about side-runs, not about ordinary agents.
    let normal = fx
        .store
        .claim(
            &superseded.identifier,
            &other,
            ClaimRequest::default(),
            fx.at(11),
            None,
        )
        .unwrap();
    assert_eq!(normal.claim.holder, "agent-b");
}

#[test]
fn attention_reply_is_human_only_and_withdraw_requires_a_claim() {
    let mut fx = Fx::new();
    let item = fx.create("Ask a human");
    let agent = fx.agent("agent-a", "sess-a");
    let (_c, token) = fx.claim(&item.identifier, &agent, 0);

    let req = AttentionRequest {
        item: Some(item.identifier.clone()),
        question: "Deploy?".into(),
        context: "Staging is green.".into(),
        options: vec!["yes".into(), "no".into()],
        recommendation: None,
        blocking: true,
    };
    let attention = fx
        .store
        .request_attention(
            Some(&fx.project),
            req.clone(),
            Some(&token),
            &agent,
            fx.at(1),
            None,
        )
        .unwrap();

    // An agent cannot answer its own question for a human.
    let err = fx
        .store
        .reply_attention(&attention.id, "yes", &agent, fx.at(2), None)
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");

    // A human can.
    fx.store
        .reply_attention(&attention.id, "yes", &fx.human("alice"), fx.at(3), None)
        .unwrap();

    // Withdraw: an agent without a live claim is refused.
    let second = fx
        .store
        .request_attention(
            Some(&fx.project),
            req.clone(),
            Some(&token),
            &agent,
            fx.at(4),
            None,
        )
        .unwrap();
    let err = fx
        .store
        .withdraw_attention(&second.id, None, &agent, fx.at(5))
        .unwrap_err();
    assert_eq!(err.code(), "claim_required");

    // A valid token whose lease died is also refused (claim_invalid), so a
    // dropped session cannot retract requests it no longer owns.
    fx.store
        .release(
            &item.identifier,
            &token,
            Some("handoff"),
            &agent,
            fx.at(6),
            None,
        )
        .unwrap();
    let err = fx
        .store
        .withdraw_attention(&second.id, Some(&token), &agent, fx.at(7))
        .unwrap_err();
    // Released: no live claim at all, so the stale token is simply not enough.
    assert_eq!(err.code(), "claim_required");

    // Another agent holding the claim is not the requester: forbidden.
    let human = fx.human("alice");
    let human_claim = fx
        .store
        .claim(
            &item.identifier,
            &human,
            ClaimRequest {
                takeover: true,
                reason: Some("moving to agent-b".into()),
                ..Default::default()
            },
            fx.at(8),
            None,
        )
        .unwrap();
    fx.store
        .release(
            &item.identifier,
            &human_claim.token,
            Some("handing over"),
            &human,
            fx.at(9),
            None,
        )
        .unwrap();
    let other = fx.agent("agent-b", "sess-b");
    let other_claim = fx
        .store
        .claim(
            &item.identifier,
            &other,
            ClaimRequest::default(),
            fx.at(10),
            None,
        )
        .unwrap();
    let err = fx
        .store
        .withdraw_attention(&second.id, Some(&other_claim.token), &other, fx.at(11))
        .unwrap_err();
    assert_eq!(err.code(), "forbidden");

    // The requester, holding a fresh claim for the same item, may withdraw.
    // (agent-b hands the item back first; no takeover needed.)
    fx.store
        .release(
            &item.identifier,
            &other_claim.token,
            Some("handing back"),
            &other,
            fx.at(12),
            None,
        )
        .unwrap();
    let again = fx
        .store
        .claim(
            &item.identifier,
            &agent,
            ClaimRequest::default(),
            fx.at(13),
            None,
        )
        .unwrap();
    let withdrawn = fx
        .store
        .withdraw_attention(&second.id, Some(&again.token), &agent, fx.at(14))
        .unwrap();
    assert_eq!(withdrawn.status, "withdrawn");

    // Humans can always withdraw (management path).
    let third = fx
        .store
        .request_attention(
            Some(&fx.project),
            req,
            Some(&again.token),
            &agent,
            fx.at(15),
            None,
        )
        .unwrap();
    let withdrawn = fx
        .store
        .withdraw_attention(&third.id, None, &human, fx.at(15))
        .unwrap();
    assert_eq!(withdrawn.status, "withdrawn");

    // No agent reply ever resolved the open question.
    let detail = fx.store.get_item(&item.identifier, fx.at(16)).unwrap();
    assert!(detail.attention.iter().all(|a| a.status != "open"));
}

#[test]
fn attention_requests_cannot_cross_projects() {
    let mut fx = Fx::new();
    let item = fx.create("Belongs to project one");
    let actor = fx.agent("agent-a", "sess-a");
    let (_c, token) = fx.claim(&item.identifier, &actor, 0);
    let (other_project, _) = fx
        .store
        .init_project(
            "Other",
            Some("OTH"),
            Some("/tmp/other"),
            Settings::default(),
            &fx.human("setup"),
        )
        .unwrap();

    let err = fx
        .store
        .request_attention(
            Some(&other_project.id),
            AttentionRequest {
                item: Some(item.identifier.clone()),
                question: "q".into(),
                context: "c".into(),
                options: vec![],
                recommendation: None,
                blocking: true,
            },
            Some(&token),
            &actor,
            fx.at(1),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
    assert!(err.to_string().contains("another project"));
}

#[test]
fn repeated_progress_is_a_noop_and_never_refreshes_the_progress_clock() {
    let mut fx = Fx::new();
    let item = fx.create("No fake progress");
    let actor = fx.agent("agent-a", "sess-a");
    let (_c, token) = fx
        .store
        .claim(
            &item.identifier,
            &actor,
            ClaimRequest {
                lease_seconds: Some(3600),
                ..Default::default()
            },
            fx.at(0),
            None,
        )
        .map(|i| (i.claim, i.token))
        .unwrap();

    let first = fx
        .store
        .record_progress(
            &item.identifier,
            Some(&token),
            Some(1),
            "implemented the parser",
            Some(Some("write tests".into())),
            None,
            None,
            None,
            &actor,
            fx.at(10),
            None,
        )
        .unwrap();
    assert!(first.substantive);
    assert!(!first.no_op);
    assert_eq!(first.item.version, 2);
    let progress_at = first.item.last_progress_at.clone();
    assert_eq!(progress_at, Some(ts(fx.at(10))));

    // Byte-identical repeat at a later time: nothing written, clock untouched.
    let repeat = fx
        .store
        .record_progress(
            &item.identifier,
            Some(&token),
            Some(2),
            "implemented the parser",
            Some(Some("write tests".into())),
            None,
            None,
            None,
            &actor,
            fx.at(600),
            None,
        )
        .unwrap();
    assert!(repeat.no_op, "identical progress must be a no-op");
    assert!(!repeat.substantive);
    assert_eq!(repeat.item.version, 2, "no version churn for a no-op");
    assert_eq!(repeat.item.last_progress_at, progress_at);
    assert_eq!(
        event_kinds(&fx.store, &item.identifier)
            .iter()
            .filter(|k| *k == "progress_recorded")
            .count(),
        1
    );

    // Changing only bookkeeping fields is an update, but not substantive
    // progress: the clock still must not move.
    let bookkeeping = fx
        .store
        .record_progress(
            &item.identifier,
            Some(&token),
            Some(2),
            "implemented the parser",
            Some(Some("write tests and docs".into())),
            None,
            None,
            None,
            &actor,
            fx.at(901),
            None,
        )
        .unwrap();
    assert!(!bookkeeping.no_op);
    assert!(!bookkeeping.substantive);
    assert_eq!(bookkeeping.item.version, 3);
    assert_eq!(bookkeeping.item.last_progress_at, progress_at);
    assert!(event_kinds(&fx.store, &item.identifier).contains(&"progress_updated".to_string()));

    // A genuinely new summary does advance the clock.
    let new = fx
        .store
        .record_progress(
            &item.identifier,
            Some(&token),
            Some(3),
            "tests are green",
            None,
            None,
            None,
            None,
            &actor,
            fx.at(1200),
            None,
        )
        .unwrap();
    assert!(new.substantive);
    assert_eq!(new.item.last_progress_at, Some(ts(fx.at(1200))));
}

#[test]
fn plaintext_claim_tokens_are_never_persisted_or_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join(".apm").join("apm.db");
    let mut store = Store::open(&db_path).unwrap();
    let (project, _) = store
        .init_project(
            "Tokens",
            Some("TKN"),
            None,
            Settings::default(),
            &Actor::human("setup"),
        )
        .unwrap();
    let actor = Actor::agent("agent-a").with_session(Some("sess-tokens".into()));
    let item = store
        .create_item(
            Some(&project.id),
            NewItem {
                title: "Secret handling".into(),
                context: ContextDoc {
                    goal: "no token leak".into(),
                    current_state: "starting".into(),
                    ..Default::default()
                },
                item_type: None,
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec![],
                actor: actor.clone(),
            },
            1_700_000_000_000,
            None,
        )
        .unwrap()
        .item;

    let issued = store
        .claim(
            &item.identifier,
            &actor,
            ClaimRequest::default(),
            1_700_000_001_000,
            Some("token-key"),
        )
        .unwrap();
    let token = issued.token.clone();
    assert!(token.starts_with("apm_"));

    // The idempotency record must be redacted, and a replay must not hand back a
    // token the database never stored.
    let replayed = store
        .claim(
            &item.identifier,
            &actor,
            ClaimRequest::default(),
            1_700_000_002_000,
            Some("token-key"),
        )
        .unwrap();
    assert!(replayed.replayed);
    assert!(
        replayed.token.is_empty(),
        "replay cannot return a plaintext token"
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let stored: String = conn
        .query_row(
            "SELECT response_json FROM idempotency WHERE key = 'token-key'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !stored.contains(&token),
        "idempotency response leaked the token"
    );
    assert!(
        !stored.contains("apm_"),
        "idempotency response contains a token prefix"
    );

    // The database stores only the hash, never the plaintext — including the WAL.
    for suffix in ["", "-wal", "-shm"] {
        let path = format!("{}{}", db_path.display(), suffix);
        if let Ok(bytes) = std::fs::read(&path) {
            let haystack = String::from_utf8_lossy(&bytes);
            assert!(
                !haystack.contains(&token),
                "plaintext token found in {path}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// APM-7: custom states, per-type workflows, versioned schema and migration
// ---------------------------------------------------------------------------

use apm::workflow::{GuardDef, StateDef, TransitionDef, Workflow};

fn state(key: &str, category: Category, terminal: bool, cancel: bool) -> StateDef {
    StateDef {
        key: key.to_string(),
        label: key.to_string(),
        category,
        terminal,
        cancel,
    }
}

fn tr(from: &str, to: &str, requires: &[&str]) -> TransitionDef {
    TransitionDef {
        from: from.to_string(),
        to: to.to_string(),
        requires: requires.iter().map(|r| r.to_string()).collect(),
    }
}

/// Four non-closed states plus a completion and a cancel outcome.
fn delivery_workflow() -> Workflow {
    Workflow {
        id: "delivery".into(),
        version: 2,
        name: "Delivery".into(),
        initial: "intake".into(),
        states: vec![
            state("intake", Category::Open, false, false),
            state("building", Category::Active, false, false),
            state("verifying", Category::Review, false, false),
            state("shipped", Category::Closed, true, false),
            state("aborted", Category::Closed, true, true),
        ],
        transitions: vec![
            tr("intake", "building", &[]),
            tr("intake", "aborted", &["reason"]),
            tr("building", "verifying", &[]),
            tr("building", "aborted", &["reason"]),
            tr("verifying", "building", &["reason"]),
            tr(
                "verifying",
                "shipped",
                &["evidence_or_acceptance", "progress_or_note"],
            ),
            tr("shipped", "building", &["reason"]),
        ],
        guards: Workflow::builtin_guards(),
    }
}

#[test]
fn custom_workflow_binds_per_item_type_and_items_keep_their_snapshot() {
    let mut fx = Fx::new();
    let actor = fx.agent("agent-a", "sess-wf");
    let workflow = fx
        .store
        .register_workflow(Some(&fx.project), delivery_workflow(), &actor, fx.at(0))
        .unwrap();
    assert_eq!(workflow.id, "delivery");
    assert_eq!(workflow.version, 2);

    let binding = fx
        .store
        .bind_type_workflow(Some(&fx.project), "epic", "delivery", 2, &actor, fx.at(1))
        .unwrap();
    assert_eq!(binding.item_type, "epic");
    assert_eq!(binding.workflow_id, "delivery");
    assert_eq!(binding.workflow_version, 2);

    let epic = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "Ship the epic".into(),
                context: fx.doc("ship", "intake"),
                item_type: Some("epic".into()),
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec!["tests pass".into()],
                actor: actor.clone(),
            },
            fx.at(2),
            None,
        )
        .unwrap()
        .item;
    assert_eq!(epic.status.as_str(), "intake");
    assert_eq!(epic.status_category, "open");
    assert_eq!(epic.workflow_id, "delivery");
    assert_eq!(epic.workflow_version, 2);

    // A task continues to use the default six-state workflow.
    let task = fx.create("Plain task");
    assert_eq!(task.workflow_id, "apm-default");
    assert_eq!(task.workflow_version, 1);

    let (_c, token) = fx.claim(&epic.identifier, &actor, fx.at(3));
    let moved = fx
        .store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(1),
            StateKey::parse("building").unwrap(),
            None,
            None,
            &actor,
            fx.at(4),
            None,
        )
        .unwrap();
    assert_eq!(moved.status_category, "active");

    // States outside the snapshot are rejected, even though they exist in the
    // default workflow.
    let err = fx
        .store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(2),
            StateKey::ready(),
            Some("not in this workflow"),
            None,
            &actor,
            fx.at(5),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "workflow_violation");
    assert!(err.to_string().contains("delivery v2"), "{err}");

    // Completion still needs evidence + progress.
    fx.store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(2),
            StateKey::parse("verifying").unwrap(),
            None,
            None,
            &actor,
            fx.at(6),
            None,
        )
        .unwrap();
    let v = fx
        .store
        .get_item(&epic.identifier, fx.at(6))
        .unwrap()
        .item
        .version;
    let err = fx
        .store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(v),
            StateKey::parse("shipped").unwrap(),
            None,
            None,
            &actor,
            fx.at(7),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "completion_evidence_required");

    // Re-binding the type does not reinterpret the existing item...
    fx.store
        .bind_type_workflow(
            Some(&fx.project),
            "epic",
            "apm-default",
            1,
            &actor,
            fx.at(8),
        )
        .unwrap();
    let after_rebind = fx.store.get_item(&epic.identifier, fx.at(8)).unwrap().item;
    assert_eq!(after_rebind.workflow_id, "delivery");
    assert_eq!(after_rebind.workflow_version, 2);
    // ...and a new item picks up the new binding.
    let new_epic = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "Another epic".into(),
                context: fx.doc("g", "c"),
                item_type: Some("epic".into()),
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec![],
                actor,
            },
            fx.at(9),
            None,
        )
        .unwrap()
        .item;
    assert_eq!(new_epic.workflow_id, "apm-default");
    assert_eq!(new_epic.status.as_str(), "ready");
}

#[test]
fn workflow_registration_rejects_invalid_documents_and_is_idempotent() {
    let mut fx = Fx::new();
    let actor = fx.agent("agent-a", "sess-wf");
    let valid = delivery_workflow();
    fx.store
        .register_workflow(Some(&fx.project), valid.clone(), &actor, fx.at(0))
        .unwrap();
    // Same id/version/content: idempotent.
    fx.store
        .register_workflow(Some(&fx.project), valid.clone(), &actor, fx.at(1))
        .unwrap();

    let mut changed = valid.clone();
    changed.name = "Different".into();
    assert_eq!(
        fx.store
            .register_workflow(Some(&fx.project), changed, &actor, fx.at(2))
            .unwrap_err()
            .code(),
        "invalid_input",
        "same id/version with different content must be rejected"
    );

    // Unknown state in a transition.
    let mut broken = valid.clone();
    broken.id = "broken-target".into();
    broken.transitions.push(tr("intake", "ghost", &[]));
    assert_eq!(
        fx.store
            .register_workflow(Some(&fx.project), broken, &actor, fx.at(3))
            .unwrap_err()
            .code(),
        "invalid_input"
    );

    // Dead end: remove every exit from a non-closed state.
    let mut dead = valid.clone();
    dead.id = "dead".into();
    dead.transitions.retain(|t| t.from != "building");
    assert_eq!(
        fx.store
            .register_workflow(Some(&fx.project), dead, &actor, fx.at(4))
            .unwrap_err()
            .code(),
        "invalid_input"
    );

    // Unreachable state.
    let mut orphan = valid.clone();
    orphan.id = "orphan".into();
    orphan
        .states
        .push(state("icebox", Category::Open, false, false));
    orphan.transitions.push(tr(
        "icebox",
        "shipped",
        &["evidence_or_acceptance", "progress_or_note"],
    ));
    assert_eq!(
        fx.store
            .register_workflow(Some(&fx.project), orphan, &actor, fx.at(5))
            .unwrap_err()
            .code(),
        "invalid_input"
    );

    // Weakened completion guard.
    let mut weak = valid.clone();
    weak.id = "weak".into();
    for t in &mut weak.transitions {
        if t.to == "shipped" {
            t.requires.retain(|g| g != "evidence_or_acceptance");
        }
    }
    assert_eq!(
        fx.store
            .register_workflow(Some(&fx.project), weak, &actor, fx.at(6))
            .unwrap_err()
            .code(),
        "invalid_input"
    );

    // Unknown guard kind, closed initial state and version 0.
    let mut bad_guard = valid.clone();
    bad_guard.id = "bad-guard".into();
    bad_guard.guards.insert(
        "reason".into(),
        GuardDef {
            kind: "magic".into(),
            description: None,
        },
    );
    assert!(
        fx.store
            .register_workflow(Some(&fx.project), bad_guard, &actor, fx.at(7))
            .is_err()
    );

    let mut closed_initial = valid.clone();
    closed_initial.id = "closed-initial".into();
    closed_initial.initial = "shipped".into();
    assert!(
        fx.store
            .register_workflow(Some(&fx.project), closed_initial, &actor, fx.at(8))
            .is_err()
    );

    let mut zero_version = valid.clone();
    zero_version.id = "zero".into();
    zero_version.version = 0;
    assert!(
        fx.store
            .register_workflow(Some(&fx.project), zero_version, &actor, fx.at(9))
            .is_err()
    );

    // Nothing invalid was persisted.
    let list = fx.store.list_workflows(Some(&fx.project)).unwrap();
    let ids: Vec<String> = list.iter().map(|w| w.id.clone()).collect();
    assert!(ids.contains(&"delivery".to_string()));
    assert!(ids.contains(&"apm-default".to_string()));
    assert!(!ids.contains(&"dead".to_string()));
    assert!(!ids.contains(&"weak".to_string()));
}

#[test]
fn default_workflow_is_unchanged_and_records_its_snapshot() {
    let mut fx = Fx::new();
    let wf = fx.store.workflow(Some(&fx.project)).unwrap();
    assert_eq!(wf.id, "apm-default");
    assert_eq!(wf.version, 1);
    assert_eq!(wf.initial, "ready");
    assert_eq!(wf.states.len(), 6);
    assert_eq!(wf.transitions.len(), 17);
    assert_eq!(wf.guards.len(), 3);

    let item = fx.create("Default behaviour");
    assert_eq!(item.workflow_id, "apm-default");
    assert_eq!(item.workflow_version, 1);
    assert_eq!(item.status.as_str(), "ready");
    assert_eq!(item.status_category, "open");

    let actor = fx.agent("agent-a", "sess-wf");
    let (_c, token) = fx.claim(&item.identifier, &actor, 0);
    fx.store
        .set_status(
            &item.identifier,
            Some(&token),
            Some(1),
            StateKey::doing(),
            None,
            None,
            &actor,
            fx.at(1),
            None,
        )
        .unwrap();
    let events = fx.store.item_events(&item.identifier, 50).unwrap();
    let change = events.iter().find(|e| e.kind == "status_changed").unwrap();
    assert_eq!(change.payload["workflow_id"], "apm-default");
    assert_eq!(change.payload["workflow_version"], 1);
    assert_eq!(change.payload["to_category"], "active");
    assert_eq!(change.payload["terminal"], false);
}

#[test]
fn dashboard_buckets_follow_categories_not_state_names() {
    let mut fx = Fx::new();
    let actor = fx.agent("agent-a", "sess-wf");
    fx.store
        .register_workflow(Some(&fx.project), delivery_workflow(), &actor, fx.at(0))
        .unwrap();
    fx.store
        .bind_type_workflow(Some(&fx.project), "epic", "delivery", 2, &actor, fx.at(1))
        .unwrap();

    let epic = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "Custom-state epic".into(),
                context: fx.doc("ship", "intake"),
                item_type: Some("epic".into()),
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec![],
                actor: actor.clone(),
            },
            fx.at(2),
            None,
        )
        .unwrap()
        .item;
    let (_c, token) = fx.claim(&epic.identifier, &actor, fx.at(3));
    fx.store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(1),
            StateKey::parse("building").unwrap(),
            None,
            None,
            &actor,
            fx.at(4),
            None,
        )
        .unwrap();

    let dash = fx
        .store
        .dashboard(Some(&fx.project), fx.at(5), DashboardOptions::default())
        .unwrap();
    assert!(
        dash.buckets
            .in_progress
            .iter()
            .any(|i| i.identifier == epic.identifier)
    );
    assert!(
        !dash
            .buckets
            .ready_unclaimed
            .iter()
            .any(|i| i.identifier == epic.identifier)
    );

    // Completion moves it into recently done by category + done_at.
    fx.store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(2),
            StateKey::parse("verifying").unwrap(),
            None,
            None,
            &actor,
            fx.at(5),
            None,
        )
        .unwrap();
    fx.progress(&epic.identifier, &token, &actor, 6, "built and verified");
    fx.store
        .add_evidence(
            &epic.identifier,
            "test",
            "cargo test",
            None,
            Some(&token),
            &actor,
            fx.at(7),
            None,
        )
        .unwrap();
    let v = fx
        .store
        .get_item(&epic.identifier, fx.at(7))
        .unwrap()
        .item
        .version;
    fx.store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(v),
            StateKey::parse("shipped").unwrap(),
            None,
            Some("verified"),
            &actor,
            fx.at(8),
            None,
        )
        .unwrap();
    let dash = fx
        .store
        .dashboard(Some(&fx.project), fx.at(9), DashboardOptions::default())
        .unwrap();
    let row = dash
        .buckets
        .recently_done
        .iter()
        .find(|i| i.identifier == epic.identifier)
        .expect("closed custom state is recently done");
    assert_eq!(row.status_category, "closed");
}

#[test]
fn migration_from_v2_backfills_workflows_and_repairs_orphan_states() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("apm.db");
    let project_id;
    let item_id;
    let legacy;

    {
        let mut store = Store::open(&path).unwrap();
        let (project, _) = store
            .init_project(
                "Migrating",
                Some("MIG"),
                None,
                Settings::default(),
                &Actor::human("setup"),
            )
            .unwrap();
        project_id = project.id.clone();
        let mut legacy_wf = Workflow::default_v1();
        legacy_wf.id = "legacy-flow".into();
        legacy_wf.version = 3;
        legacy_wf.name = "Legacy flow".into();
        let actor = Actor::agent("agent-a").with_session(Some("sess-m".into()));
        let item = store
            .create_item(
                Some(&project_id),
                NewItem {
                    title: "Legacy item".into(),
                    context: ContextDoc {
                        goal: "survive".into(),
                        current_state: "old".into(),
                        ..Default::default()
                    },
                    item_type: Some("task".into()),
                    priority: None,
                    parent: None,
                    depends_on: vec![],
                    labels: vec![],
                    acceptance: vec![],
                    actor,
                },
                1_700_000_000_000,
                None,
            )
            .unwrap()
            .item;
        item_id = item.id.clone();
        legacy = legacy_wf;
    }

    // Downgrade the file to a v2 shape: no workflow tables/columns, the legacy
    // project workflow JSON, and an item whose state no workflow declares.
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE type_workflows;
             DROP TABLE workflows;
             ALTER TABLE work_items DROP COLUMN workflow_id;
             ALTER TABLE work_items DROP COLUMN workflow_version;
             ALTER TABLE work_items DROP COLUMN state_category;
             ALTER TABLE projects DROP COLUMN default_workflow_id;
             ALTER TABLE projects DROP COLUMN default_workflow_version;",
        )
        .unwrap();
        conn.execute(
            "UPDATE projects SET workflow_json = ?1 WHERE id = ?2",
            rusqlite::params![legacy.to_json().unwrap(), project_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_items SET status = 'legacy_state' WHERE id = ?1",
            rusqlite::params![item_id],
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 2).unwrap();
    }

    let store = Store::open(&path).unwrap();
    let detail = store.get_item(&item_id, 1_700_000_100_000).unwrap();
    // Bound to the imported legacy workflow, state repaired to its initial.
    assert_eq!(detail.item.workflow_id, "legacy-flow");
    assert_eq!(detail.item.workflow_version, 3);
    assert_eq!(detail.item.status.as_str(), "ready");
    assert_eq!(detail.item.status_category, "open");
    // The repair is auditable.
    let events = store.item_events(&item_id, 100).unwrap();
    assert!(events.iter().any(|e| e.kind == "workflow_migrated"));
    // No orphan: the item can still transition.
    let wf = store.workflow(Some(&project_id)).unwrap();
    assert_eq!(wf.id, "legacy-flow");
    assert_eq!(wf.version, 3);
    assert!(wf.transition("ready", "doing").is_some());
    let list = store
        .list_items(
            ListFilter {
                project: Some(project_id.clone()),
                include_closed: true,
                ..Default::default()
            },
            1_700_000_100_000,
        )
        .unwrap();
    assert_eq!(list.len(), 1);
}

#[test]
fn custom_closed_states_are_filtered_by_category() {
    let mut fx = Fx::new();
    let actor = fx.agent("agent-a", "sess-closed");
    fx.store
        .register_workflow(Some(&fx.project), delivery_workflow(), &actor, fx.at(0))
        .unwrap();
    fx.store
        .bind_type_workflow(Some(&fx.project), "epic", "delivery", 2, &actor, fx.at(1))
        .unwrap();

    let mk = |fx: &mut Fx, title: &str| {
        fx.store
            .create_item(
                Some(&fx.project),
                NewItem {
                    title: title.into(),
                    context: fx.doc("ship", "intake"),
                    item_type: Some("epic".into()),
                    priority: None,
                    parent: None,
                    depends_on: vec![],
                    labels: vec![],
                    acceptance: vec![],
                    actor: actor.clone(),
                },
                fx.at(2),
                None,
            )
            .unwrap()
            .item
    };
    let shipped = mk(&mut fx, "Shipped epic");
    let aborted = mk(&mut fx, "Aborted epic");

    // Ship one item through the custom completion state.
    let (_c1, token) = fx.claim(&shipped.identifier, &actor, fx.at(3));
    for (offset, state) in [(4, "building"), (5, "verifying")] {
        let v = fx
            .store
            .get_item(&shipped.identifier, fx.at(offset))
            .unwrap()
            .item
            .version;
        fx.store
            .set_status(
                &shipped.identifier,
                Some(&token),
                Some(v),
                StateKey::parse(state).unwrap(),
                None,
                None,
                &actor,
                fx.at(offset),
                None,
            )
            .unwrap();
    }
    fx.progress(&shipped.identifier, &token, &actor, 6, "built and verified");
    fx.store
        .add_evidence(
            &shipped.identifier,
            "test",
            "cargo test",
            None,
            Some(&token),
            &actor,
            fx.at(7),
            None,
        )
        .unwrap();
    let v = fx
        .store
        .get_item(&shipped.identifier, fx.at(7))
        .unwrap()
        .item
        .version;
    fx.store
        .set_status(
            &shipped.identifier,
            Some(&token),
            Some(v),
            StateKey::parse("shipped").unwrap(),
            None,
            Some("shipped"),
            &actor,
            fx.at(8),
            None,
        )
        .unwrap();

    // Cancel the other one through the custom cancel state.
    let (_c2, token2) = fx.claim(&aborted.identifier, &actor, fx.at(3));
    fx.store
        .set_status(
            &aborted.identifier,
            Some(&token2),
            Some(1),
            StateKey::parse("aborted").unwrap(),
            Some("not needed anymore"),
            None,
            &actor,
            fx.at(9),
            None,
        )
        .unwrap();

    // Default list: neither custom closed state appears.
    let default_list = fx
        .store
        .list_items(
            ListFilter {
                project: Some(fx.project.clone()),
                ..Default::default()
            },
            fx.at(10),
        )
        .unwrap();
    assert!(
        default_list.is_empty(),
        "custom closed states leaked into the open list"
    );

    // include_closed surfaces both.
    let closed_list = fx
        .store
        .list_items(
            ListFilter {
                project: Some(fx.project.clone()),
                include_closed: true,
                ..Default::default()
            },
            fx.at(10),
        )
        .unwrap();
    assert_eq!(closed_list.len(), 2);
    assert!(closed_list.iter().all(|i| i.status_category == "closed"));

    // Doctor does not count custom closed states as open.
    let doctor = fx.store.doctor(Some(&fx.project), fx.at(10)).unwrap();
    assert_eq!(doctor.open_items, 0, "closed custom states counted as open");

    // Recently done window: inside it, shipped appears and aborted does not.
    let recent = fx
        .store
        .dashboard(Some(&fx.project), fx.at(10), DashboardOptions::default())
        .unwrap();
    assert!(
        recent
            .buckets
            .recently_done
            .iter()
            .any(|i| i.id == shipped.id)
    );
    assert!(
        !recent
            .buckets
            .recently_done
            .iter()
            .any(|i| i.id == aborted.id)
    );

    // Beyond done_recent_seconds the shipped item must disappear instead of
    // living in recently_done forever.
    let later = fx.at(10) + 90_000_000;
    let stale = fx
        .store
        .dashboard(Some(&fx.project), later, DashboardOptions::default())
        .unwrap();
    assert!(
        !stale
            .buckets
            .recently_done
            .iter()
            .any(|i| i.id == shipped.id),
        "shipped stayed in recently_done past the window"
    );
}

#[test]
fn named_guard_aliases_are_validated_by_kind() {
    let mut fx = Fx::new();
    let actor = fx.agent("agent-a", "sess-alias");
    let mut wf = delivery_workflow();
    wf.id = "delivery-alias".into();
    wf.version = 4;
    for tr in &mut wf.transitions {
        tr.requires = tr
            .requires
            .iter()
            .map(|g| match g.as_str() {
                "reason" => "why".to_string(),
                "evidence_or_acceptance" => "completion_evidence".to_string(),
                "progress_or_note" => "completion_progress".to_string(),
                other => other.to_string(),
            })
            .collect();
    }
    wf.guards.clear();
    wf.guards.insert(
        "why".into(),
        GuardDef {
            kind: "reason".into(),
            description: None,
        },
    );
    wf.guards.insert(
        "completion_evidence".into(),
        GuardDef {
            kind: "evidence_or_acceptance".into(),
            description: None,
        },
    );
    wf.guards.insert(
        "completion_progress".into(),
        GuardDef {
            kind: "progress_or_note".into(),
            description: None,
        },
    );
    fx.store
        .register_workflow(Some(&fx.project), wf, &actor, fx.at(0))
        .expect("aliased guards must validate by kind");
    fx.store
        .bind_type_workflow(
            Some(&fx.project),
            "epic",
            "delivery-alias",
            4,
            &actor,
            fx.at(1),
        )
        .unwrap();

    let epic = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "Alias epic".into(),
                context: fx.doc("ship", "intake"),
                item_type: Some("epic".into()),
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec![],
                actor: actor.clone(),
            },
            fx.at(2),
            None,
        )
        .unwrap()
        .item;
    let (_c, token) = fx.claim(&epic.identifier, &actor, fx.at(3));
    let mut version = 1;
    for (offset, state) in [(4, "building"), (5, "verifying")] {
        fx.store
            .set_status(
                &epic.identifier,
                Some(&token),
                Some(version),
                StateKey::parse(state).unwrap(),
                None,
                None,
                &actor,
                fx.at(offset),
                None,
            )
            .unwrap();
        version = fx
            .store
            .get_item(&epic.identifier, fx.at(offset))
            .unwrap()
            .item
            .version;
    }
    // Runtime enforcement follows the guard kinds as well.
    let err = fx
        .store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(version),
            StateKey::parse("shipped").unwrap(),
            None,
            None,
            &actor,
            fx.at(6),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "completion_evidence_required");

    fx.progress(&epic.identifier, &token, &actor, 7, "verified");
    fx.store
        .add_evidence(
            &epic.identifier,
            "test",
            "aliased guards",
            None,
            Some(&token),
            &actor,
            fx.at(8),
            None,
        )
        .unwrap();
    let v = fx
        .store
        .get_item(&epic.identifier, fx.at(8))
        .unwrap()
        .item
        .version;
    let done = fx
        .store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(v),
            StateKey::parse("shipped").unwrap(),
            None,
            Some("shipped with aliased guards"),
            &actor,
            fx.at(9),
            None,
        )
        .unwrap();
    assert_eq!(done.status.as_str(), "shipped");

    let events = fx.store.item_events(&epic.identifier, 100).unwrap();
    let change = events.iter().find(|e| e.kind == "status_changed").unwrap();
    assert_eq!(change.payload["workflow_id"], "delivery-alias");
    assert_eq!(change.payload["workflow_version"], 4);
}

#[test]
fn item_type_changes_do_not_reinterpret_the_workflow_snapshot() {
    let mut fx = Fx::new();
    let actor = fx.agent("agent-a", "sess-type");
    let item = fx.create("Rename my type");
    let (_c, token) = fx.claim(&item.identifier, &actor, fx.at(0));
    assert_eq!(item.workflow_id, "apm-default");

    let updated = fx
        .store
        .update_item(
            &item.identifier,
            Some(&token),
            Some(1),
            ItemPatch {
                item_type: Some("Epic".into()),
                ..Default::default()
            },
            &actor,
            fx.at(1),
            None,
        )
        .unwrap();
    assert_eq!(updated.item_type, "epic");
    assert_eq!(
        updated.workflow_id, "apm-default",
        "changing the type must not silently migrate the workflow snapshot"
    );
    assert_eq!(updated.workflow_version, 1);

    let err = fx
        .store
        .update_item(
            &item.identifier,
            Some(&token),
            Some(updated.version),
            ItemPatch {
                item_type: Some("bad type".into()),
                ..Default::default()
            },
            &actor,
            fx.at(2),
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "invalid_input");
}

#[test]
fn upgrade_plan_refuses_to_overwrite_a_concurrently_changed_item() {
    let mut fx = Fx::new();
    let actor = fx.agent("agent-a", "sess-cas");
    let mut v1 = delivery_workflow();
    v1.version = 1;
    fx.store
        .register_workflow(Some(&fx.project), v1, &actor, fx.at(0))
        .unwrap();
    let v2 = delivery_workflow(); // version 2, same states
    fx.store
        .register_workflow(Some(&fx.project), v2, &actor, fx.at(1))
        .unwrap();
    fx.store
        .bind_type_workflow(Some(&fx.project), "epic", "delivery", 1, &actor, fx.at(2))
        .unwrap();

    let epic = fx
        .store
        .create_item(
            Some(&fx.project),
            NewItem {
                title: "CAS epic".into(),
                context: fx.doc("ship", "intake"),
                item_type: Some("epic".into()),
                priority: None,
                parent: None,
                depends_on: vec![],
                labels: vec![],
                acceptance: vec![],
                actor: actor.clone(),
            },
            fx.at(3),
            None,
        )
        .unwrap()
        .item;
    let (_c, token) = fx.claim(&epic.identifier, &actor, fx.at(4));
    fx.store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(1),
            StateKey::parse("building").unwrap(),
            None,
            None,
            &actor,
            fx.at(5),
            None,
        )
        .unwrap();

    // Plan against the current state/version.
    let plan = fx
        .store
        .plan_workflow_upgrade(
            Some(&fx.project),
            "epic",
            "delivery",
            2,
            &std::collections::BTreeMap::new(),
            true,
        )
        .unwrap();
    assert_eq!(plan.migrated, 1);

    // Another writer moves the item before the plan is applied.
    let v = fx
        .store
        .get_item(&epic.identifier, fx.at(6))
        .unwrap()
        .item
        .version;
    fx.store
        .set_status(
            &epic.identifier,
            Some(&token),
            Some(v),
            StateKey::parse("verifying").unwrap(),
            None,
            None,
            &actor,
            fx.at(7),
            None,
        )
        .unwrap();

    let err = fx
        .store
        .apply_workflow_upgrade(Some(&fx.project), plan, &actor, fx.at(8))
        .unwrap_err();
    assert_eq!(err.code(), "version_conflict");
    let current = fx.store.get_item(&epic.identifier, fx.at(8)).unwrap().item;
    assert_eq!(
        current.status.as_str(),
        "verifying",
        "newer state must survive"
    );
    assert_eq!(current.workflow_version, 1, "rolled back snapshot change");
    assert!(
        !fx.store
            .item_events(&epic.identifier, 100)
            .unwrap()
            .iter()
            .any(|e| e.kind == "workflow_migrated"),
        "no audit event for a rolled-back upgrade"
    );

    // Re-planning and applying now succeeds, and the event carries versions.
    let plan = fx
        .store
        .plan_workflow_upgrade(
            Some(&fx.project),
            "epic",
            "delivery",
            2,
            &std::collections::BTreeMap::new(),
            true,
        )
        .unwrap();
    let report = fx
        .store
        .apply_workflow_upgrade(Some(&fx.project), plan, &actor, fx.at(9))
        .unwrap();
    assert_eq!(report.migrated, 1);
    let current = fx.store.get_item(&epic.identifier, fx.at(9)).unwrap().item;
    assert_eq!(current.workflow_version, 2);
    let event = fx
        .store
        .item_events(&epic.identifier, 100)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "workflow_migrated")
        .unwrap();
    assert_eq!(event.prev_version, Some(3));
    assert_eq!(event.new_version, Some(4));
    assert_eq!(event.payload["to_category"], "review");
}
