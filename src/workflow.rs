//! Declarative default workflow.
//!
//! The workflow is data, not hard-coded `match` arms: `apm workflow --json`
//! emits the exact table the engine enforces, and a project can start from that
//! output. In v1 the six business states are fixed (they are a Rust enum and a
//! schema value); what is configurable is the transition table and its guards.
//! That boundary is documented in README.md.
//!
//! Guards are intentionally few and objective:
//!
//! * `reason` — a non-empty justification is required (blocking, reopening, …);
//! * `evidence_or_acceptance` — at least one met/waived acceptance criterion or
//!   one evidence record exists;
//! * `progress_or_note` — the item carries a progress summary or a completion
//!   note, so "done" is never a bare status flip.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::model::Status;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    pub id: String,
    pub version: u32,
    /// Hex colour hints for UIs; also documents each state's `category`.
    pub states: Vec<StateDef>,
    pub transitions: Vec<TransitionDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateDef {
    pub key: String,
    pub label: String,
    pub category: String,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionDef {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub requires: Vec<String>,
}

impl Workflow {
    pub fn default_v1() -> Workflow {
        let states = Status::ALL
            .iter()
            .map(|s| StateDef {
                key: s.as_str().to_string(),
                label: match s {
                    Status::Ready => "Ready",
                    Status::Doing => "Doing",
                    Status::Blocked => "Blocked",
                    Status::Review => "Review",
                    Status::Done => "Done",
                    Status::Cancelled => "Cancelled",
                }
                .to_string(),
                category: s.category().to_string(),
                terminal: !s.is_open(),
            })
            .collect();

        let t = |from: Status, to: Status, requires: &[&str]| TransitionDef {
            from: from.as_str().to_string(),
            to: to.as_str().to_string(),
            requires: requires.iter().map(|s| s.to_string()).collect(),
        };
        let completion = ["evidence_or_acceptance", "progress_or_note"];

        let transitions = vec![
            t(Status::Ready, Status::Doing, &[]),
            t(Status::Ready, Status::Blocked, &["reason"]),
            t(Status::Ready, Status::Cancelled, &["reason"]),
            t(Status::Doing, Status::Ready, &["reason"]),
            t(Status::Doing, Status::Blocked, &["reason"]),
            t(Status::Doing, Status::Review, &[]),
            t(Status::Doing, Status::Done, &completion),
            t(Status::Doing, Status::Cancelled, &["reason"]),
            t(Status::Blocked, Status::Ready, &["reason"]),
            t(Status::Blocked, Status::Doing, &["reason"]),
            t(Status::Blocked, Status::Cancelled, &["reason"]),
            t(Status::Review, Status::Doing, &["reason"]),
            t(Status::Review, Status::Blocked, &["reason"]),
            t(Status::Review, Status::Done, &completion),
            t(Status::Review, Status::Cancelled, &["reason"]),
            t(Status::Done, Status::Doing, &["reason"]),
            t(Status::Cancelled, Status::Ready, &["reason"]),
        ];

        Workflow {
            id: "apm-default".to_string(),
            version: 1,
            states,
            transitions,
        }
    }

    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse and validate a workflow document (e.g. from project settings).
    pub fn from_json(raw: &str) -> Result<Workflow> {
        let wf: Workflow = serde_json::from_str(raw)
            .map_err(|e| Error::invalid(format!("workflow document is not valid JSON: {e}")))?;
        wf.validate()?;
        Ok(wf)
    }

    pub fn validate(&self) -> Result<()> {
        const KNOWN_GUARDS: [&str; 3] = ["reason", "evidence_or_acceptance", "progress_or_note"];
        for state in &self.states {
            if Status::parse(&state.key).is_err() {
                return Err(Error::invalid(format!(
                    "workflow declares unknown state '{}': custom states are not supported in v1 \
                     (fixed states: ready, doing, blocked, review, done, cancelled)",
                    state.key
                )));
            }
        }
        if self.states.len() != Status::ALL.len() {
            return Err(Error::invalid(
                "workflow must declare exactly the six v1 states",
            ));
        }
        for tr in &self.transitions {
            Status::parse(&tr.from).map_err(|_| {
                Error::invalid(format!(
                    "workflow transition from unknown state '{}'",
                    tr.from
                ))
            })?;
            Status::parse(&tr.to).map_err(|_| {
                Error::invalid(format!("workflow transition to unknown state '{}'", tr.to))
            })?;
            for guard in &tr.requires {
                if !KNOWN_GUARDS.contains(&guard.as_str()) {
                    return Err(Error::invalid(format!(
                        "workflow guard '{guard}' is not supported (known: {})",
                        KNOWN_GUARDS.join(", ")
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn transition(&self, from: Status, to: Status) -> Option<&TransitionDef> {
        self.transitions
            .iter()
            .find(|t| t.from == from.as_str() && t.to == to.as_str())
    }

    /// Enforce the transition table and its guards.
    pub fn check_transition(&self, ctx: &GuardContext<'_>) -> Result<()> {
        let Some(tr) = self.transition(ctx.from, ctx.to) else {
            let allowed = self
                .transitions
                .iter()
                .filter(|t| t.from == ctx.from.as_str())
                .map(|t| t.to.as_str())
                .collect::<Vec<_>>();
            return Err(Error::WorkflowViolation(format!(
                "transition {} -> {} is not allowed (from {} you may go to: {})",
                ctx.from.as_str(),
                ctx.to.as_str(),
                ctx.from.as_str(),
                if allowed.is_empty() {
                    "nothing; the state is terminal".to_string()
                } else {
                    allowed.join(", ")
                }
            )));
        };
        for guard in &tr.requires {
            match guard.as_str() {
                "reason" => {
                    let reason = ctx.reason.unwrap_or("").trim();
                    if reason.is_empty() {
                        return Err(Error::WorkflowViolation(format!(
                            "transition {} -> {} requires --reason",
                            ctx.from.as_str(),
                            ctx.to.as_str()
                        )));
                    }
                }
                "evidence_or_acceptance" => {
                    // Acceptance criteria are the contract. If the item defines
                    // any, *all* of them must be met or waived — a single green
                    // criterion does not make a two-criterion item done. Only an
                    // item with no criteria may be completed on evidence alone.
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
                other => {
                    return Err(Error::Internal(format!("unknown guard '{other}'")));
                }
            }
        }
        Ok(())
    }
}

pub struct GuardContext<'a> {
    pub from: Status,
    pub to: Status,
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

    fn ctx<'a>(from: Status, to: Status) -> GuardContext<'a> {
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
        assert!(parsed.transition(Status::Doing, Status::Done).is_some());
        assert!(parsed.transition(Status::Ready, Status::Done).is_none());
    }

    #[test]
    fn done_requires_evidence_and_progress() {
        let wf = Workflow::default_v1();
        let mut c = ctx(Status::Doing, Status::Done);
        // No criteria, no evidence.
        assert!(wf.check_transition(&c).is_err());

        c.evidence_count = 1;
        assert!(wf.check_transition(&c).is_err(), "progress still missing");

        c.progress_summary = Some("tests green");
        assert!(wf.check_transition(&c).is_ok());
    }

    #[test]
    fn done_requires_every_acceptance_criterion() {
        let wf = Workflow::default_v1();
        let mut c = ctx(Status::Doing, Status::Done);
        c.acceptance_total = 2;
        c.progress_summary = Some("half the criteria");
        c.met_acceptance = 1;
        let err = wf.check_transition(&c).unwrap_err();
        assert_eq!(err.code(), "completion_evidence_required");

        c.met_acceptance = 2;
        assert!(wf.check_transition(&c).is_ok());

        // Evidence cannot substitute for an unmet criterion.
        c.met_acceptance = 1;
        c.evidence_count = 5;
        assert!(wf.check_transition(&c).is_err());
    }

    #[test]
    fn blocked_and_reopen_require_reason() {
        let wf = Workflow::default_v1();
        let c = ctx(Status::Doing, Status::Blocked);
        assert!(wf.check_transition(&c).is_err());
        let mut c = ctx(Status::Done, Status::Doing);
        assert!(wf.check_transition(&c).is_err());
        c.reason = Some("regression found");
        assert!(wf.check_transition(&c).is_ok());
    }

    #[test]
    fn unknown_state_is_rejected() {
        let mut wf = Workflow::default_v1();
        wf.states.push(StateDef {
            key: "icebox".into(),
            label: "Icebox".into(),
            category: "open".into(),
            terminal: false,
        });
        assert!(wf.validate().is_err());
    }
}
