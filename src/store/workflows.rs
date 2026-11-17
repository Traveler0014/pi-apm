//! Versioned workflow storage and per-type bindings.
//!
//! A workflow document is stored per `(project, id, version)`; registering the
//! same id/version with different content is rejected instead of silently
//! rewriting history. Items snapshot `(workflow_id, workflow_version)` when they
//! are created, and transitions are always validated against that snapshot, so
//! re-binding an item type never reinterprets an existing item.
//!
//! The store is the only writer: `register_workflow` validates the document, and
//! `bind_type_workflow` only accepts a workflow version that exists.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::*;
use crate::workflow::{DEFAULT_WORKFLOW_ID, DEFAULT_WORKFLOW_VERSION, Workflow};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSummary {
    pub id: String,
    pub version: i64,
    pub name: String,
    pub initial: String,
    pub states: i64,
    pub is_project_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeWorkflowBinding {
    pub item_type: String,
    pub workflow_id: String,
    pub workflow_version: i64,
}

impl Store {
    /// Register (or idempotently confirm) a workflow version.
    pub fn register_workflow(
        &mut self,
        project: Option<&str>,
        mut workflow: Workflow,
        actor: &Actor,
        at: i64,
    ) -> Result<Workflow> {
        workflow.normalise()?;
        workflow.validate()?;
        let project_row = self.require_project(project)?;
        let canonical = workflow.to_json()?;

        self.with_tx(|tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT definition_json FROM workflows
                     WHERE project_id = ?1 AND id = ?2 AND version = ?3",
                    params![project_row.id, workflow.id, workflow.version],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(stored) = existing {
                if stored == canonical {
                    return Ok(workflow.clone());
                }
                return Err(Error::invalid(format!(
                    "workflow {} v{} is already registered with different content; bump the version",
                    workflow.id, workflow.version
                )));
            }
            tx.execute(
                "INSERT INTO workflows
                 (project_id, id, version, name, definition_json, created_at, created_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    project_row.id,
                    workflow.id,
                    workflow.version,
                    workflow.name,
                    canonical,
                    at,
                    actor.name
                ],
            )?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: None,
                    kind: "workflow_registered",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key: None,
                    payload: json!({
                        "workflow_id": workflow.id,
                        "workflow_version": workflow.version,
                        "states": workflow.states.len(),
                        "initial": workflow.initial,
                    }),
                },
            )?;
            Ok(workflow.clone())
        })
    }

    pub fn workflow_by_id(
        &self,
        project: Option<&str>,
        id: &str,
        version: i64,
    ) -> Result<Workflow> {
        let project_row = self.require_project(project)?;
        let json: String = self
            .conn
            .query_row(
                "SELECT definition_json FROM workflows
                 WHERE project_id = ?1 AND id = ?2 AND version = ?3",
                params![project_row.id, id, version],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                entity: "workflow",
                id: format!("{id}@{version}"),
            })?;
        Workflow::from_json(&json)
    }

    pub fn list_workflows(&self, project: Option<&str>) -> Result<Vec<WorkflowSummary>> {
        let project_row = self.require_project(project)?;
        let default_id = project_row
            .default_workflow_id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKFLOW_ID.to_string());
        let default_version = project_row
            .default_workflow_version
            .unwrap_or(DEFAULT_WORKFLOW_VERSION);
        let mut stmt = self.conn.prepare(
            "SELECT id, version, name, definition_json FROM workflows
             WHERE project_id = ?1 ORDER BY id, version",
        )?;
        let rows = stmt
            .query_map(params![project_row.id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (id, version, name, json) in rows {
            let wf = Workflow::from_json(&json)?;
            out.push(WorkflowSummary {
                id: id.clone(),
                version,
                name,
                initial: wf.initial.clone(),
                states: wf.states.len() as i64,
                is_project_default: id == default_id && version == default_version,
            });
        }
        Ok(out)
    }

    /// Bind an item type to a workflow version. Existing items keep the snapshot
    /// they were created with; this only affects items created afterwards.
    pub fn bind_type_workflow(
        &mut self,
        project: Option<&str>,
        item_type: &str,
        workflow_id: &str,
        workflow_version: i64,
        actor: &Actor,
        at: i64,
    ) -> Result<TypeWorkflowBinding> {
        let item_type = item_type_key(item_type)?;
        let project_row = self.require_project(project)?;
        // Fail before the transaction if the workflow does not exist.
        let workflow = self.workflow_by_id(Some(&project_row.id), workflow_id, workflow_version)?;
        self.with_tx(|tx| {
            let previous: Option<(String, i64)> = tx
                .query_row(
                    "SELECT workflow_id, workflow_version FROM type_workflows
                     WHERE project_id = ?1 AND item_type = ?2",
                    params![project_row.id, item_type],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            // Same binding: no UPDATE, no event (idempotent replay).
            if let Some((existing_id, existing_version)) = &previous
                && *existing_id == workflow.id
                && *existing_version == workflow.version
            {
                return Ok(TypeWorkflowBinding {
                    item_type,
                    workflow_id: workflow.id,
                    workflow_version: workflow.version,
                });
            }
            tx.execute(
                "INSERT INTO type_workflows
                 (project_id, item_type, workflow_id, workflow_version, updated_at, updated_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(project_id, item_type) DO UPDATE SET
                    workflow_id = excluded.workflow_id,
                    workflow_version = excluded.workflow_version,
                    updated_at = excluded.updated_at,
                    updated_by = excluded.updated_by",
                params![
                    project_row.id,
                    item_type,
                    workflow.id,
                    workflow.version,
                    at,
                    actor.name
                ],
            )?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: None,
                    kind: "type_workflow_bound",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key: None,
                    payload: json!({
                        "item_type": item_type,
                        "workflow_id": workflow.id,
                        "workflow_version": workflow.version,
                        "previous": previous.map(|(id, v)| json!({"workflow_id": id, "workflow_version": v})),
                    }),
                },
            )?;
            Ok(TypeWorkflowBinding {
                item_type,
                workflow_id: workflow.id,
                workflow_version: workflow.version,
            })
        })
    }

    pub fn type_workflows(&self, project: Option<&str>) -> Result<Vec<TypeWorkflowBinding>> {
        let project_row = self.require_project(project)?;
        let mut stmt = self.conn.prepare(
            "SELECT item_type, workflow_id, workflow_version FROM type_workflows
             WHERE project_id = ?1 ORDER BY item_type",
        )?;
        let rows = stmt
            .query_map(params![project_row.id], |r| {
                Ok(TypeWorkflowBinding {
                    item_type: r.get(0)?,
                    workflow_id: r.get(1)?,
                    workflow_version: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The workflow an item of `item_type` would snapshot right now.
    pub(crate) fn workflow_for_type(&self, project_id: &str, item_type: &str) -> Result<Workflow> {
        let binding: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT workflow_id, workflow_version FROM type_workflows
                 WHERE project_id = ?1 AND item_type = ?2",
                params![project_id, item_type],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (id, version) = binding.unwrap_or_else(|| {
            let row = self.require_project(Some(project_id));
            match row {
                Ok(p) => (
                    p.default_workflow_id
                        .unwrap_or_else(|| DEFAULT_WORKFLOW_ID.to_string()),
                    p.default_workflow_version
                        .unwrap_or(DEFAULT_WORKFLOW_VERSION),
                ),
                Err(_) => (DEFAULT_WORKFLOW_ID.to_string(), DEFAULT_WORKFLOW_VERSION),
            }
        });
        self.load_workflow(project_id, &id, version)
    }

    /// The snapshot an existing item runs on.
    pub(crate) fn workflow_for_item(&self, item: &RawItem) -> Result<Workflow> {
        self.load_workflow(&item.project_id, &item.workflow_id, item.workflow_version)
    }

    pub(crate) fn load_workflow(
        &self,
        project_id: &str,
        id: &str,
        version: i64,
    ) -> Result<Workflow> {
        let json: String = self
            .conn
            .query_row(
                "SELECT definition_json FROM workflows
                 WHERE project_id = ?1 AND id = ?2 AND version = ?3",
                params![project_id, id, version],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound {
                entity: "workflow",
                id: format!("{id}@{version}"),
            })?;
        Workflow::from_json(&json)
    }
}

/// Seed the default workflow for a freshly created project. Called inside
/// `init_project`'s transaction; idempotent.
pub(crate) fn seed_default_workflow(
    tx: &rusqlite::Transaction<'_>,
    project_id: &str,
    at: i64,
    actor: &Actor,
) -> Result<Workflow> {
    let workflow = Workflow::default_v1();
    let canonical = workflow.to_json()?;
    tx.execute(
        "INSERT OR IGNORE INTO workflows
         (project_id, id, version, name, definition_json, created_at, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            project_id,
            workflow.id,
            workflow.version,
            workflow.name,
            canonical,
            at,
            actor.name
        ],
    )?;
    tx.execute(
        "UPDATE projects SET default_workflow_id = ?1, default_workflow_version = ?2
         WHERE id = ?3",
        params![workflow.id, workflow.version, project_id],
    )?;
    Ok(workflow)
}

pub(crate) fn item_type_key(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Error::invalid("item type must be non-empty"));
    }
    // Reuse the state-key slug rules: lowercase, digits, '.', '_' or '-'.
    Ok(StateKey::parse(&trimmed.to_ascii_lowercase())?.into_string())
}

/// One item touched by a workflow upgrade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowUpgradeItem {
    pub identifier: String,
    pub item_id: String,
    pub from_workflow_id: String,
    pub from_workflow_version: i64,
    pub from_state: String,
    /// Optimistic-lock version the plan was computed against.
    pub from_version: i64,
    pub to_state: Option<String>,
    /// `migrate`, `already`, `blocked`, `skipped_closed` or `rebind_only`.
    pub action: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowVersionRef {
    pub workflow_id: String,
    pub workflow_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowUpgradeReport {
    pub item_type: String,
    pub target: WorkflowVersionRef,
    pub binding_before: Option<WorkflowVersionRef>,
    pub dry_run: bool,
    pub migrate_items: bool,
    pub items: Vec<WorkflowUpgradeItem>,
    pub migrated: i64,
    pub skipped: i64,
    pub blocked: i64,
    pub binding_changed: bool,
}

impl Store {
    /// The workflow to display for a project, optionally resolved through an
    /// item-type binding.
    pub fn resolve_workflow(
        &self,
        project: Option<&str>,
        item_type: Option<&str>,
    ) -> Result<Workflow> {
        match item_type {
            Some(item_type) => {
                let project_row = self.require_project(project)?;
                let item_type = item_type_key(item_type)?;
                self.workflow_for_type(&project_row.id, &item_type)
            }
            None => self.workflow(project),
        }
    }

    /// Make a workflow version the project default (used when an item type has
    /// no explicit binding).
    pub fn set_default_workflow(
        &mut self,
        project: Option<&str>,
        id: &str,
        version: i64,
        actor: &Actor,
        at: i64,
    ) -> Result<Workflow> {
        let project_row = self.require_project(project)?;
        let workflow = self.workflow_by_id(Some(&project_row.id), id, version)?;
        let current = (
            project_row.default_workflow_id.clone(),
            project_row.default_workflow_version,
        );
        if current == (Some(workflow.id.clone()), Some(workflow.version)) {
            return Ok(workflow);
        }
        self.with_tx(|tx| {
            tx.execute(
                "UPDATE projects SET default_workflow_id = ?1, default_workflow_version = ?2
                 WHERE id = ?3",
                params![workflow.id, workflow.version, project_row.id],
            )?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at,
                    project_id: &project_row.id,
                    work_item_id: None,
                    kind: "project_default_workflow_changed",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key: None,
                    payload: json!({
                        "workflow_id": workflow.id,
                        "workflow_version": workflow.version,
                    }),
                },
            )?;
            Ok(workflow)
        })
    }

    /// Compute an upgrade plan without writing anything.
    ///
    /// Rules that protect the completion invariants:
    ///
    /// * a mapping target must be declared by the target workflow;
    /// * an **open** item may not be mapped into a **closed** state — that would
    ///   close it without satisfying the completion guards; use `apm status`
    ///   (which enforces them) after the upgrade instead;
    /// * a **closed** item is never reopened by a mapping; it is reported as
    ///   `skipped_closed` and keeps its old snapshot;
    /// * an open item whose current state is unknown to the target and unmapped
    ///   blocks the whole upgrade instead of producing an un-flowable item.
    pub fn plan_workflow_upgrade(
        &self,
        project: Option<&str>,
        item_type: &str,
        target_id: &str,
        target_version: i64,
        mappings: &std::collections::BTreeMap<String, String>,
        migrate_items: bool,
    ) -> Result<WorkflowUpgradeReport> {
        let project_row = self.require_project(project)?;
        let item_type = item_type_key(item_type)?;
        let target = self.workflow_by_id(Some(&project_row.id), target_id, target_version)?;

        for (from, to) in mappings {
            StateKey::parse(from)?;
            StateKey::parse(to)?;
            if target.state(to).is_none() {
                return Err(Error::invalid(format!(
                    "mapping target '{to}' (for '{from}') is not declared by {} v{}",
                    target.id, target.version
                )));
            }
        }

        let binding_before: Option<WorkflowVersionRef> = self
            .conn
            .query_row(
                "SELECT workflow_id, workflow_version FROM type_workflows
                 WHERE project_id = ?1 AND item_type = ?2",
                params![project_row.id, item_type],
                |r| {
                    Ok(WorkflowVersionRef {
                        workflow_id: r.get(0)?,
                        workflow_version: r.get(1)?,
                    })
                },
            )
            .optional()?;

        let rows: Vec<(String, String, String, String, i64, i64, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, identifier, status, workflow_id, workflow_version, state_category,
                        CAST(version AS TEXT)
                 FROM work_items WHERE project_id = ?1 AND item_type = ?2 ORDER BY number",
            )?;
            stmt.query_map(params![project_row.id, item_type], |r| {
                let version: String = r.get(6)?;
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    version.parse::<i64>().unwrap_or(0),
                    r.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut items = Vec::with_capacity(rows.len());
        for (item_id, identifier, state, from_id, from_version, item_version, category) in rows {
            let source_closed = category == Category::Closed.as_str();
            let already = from_id == target.id && from_version == target.version;
            let state_in_target = target.state(&state).is_some();
            let mapped = if state_in_target {
                Some(state.clone())
            } else {
                mappings.get(&state).cloned()
            };
            let mapped_category = mapped.as_deref().and_then(|m| target.category_of(m));

            let (action, to_state, reason): (&str, Option<String>, Option<String>) =
                if !migrate_items {
                    if already && state_in_target {
                        ("already", Some(state.clone()), None)
                    } else {
                        (
                            "rebind_only",
                            None,
                            Some("type re-bound; item keeps its snapshot".to_string()),
                        )
                    }
                } else if source_closed {
                    // Closed items always keep their historical snapshot: re-binding
                    // must never rewrite what happened, even when the target declares
                    // the same state.
                    if already {
                        ("already", Some(state.clone()), None)
                    } else {
                        (
                            "skipped_closed",
                            None,
                            Some("closed item keeps its historical workflow snapshot".to_string()),
                        )
                    }
                } else if state_in_target {
                    if already {
                        ("already", Some(state.clone()), None)
                    } else {
                        ("migrate", mapped.clone(), None)
                    }
                } else if let Some(to) = mapped.clone() {
                    let to_closed = mapped_category.map(|c| c.is_closed()).unwrap_or(false);
                    if to_closed {
                        (
                            "blocked",
                            None,
                            Some(format!(
                                "mapping '{state}' into closed state '{to}' would close the item \
                             without satisfying the completion guards; migrate to a non-closed \
                             state and use `apm status` instead"
                            )),
                        )
                    } else {
                        ("migrate", Some(to), None)
                    }
                } else {
                    (
                        "blocked",
                        None,
                        Some(format!(
                            "state '{state}' is not declared by {} v{} and has no mapping",
                            target.id, target.version
                        )),
                    )
                };

            items.push(WorkflowUpgradeItem {
                identifier,
                item_id,
                from_workflow_id: from_id,
                from_workflow_version: from_version,
                from_state: state,
                from_version: item_version,
                to_state,
                action: action.to_string(),
                reason,
            });
        }

        let migrated = items.iter().filter(|i| i.action == "migrate").count() as i64;
        let skipped = items
            .iter()
            .filter(|i| {
                matches!(
                    i.action.as_str(),
                    "already" | "skipped_closed" | "rebind_only"
                )
            })
            .count() as i64;
        let blocked = items.iter().filter(|i| i.action == "blocked").count() as i64;

        if blocked > 0 {
            let blocked_items: Vec<String> = items
                .iter()
                .filter(|i| i.action == "blocked")
                .map(|i| {
                    format!(
                        "{} ({}){}",
                        i.identifier,
                        i.from_state,
                        i.reason
                            .as_ref()
                            .map(|r| format!(": {r}"))
                            .unwrap_or_default()
                    )
                })
                .collect();
            return Err(Error::WorkflowViolation(format!(
                "upgrade blocked: {} open item(s) cannot move to {} v{}: {}",
                blocked,
                target.id,
                target.version,
                blocked_items.join("; ")
            )));
        }

        let binding_changed = match &binding_before {
            Some(before) => {
                before.workflow_id != target.id || before.workflow_version != target.version
            }
            None => true,
        };

        Ok(WorkflowUpgradeReport {
            item_type,
            target: WorkflowVersionRef {
                workflow_id: target.id,
                workflow_version: target.version,
            },
            binding_before,
            dry_run: true,
            migrate_items,
            items,
            migrated,
            skipped,
            blocked,
            binding_changed,
        })
    }

    /// Apply a previously computed plan.
    ///
    /// Every item write is a compare-and-swap on `(version, status)`: if another
    /// process transitioned or progressed the item between planning and applying,
    /// the whole transaction rolls back with `upgrade_conflict` instead of
    /// overwriting the newer state. Events carry `prev_version`/`new_version`
    /// like every other state write.
    pub fn apply_workflow_upgrade(
        &mut self,
        project: Option<&str>,
        plan: WorkflowUpgradeReport,
        actor: &Actor,
        at: i64,
    ) -> Result<WorkflowUpgradeReport> {
        let project_row = self.require_project(project)?;
        let target = self.workflow_by_id(
            Some(&project_row.id),
            &plan.target.workflow_id,
            plan.target.workflow_version,
        )?;
        if !plan.item_type.is_empty() {
            let _ = item_type_key(&plan.item_type)?;
        }
        let mut report = plan;
        report.dry_run = false;

        self.with_tx(|tx| {
            let current_binding: Option<(String, i64)> = tx
                .query_row(
                    "SELECT workflow_id, workflow_version FROM type_workflows
                     WHERE project_id = ?1 AND item_type = ?2",
                    params![project_row.id, report.item_type],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let binding_changed = match current_binding {
                Some((id, version)) => id != target.id || version != target.version,
                None => true,
            };
            report.binding_changed = binding_changed;

            if binding_changed {
                tx.execute(
                    "INSERT INTO type_workflows
                     (project_id, item_type, workflow_id, workflow_version, updated_at, updated_by)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(project_id, item_type) DO UPDATE SET
                        workflow_id = excluded.workflow_id,
                        workflow_version = excluded.workflow_version,
                        updated_at = excluded.updated_at,
                        updated_by = excluded.updated_by",
                    params![
                        project_row.id,
                        report.item_type,
                        target.id,
                        target.version,
                        at,
                        actor.name
                    ],
                )?;
                super::events::append_event(
                    tx,
                    super::events::EventInput {
                        at,
                        project_id: &project_row.id,
                        work_item_id: None,
                        kind: "type_workflow_bound",
                        actor,
                        prev_version: None,
                        new_version: None,
                        idempotency_key: None,
                        payload: json!({
                            "item_type": report.item_type,
                            "workflow_id": target.id,
                            "workflow_version": target.version,
                            "reason": "workflow upgrade",
                        }),
                    },
                )?;
            }

            for item in report.items.iter().filter(|i| i.action == "migrate") {
                let to_state = item
                    .to_state
                    .clone()
                    .unwrap_or_else(|| item.from_state.clone());
                let category = target
                    .category_of(&to_state)
                    .unwrap_or_else(|| Category::legacy_for_state(&to_state));
                let changed = tx.execute(
                    "UPDATE work_items SET workflow_id = ?1, workflow_version = ?2,
                        status = ?3, state_category = ?4, version = version + 1, updated_at = ?5
                     WHERE id = ?6 AND version = ?7 AND status = ?8",
                    params![
                        target.id,
                        target.version,
                        to_state,
                        category.as_str(),
                        at,
                        item.item_id,
                        item.from_version,
                        item.from_state
                    ],
                )?;
                if changed != 1 {
                    let current: Option<(i64, String)> = tx
                        .query_row(
                            "SELECT version, status FROM work_items WHERE id = ?1",
                            params![item.item_id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    // The plan was computed outside the transaction, so the item
                    // may have moved on. Report the optimistic-lock failure (and
                    // roll back every item in this upgrade) instead of overwriting
                    // newer state. `current.version` is included for diagnosis.
                    let _ = &current;
                    return Err(Error::VersionConflict {
                        expected: item.from_version,
                        actual: current.map(|(v, _)| v).unwrap_or(-1),
                    });
                }
                super::events::append_event(
                    tx,
                    super::events::EventInput {
                        at,
                        project_id: &project_row.id,
                        work_item_id: Some(&item.item_id),
                        kind: "workflow_migrated",
                        actor,
                        prev_version: Some(item.from_version),
                        new_version: Some(item.from_version + 1),
                        idempotency_key: None,
                        payload: json!({
                            "reason": "operator workflow upgrade",
                            "from_workflow_id": item.from_workflow_id,
                            "from_workflow_version": item.from_workflow_version,
                            "from_state": item.from_state,
                            "to_workflow_id": target.id,
                            "to_workflow_version": target.version,
                            "to_state": to_state,
                            "to_category": category.as_str(),
                        }),
                    },
                )?;
            }
            Ok(())
        })?;
        Ok(report)
    }

    /// Plan + apply (or just plan when `dry_run`). Kept as the one-call API used
    /// by the CLI; tests use plan/apply directly to exercise the CAS window.
    #[allow(clippy::too_many_arguments)]
    pub fn upgrade_type_workflow(
        &mut self,
        project: Option<&str>,
        item_type: &str,
        target_id: &str,
        target_version: i64,
        mappings: &std::collections::BTreeMap<String, String>,
        migrate_items: bool,
        dry_run: bool,
        actor: &Actor,
        at: i64,
    ) -> Result<WorkflowUpgradeReport> {
        let plan = self.plan_workflow_upgrade(
            project,
            item_type,
            target_id,
            target_version,
            mappings,
            migrate_items,
        )?;
        if dry_run {
            return Ok(plan);
        }
        self.apply_workflow_upgrade(project, plan, actor, at)
    }
}

/// Result of one atomic workflow configuration change (`workflow set`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfigOutcome {
    pub workflow: Workflow,
    pub created: bool,
    pub bound: Vec<TypeWorkflowBinding>,
    pub made_default: bool,
    pub dry_run: bool,
}

impl Store {
    /// Validate a workflow document plus every binding/default change, then apply
    /// them in **one transaction**.
    ///
    /// This exists because "register then bind each type then set default" as
    /// separate transactions can half-apply when a later binding is invalid. All
    /// item types are validated before any write, and a failure anywhere rolls
    /// the whole change back. Unchanged registrations/bindings/defaults are
    /// no-ops (no UPDATE, no audit event), so repeating the same `set` is
    /// idempotent.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_workflow_config(
        &mut self,
        project: Option<&str>,
        mut workflow: Workflow,
        bind: &[String],
        make_default: bool,
        dry_run: bool,
        actor: &Actor,
        at: i64,
    ) -> Result<WorkflowConfigOutcome> {
        workflow.normalise()?;
        workflow.validate()?;
        let project_row = self.require_project(project)?;

        // Validate and de-duplicate every requested type *before* touching the DB.
        let mut item_types: Vec<String> = Vec::new();
        for raw in bind {
            let item_type = item_type_key(raw)?;
            if !item_types.contains(&item_type) {
                item_types.push(item_type);
            }
        }

        let canonical = workflow.to_json()?;
        if dry_run {
            let existing: Option<String> = self
                .conn
                .query_row(
                    "SELECT definition_json FROM workflows
                     WHERE project_id = ?1 AND id = ?2 AND version = ?3",
                    params![project_row.id, workflow.id, workflow.version],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(stored) = &existing
                && stored != &canonical
            {
                return Err(Error::invalid(format!(
                    "workflow {} v{} is already registered with different content; bump the version",
                    workflow.id, workflow.version
                )));
            }
            let mut would_bind = Vec::new();
            for item_type in &item_types {
                would_bind.push(TypeWorkflowBinding {
                    item_type: item_type.clone(),
                    workflow_id: workflow.id.clone(),
                    workflow_version: workflow.version,
                });
            }
            let would_default = make_default
                && (project_row.default_workflow_id.as_deref() != Some(workflow.id.as_str())
                    || project_row.default_workflow_version != Some(workflow.version));
            return Ok(WorkflowConfigOutcome {
                workflow,
                created: existing.is_none(),
                bound: would_bind,
                made_default: would_default,
                dry_run: true,
            });
        }

        let mut outcome = WorkflowConfigOutcome {
            workflow: workflow.clone(),
            created: false,
            bound: Vec::new(),
            made_default: false,
            dry_run: false,
        };
        let workflow_id = workflow.id.clone();
        let workflow_version = workflow.version;
        let workflow_name = workflow.name.clone();

        self.with_tx(|tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT definition_json FROM workflows
                     WHERE project_id = ?1 AND id = ?2 AND version = ?3",
                    params![project_row.id, workflow_id, workflow_version],
                    |r| r.get(0),
                )
                .optional()?;
            match existing {
                Some(stored) if stored != canonical => {
                    return Err(Error::invalid(format!(
                        "workflow {workflow_id} v{workflow_version} is already registered with \
                         different content; bump the version"
                    )));
                }
                Some(_) => outcome.created = false,
                None => {
                    tx.execute(
                        "INSERT INTO workflows
                         (project_id, id, version, name, definition_json, created_at, created_by)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            project_row.id,
                            workflow_id,
                            workflow_version,
                            workflow_name,
                            canonical,
                            at,
                            actor.name
                        ],
                    )?;
                    super::events::append_event(
                        tx,
                        super::events::EventInput {
                            at,
                            project_id: &project_row.id,
                            work_item_id: None,
                            kind: "workflow_registered",
                            actor,
                            prev_version: None,
                            new_version: None,
                            idempotency_key: None,
                            payload: json!({
                                "workflow_id": workflow_id,
                                "workflow_version": workflow_version,
                                "states": workflow.states.len(),
                                "initial": workflow.initial,
                            }),
                        },
                    )?;
                    outcome.created = true;
                }
            }

            for item_type in &item_types {
                let previous: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT workflow_id, workflow_version FROM type_workflows
                         WHERE project_id = ?1 AND item_type = ?2",
                        params![project_row.id, item_type],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                if let Some((existing_id, existing_version)) = &previous
                    && *existing_id == workflow_id
                    && *existing_version == workflow_version
                {
                    outcome.bound.push(TypeWorkflowBinding {
                        item_type: item_type.clone(),
                        workflow_id: workflow_id.clone(),
                        workflow_version,
                    });
                    continue;
                }
                tx.execute(
                    "INSERT INTO type_workflows
                     (project_id, item_type, workflow_id, workflow_version, updated_at, updated_by)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(project_id, item_type) DO UPDATE SET
                        workflow_id = excluded.workflow_id,
                        workflow_version = excluded.workflow_version,
                        updated_at = excluded.updated_at,
                        updated_by = excluded.updated_by",
                    params![
                        project_row.id,
                        item_type,
                        workflow_id,
                        workflow_version,
                        at,
                        actor.name
                    ],
                )?;
                super::events::append_event(
                    tx,
                    super::events::EventInput {
                        at,
                        project_id: &project_row.id,
                        work_item_id: None,
                        kind: "type_workflow_bound",
                        actor,
                        prev_version: None,
                        new_version: None,
                        idempotency_key: None,
                        payload: json!({
                            "item_type": item_type,
                            "workflow_id": workflow_id,
                            "workflow_version": workflow_version,
                            "previous": previous.map(|(id, v)| json!({
                                "workflow_id": id,
                                "workflow_version": v,
                            })),
                        }),
                    },
                )?;
                outcome.bound.push(TypeWorkflowBinding {
                    item_type: item_type.clone(),
                    workflow_id: workflow_id.clone(),
                    workflow_version,
                });
            }

            if make_default {
                let current: (Option<String>, Option<i64>) = tx.query_row(
                    "SELECT default_workflow_id, default_workflow_version FROM projects WHERE id = ?1",
                    params![project_row.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if current != (Some(workflow_id.clone()), Some(workflow_version)) {
                    tx.execute(
                        "UPDATE projects SET default_workflow_id = ?1, default_workflow_version = ?2
                         WHERE id = ?3",
                        params![workflow_id, workflow_version, project_row.id],
                    )?;
                    super::events::append_event(
                        tx,
                        super::events::EventInput {
                            at,
                            project_id: &project_row.id,
                            work_item_id: None,
                            kind: "project_default_workflow_changed",
                            actor,
                            prev_version: None,
                            new_version: None,
                            idempotency_key: None,
                            payload: json!({
                                "workflow_id": workflow_id,
                                "workflow_version": workflow_version,
                                "previous": json!({
                                    "workflow_id": current.0,
                                    "workflow_version": current.1,
                                }),
                            }),
                        },
                    )?;
                    outcome.made_default = true;
                }
            }
            Ok(())
        })?;
        Ok(outcome)
    }
}
