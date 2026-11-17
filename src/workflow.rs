//! Versioned, declarative workflows.
//!
//! A workflow is data: a set of states (each mapped to one dashboard
//! [`Category`] and a `terminal` flag), an initial state, transitions, and a
//! named guard library. The default workflow reproduces the original six-state
//! machine exactly, so existing databases and agents keep working.
//!
//! Validation is intentionally strict, because the schema is the product:
//!
//! * state keys are unique slugs, the initial state exists and is not closed;
//! * `terminal` and `Category::Closed` must agree;
//! * every state must be reachable from the initial state, and every non-closed
//!   state must have an outgoing transition — no un-flowable orphan states;
//! * transitions may only reference declared states, and every guard they name
//!   must exist in the workflow's guard library with a known kind;
//! * every transition **into a closed state** must require both the
//!   `progress_or_note` and `evidence_or_acceptance` guards, so a custom workflow
//!   can never weaken the completion rules.
//!
//! Reopen transitions out of a closed state are allowed (the default workflow has
//! `done -> doing`); they require a `reason`, which the audit trail records.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::{Category, StateKey};

pub const DEFAULT_WORKFLOW_ID: &str = "apm-default";
pub const DEFAULT_WORKFLOW_VERSION: i64 = 1;

/// Guard kinds the engine knows how to evaluate. A workflow declares which
/// guards it offers and transitions reference them by name.
pub const GUARD_KINDS: [&str; 3] = ["reason", "evidence_or_acceptance", "progress_or_note"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub version: i64,
    #[serde(default)]
    pub name: String,
    /// State the item starts in. Defaults to the first non-closed state for
    /// legacy documents that predate this field.
    #[serde(default)]
    pub initial: String,
    pub states: Vec<StateDef>,
    pub transitions: Vec<TransitionDef>,
    /// Named guard library. Transitions reference these names in `requires`.
    #[serde(default)]
    pub guards: BTreeMap<String, GuardDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDef {
    pub key: String,
    #[serde(default)]
    pub label: String,
    pub category: Category,
    /// Closed outcome flag. Must agree with `category == closed`.
    #[serde(default)]
    pub terminal: bool,
    /// A closed state reached by *cancelling* rather than completing. Cancel
    /// transitions require a reason; completion transitions must require the
    /// evidence/progress guards.
    #[serde(default)]
    pub cancel: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionDef {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub requires: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardDef {
    /// One of [`GUARD_KINDS`].
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl GuardDef {
    fn new(kind: &str, description: &str) -> GuardDef {
        GuardDef {
            kind: kind.to_string(),
            description: Some(description.to_string()),
        }
    }
}

impl Workflow {
    /// The original six-state machine, expressed in the v2 schema. Behaviour is
    /// identical to pre-workflow releases.
    pub fn default_v1() -> Workflow {
        let state =
            |key: &str, label: &str, category: Category, terminal: bool, cancel: bool| StateDef {
                key: key.to_string(),
                label: label.to_string(),
                category,
                terminal,
                cancel,
            };
        let t = |from: &str, to: &str, requires: &[&str]| TransitionDef {
            from: from.to_string(),
            to: to.to_string(),
            requires: requires.iter().map(|s| s.to_string()).collect(),
        };
        let completion = ["evidence_or_acceptance", "progress_or_note"];

        Workflow {
            id: DEFAULT_WORKFLOW_ID.to_string(),
            version: DEFAULT_WORKFLOW_VERSION,
            name: "Default workflow".to_string(),
            initial: "ready".to_string(),
            states: vec![
                state("ready", "Ready", Category::Open, false, false),
                state("doing", "Doing", Category::Active, false, false),
                state("blocked", "Blocked", Category::Waiting, false, false),
                state("review", "Review", Category::Review, false, false),
                state("done", "Done", Category::Closed, true, false),
                state("cancelled", "Cancelled", Category::Closed, true, true),
            ],
            transitions: vec![
                t("ready", "doing", &[]),
                t("ready", "blocked", &["reason"]),
                t("ready", "cancelled", &["reason"]),
                t("doing", "ready", &["reason"]),
                t("doing", "blocked", &["reason"]),
                t("doing", "review", &[]),
                t("doing", "done", &completion),
                t("doing", "cancelled", &["reason"]),
                t("blocked", "ready", &["reason"]),
                t("blocked", "doing", &["reason"]),
                t("blocked", "cancelled", &["reason"]),
                t("review", "doing", &["reason"]),
                t("review", "blocked", &["reason"]),
                t("review", "done", &completion),
                t("review", "cancelled", &["reason"]),
                t("done", "doing", &["reason"]),
                t("cancelled", "ready", &["reason"]),
            ],
            guards: builtin_guards(),
        }
    }

    pub fn builtin_guards() -> BTreeMap<String, GuardDef> {
        builtin_guards()
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse and validate a workflow document. Legacy documents (no `initial`,
    /// no `guards`, no `name`) are upgraded in memory first.
    pub fn from_json(raw: &str) -> Result<Workflow> {
        let mut wf: Workflow = serde_json::from_str(raw)
            .map_err(|e| Error::invalid(format!("workflow document is not valid JSON: {e}")))?;
        wf.normalise()?;
        wf.validate()?;
        Ok(wf)
    }

    /// Normalise a document into canonical form (trim/lowercase keys, fill
    /// defaults). Validation still has to pass afterwards.
    pub fn normalise(&mut self) -> Result<()> {
        self.id = self.id.trim().to_ascii_lowercase();
        if self.id.is_empty() {
            return Err(Error::invalid("workflow id must be non-empty"));
        }
        if self.version < 1 {
            return Err(Error::invalid("workflow version must be >= 1"));
        }
        if self.name.trim().is_empty() {
            self.name = self.id.clone();
        }
        for state in &mut self.states {
            state.key = StateKey::parse(&state.key)?.into_string();
            if state.label.trim().is_empty() {
                state.label = state.key.clone();
            }
        }
        for tr in &mut self.transitions {
            tr.from = StateKey::parse(&tr.from)?.into_string();
            tr.to = StateKey::parse(&tr.to)?.into_string();
            tr.requires = tr
                .requires
                .iter()
                .map(|g| g.trim().to_ascii_lowercase())
                .filter(|g| !g.is_empty())
                .collect();
            tr.requires.sort();
            tr.requires.dedup();
        }
        if self.initial.trim().is_empty() {
            self.initial = self
                .states
                .iter()
                .find(|s| !s.category.is_closed())
                .or_else(|| self.states.first())
                .map(|s| s.key.clone())
                .unwrap_or_default();
        } else {
            self.initial = StateKey::parse(&self.initial)?.into_string();
        }
        if self.guards.is_empty() {
            self.guards = builtin_guards();
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.states.is_empty() {
            return Err(Error::invalid("workflow must declare at least one state"));
        }
        if self.states.len() < 2 {
            return Err(Error::invalid(
                "workflow must declare at least one non-closed state and one closed state",
            ));
        }

        let mut keys = BTreeSet::new();
        for state in &self.states {
            StateKey::parse(&state.key)?;
            if !keys.insert(state.key.clone()) {
                return Err(Error::invalid(format!(
                    "workflow declares duplicate state '{}'",
                    state.key
                )));
            }
            if state.terminal != state.category.is_closed() {
                return Err(Error::invalid(format!(
                    "state '{}': terminal must be true exactly when category is 'closed'",
                    state.key
                )));
            }
            if state.cancel && !state.terminal {
                return Err(Error::invalid(format!(
                    "state '{}': only a closed state can be a cancel outcome",
                    state.key
                )));
            }
        }

        let Some(initial) = self.state(&self.initial) else {
            return Err(Error::invalid(format!(
                "initial state '{}' is not declared by the workflow",
                self.initial
            )));
        };
        if initial.category.is_closed() {
            return Err(Error::invalid("initial state must not be a closed state"));
        }

        for guard_name in self.guards.keys() {
            StateKey::parse(guard_name).map_err(|_| {
                Error::invalid(format!("guard name '{guard_name}' is not a valid slug"))
            })?;
        }
        for (name, guard) in &self.guards {
            if !GUARD_KINDS.contains(&guard.kind.as_str()) {
                return Err(Error::invalid(format!(
                    "guard '{name}' uses unsupported kind '{}' (known: {})",
                    guard.kind,
                    GUARD_KINDS.join(", ")
                )));
            }
        }

        let mut seen = BTreeSet::new();
        for tr in &self.transitions {
            let Some(from) = self.state(&tr.from) else {
                return Err(Error::invalid(format!(
                    "transition references unknown source state '{}'",
                    tr.from
                )));
            };
            let Some(to) = self.state(&tr.to) else {
                return Err(Error::invalid(format!(
                    "transition references unknown target state '{}'",
                    tr.to
                )));
            };
            if tr.from == tr.to {
                return Err(Error::invalid(format!(
                    "transition '{} -> {}' is a self-loop",
                    tr.from, tr.to
                )));
            }
            if !seen.insert((tr.from.clone(), tr.to.clone())) {
                return Err(Error::invalid(format!(
                    "duplicate transition '{} -> {}'",
                    tr.from, tr.to
                )));
            }
            for guard in &tr.requires {
                if !self.guards.contains_key(guard) {
                    return Err(Error::invalid(format!(
                        "transition '{} -> {}' requires undeclared guard '{guard}'",
                        tr.from, tr.to
                    )));
                }
            }
            // Guards are a named library: what matters is the *kind* a transition
            // requires, not the literal name. A workflow may alias
            // `evidence_or_acceptance` as `completion_evidence`, for example.
            let required_kinds: BTreeSet<&str> = tr
                .requires
                .iter()
                .filter_map(|name| self.guards.get(name))
                .map(|guard| guard.kind.as_str())
                .collect();
            // Defence in depth: a workflow must not be able to weaken completion.
            if to.category.is_closed() {
                if to.cancel {
                    if !required_kinds.contains("reason") {
                        return Err(Error::invalid(format!(
                            "transition '{} -> {}' enters cancel state '{}' but requires no guard of \
                             kind 'reason'",
                            tr.from, tr.to, tr.to
                        )));
                    }
                } else {
                    for required in ["progress_or_note", "evidence_or_acceptance"] {
                        if !required_kinds.contains(required) {
                            return Err(Error::invalid(format!(
                                "transition '{} -> {}' enters closed state '{}' but requires no \
                                 guard of kind '{required}'; completion guards cannot be weakened",
                                tr.from, tr.to, tr.to
                            )));
                        }
                    }
                }
            }
            let _ = from;
        }

        // Every non-closed state needs an exit, otherwise items can get stuck.
        for state in &self.states {
            if state.category.is_closed() {
                continue;
            }
            let has_exit = self.transitions.iter().any(|t| t.from == state.key);
            if !has_exit {
                return Err(Error::invalid(format!(
                    "state '{}' has no outgoing transition (dead end)",
                    state.key
                )));
            }
        }

        // Every state must be reachable from the initial state.
        let mut reachable: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(self.initial.clone());
        reachable.insert(self.initial.clone());
        while let Some(current) = queue.pop_front() {
            for tr in self.transitions.iter().filter(|t| t.from == current) {
                if reachable.insert(tr.to.clone()) {
                    queue.push_back(tr.to.clone());
                }
            }
        }
        if let Some(orphan) = self.states.iter().find(|s| !reachable.contains(&s.key)) {
            return Err(Error::invalid(format!(
                "state '{}' is unreachable from initial state '{}'; the workflow would create \
                 un-flowable items",
                orphan.key, self.initial
            )));
        }

        if !self.states.iter().any(|s| s.category.is_closed()) {
            return Err(Error::invalid(
                "workflow must declare at least one closed (terminal) state",
            ));
        }
        Ok(())
    }

    pub fn state(&self, key: &str) -> Option<&StateDef> {
        self.states.iter().find(|s| s.key == key)
    }

    pub fn category_of(&self, key: &str) -> Option<Category> {
        self.state(key).map(|s| s.category)
    }

    pub fn is_terminal(&self, key: &str) -> bool {
        self.state(key).map(|s| s.terminal).unwrap_or(false)
    }

    pub fn initial_state(&self) -> &StateDef {
        self.state(&self.initial)
            .or_else(|| self.states.first())
            .expect("validated workflows have states")
    }

    pub fn transition(&self, from: &str, to: &str) -> Option<&TransitionDef> {
        self.transitions
            .iter()
            .find(|t| t.from == from && t.to == to)
    }

    /// Enforce the transition table and its guards against the item's snapshot.
    pub fn check_transition(&self, ctx: &GuardContext<'_>) -> Result<()> {
        if self.state(ctx.to.as_str()).is_none() {
            return Err(Error::WorkflowViolation(format!(
                "state '{}' is not part of workflow {} v{}",
                ctx.to, self.id, self.version
            )));
        }
        if self.state(ctx.from.as_str()).is_none() {
            return Err(Error::WorkflowViolation(format!(
                "current state '{}' is not part of workflow {} v{}; the item needs migration",
                ctx.from, self.id, self.version
            )));
        }
        let Some(tr) = self.transition(ctx.from.as_str(), ctx.to.as_str()) else {
            let allowed = self
                .transitions
                .iter()
                .filter(|t| t.from == ctx.from.as_str())
                .map(|t| t.to.as_str())
                .collect::<Vec<_>>();
            return Err(Error::WorkflowViolation(format!(
                "transition {} -> {} is not allowed in workflow {} v{} (from {} you may go to: {})",
                ctx.from,
                ctx.to,
                self.id,
                self.version,
                ctx.from,
                if allowed.is_empty() {
                    "nothing; the state is terminal".to_string()
                } else {
                    allowed.join(", ")
                }
            )));
        };
        for guard_name in &tr.requires {
            let guard = self
                .guards
                .get(guard_name)
                .ok_or_else(|| Error::Internal(format!("unknown guard '{guard_name}'")))?;
            match guard.kind.as_str() {
                "reason" => {
                    let reason = ctx.reason.unwrap_or("").trim();
                    if reason.is_empty() {
                        return Err(Error::WorkflowViolation(format!(
                            "transition {} -> {} requires --reason",
                            ctx.from, ctx.to
                        )));
                    }
                }
                "evidence_or_acceptance" => {
                    // Acceptance criteria are the contract. If the item defines
                    // any, *all* of them must be met or waived — a single green
                    // criterion does not make a multi-criterion item done. Only
                    // an item with no criteria may complete on evidence alone.
                    if ctx.acceptance_total > 0 {
                        if ctx.met_acceptance < ctx.acceptance_total {
                            return Err(Error::CompletionEvidenceRequired(format!(
                                "completing requires every acceptance criterion to be met or \
                                 waived ({}/{} done); mark them with `apm accept set`",
                                ctx.met_acceptance, ctx.acceptance_total
                            )));
                        }
                    } else if ctx.evidence_count == 0 {
                        return Err(Error::CompletionEvidenceRequired(
                            "completing requires verification evidence (`apm evidence add`) when \
                             the item defines no acceptance criteria"
                                .into(),
                        ));
                    }
                }
                "progress_or_note" => {
                    let has_progress = ctx
                        .progress_summary
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false);
                    let has_note = ctx
                        .completion_note
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false);
                    if !has_progress && !has_note {
                        return Err(Error::CompletionEvidenceRequired(
                            "completing requires a progress summary (`apm progress`) or a completion \
                             note (`--note`); a bare status flip is not accepted"
                                .into(),
                        ));
                    }
                }
                other => return Err(Error::Internal(format!("unsupported guard kind '{other}'"))),
            }
        }
        Ok(())
    }
}

fn builtin_guards() -> BTreeMap<String, GuardDef> {
    let mut guards = BTreeMap::new();
    guards.insert(
        "reason".to_string(),
        GuardDef::new(
            "reason",
            "a non-empty justification is required (blocking, reopening, cancelling)",
        ),
    );
    guards.insert(
        "evidence_or_acceptance".to_string(),
        GuardDef::new(
            "evidence_or_acceptance",
            "all acceptance criteria met/waived, or evidence when none are defined",
        ),
    );
    guards.insert(
        "progress_or_note".to_string(),
        GuardDef::new(
            "progress_or_note",
            "a progress summary or completion note must exist",
        ),
    );
    guards
}

pub struct GuardContext<'a> {
    pub from: StateKey,
    pub to: StateKey,
    pub reason: Option<&'a str>,
    pub note: Option<&'a str>,
    pub progress_summary: Option<&'a str>,
    pub completion_note: Option<&'a str>,
    pub met_acceptance: i64,
    pub evidence_count: i64,
    pub acceptance_total: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(from: StateKey, to: StateKey) -> GuardContext<'a> {
        GuardContext {
            from,
            to,
            reason: None,
            note: None,
            progress_summary: None,
            completion_note: None,
            met_acceptance: 0,
            evidence_count: 0,
            acceptance_total: 0,
        }
    }

    #[test]
    fn default_workflow_roundtrips_through_json() {
        let wf = Workflow::default_v1();
        let json = wf.to_json().unwrap();
        let parsed = Workflow::from_json(&json).unwrap();
        assert_eq!(parsed.transitions.len(), wf.transitions.len());
        assert!(parsed.transition("doing", "done").is_some());
        assert!(parsed.transition("ready", "done").is_none());
        assert_eq!(parsed.initial, "ready");
        assert_eq!(parsed.states.len(), 6);
    }

    #[test]
    fn legacy_document_without_new_fields_upgrades() {
        let legacy = serde_json::json!({
            "id": "apm-default",
            "version": 1,
            "states": [
                {"key": "ready", "label": "Ready", "category": "open", "terminal": false},
                {"key": "done", "label": "Done", "category": "closed", "terminal": true}
            ],
            "transitions": [
                {"from": "ready", "to": "done", "requires": ["evidence_or_acceptance", "progress_or_note"]},
                {"from": "done", "to": "ready", "requires": ["reason"]}
            ]
        });
        let wf = Workflow::from_json(&legacy.to_string()).unwrap();
        assert_eq!(wf.initial, "ready");
        assert!(wf.guards.contains_key("reason"));
    }

    #[test]
    fn done_requires_evidence_and_progress() {
        let wf = Workflow::default_v1();
        let mut c = ctx(StateKey::doing(), StateKey::done());
        assert!(wf.check_transition(&c).is_err());

        c.evidence_count = 1;
        assert!(wf.check_transition(&c).is_err(), "progress still missing");

        c.progress_summary = Some("tests green");
        assert!(wf.check_transition(&c).is_ok());
    }

    #[test]
    fn done_requires_every_acceptance_criterion() {
        let wf = Workflow::default_v1();
        let mut c = ctx(StateKey::doing(), StateKey::done());
        c.acceptance_total = 2;
        c.progress_summary = Some("half the criteria");
        c.met_acceptance = 1;
        let err = wf.check_transition(&c).unwrap_err();
        assert_eq!(err.code(), "completion_evidence_required");

        c.met_acceptance = 2;
        assert!(wf.check_transition(&c).is_ok());

        c.met_acceptance = 1;
        c.evidence_count = 5;
        assert!(wf.check_transition(&c).is_err());
    }

    #[test]
    fn blocked_and_reopen_require_reason() {
        let wf = Workflow::default_v1();
        let c = ctx(StateKey::doing(), StateKey::blocked());
        assert!(wf.check_transition(&c).is_err());
        let mut c = ctx(StateKey::done(), StateKey::doing());
        assert!(wf.check_transition(&c).is_err());
        c.reason = Some("regression found");
        assert!(wf.check_transition(&c).is_ok());
    }

    #[test]
    fn custom_workflow_validates() {
        let mut wf = Workflow::default_v1();
        wf.id = "delivery".into();
        wf.version = 2;
        wf.states.push(StateDef {
            key: "deploying".into(),
            label: "Deploying".into(),
            category: Category::Active,
            terminal: false,
            cancel: false,
        });
        wf.transitions.push(TransitionDef {
            from: "review".into(),
            to: "deploying".into(),
            requires: vec![],
        });
        wf.transitions.push(TransitionDef {
            from: "deploying".into(),
            to: "done".into(),
            requires: vec!["evidence_or_acceptance".into(), "progress_or_note".into()],
        });
        wf.validate().unwrap();
    }

    #[test]
    fn rejects_unreachable_state() {
        let mut wf = Workflow::default_v1();
        wf.states.push(StateDef {
            key: "icebox".into(),
            label: "Icebox".into(),
            category: Category::Open,
            terminal: false,
            cancel: false,
        });
        // Give it an exit so the failure is specifically about reachability.
        wf.transitions.push(TransitionDef {
            from: "icebox".into(),
            to: "done".into(),
            requires: vec!["evidence_or_acceptance".into(), "progress_or_note".into()],
        });
        let err = wf.validate().unwrap_err();
        assert!(err.to_string().contains("unreachable"), "{err}");
    }

    #[test]
    fn rejects_dead_end_and_terminal_mismatch() {
        let mut wf = Workflow::default_v1();
        wf.transitions.retain(|t| t.from != "blocked");
        assert!(wf.validate().unwrap_err().to_string().contains("dead end"));

        let mut wf = Workflow::default_v1();
        wf.states[0].terminal = true; // ready is open
        assert!(wf.validate().unwrap_err().to_string().contains("terminal"));
    }

    #[test]
    fn rejects_weakened_completion_guards() {
        let mut wf = Workflow::default_v1();
        for tr in &mut wf.transitions {
            if tr.to == "done" {
                tr.requires.retain(|g| g != "evidence_or_acceptance");
            }
        }
        let err = wf.validate().unwrap_err();
        assert!(err.to_string().contains("cannot be weakened"), "{err}");
    }

    #[test]
    fn rejects_unknown_guard_and_bad_transition() {
        let mut wf = Workflow::default_v1();
        wf.transitions[0].requires.push("nope".into());
        assert!(
            wf.validate()
                .unwrap_err()
                .to_string()
                .contains("undeclared guard")
        );

        let mut wf = Workflow::default_v1();
        wf.transitions.push(TransitionDef {
            from: "ready".into(),
            to: "ghost".into(),
            requires: vec![],
        });
        assert!(
            wf.validate()
                .unwrap_err()
                .to_string()
                .contains("unknown target")
        );

        let mut wf = Workflow::default_v1();
        wf.guards.get_mut("reason").unwrap().kind = "magic".into();
        assert!(
            wf.validate()
                .unwrap_err()
                .to_string()
                .contains("unsupported kind")
        );
    }

    #[test]
    fn unknown_state_is_rejected_at_transition_time() {
        let wf = Workflow::default_v1();
        let c = ctx(StateKey::doing(), StateKey::parse("nope").unwrap());
        let err = wf.check_transition(&c).unwrap_err();
        assert_eq!(err.code(), "workflow_violation");
    }
}
