//! Project lifecycle: initialisation and settings.

use rusqlite::{OptionalExtension, params};
use serde_json::json;

use super::*;
use crate::workflow;

impl Store {
    /// Create the project if it does not exist yet. Idempotent by name so that
    /// a repeated `apm init` in CI or a fresh clone is harmless.
    pub fn init_project(
        &mut self,
        name: &str,
        prefix: Option<&str>,
        root_path: Option<&str>,
        settings: Settings,
        actor: &Actor,
    ) -> Result<(ProjectView, bool)> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::invalid("project name must be non-empty"));
        }
        settings.validate()?;
        let prefix = match prefix {
            Some(p) => normalise_prefix(p)?,
            None => derive_prefix(name)?,
        };
        if let Some(existing) = self.project_by_name(name)? {
            return Ok((existing.view(&self.path), false));
        }
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        let id_for_tx = id.clone();
        let name_owned = name.to_string();
        let prefix_for_tx = prefix.clone();
        let root_owned = root_path.map(str::to_string);
        let settings_json = serde_json::to_string(&settings)?;
        self.with_tx(|tx| {
            tx.execute(
                "INSERT INTO projects (id, name, prefix, root_path, created_at, settings_json, workflow_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![
                    id_for_tx,
                    name_owned,
                    prefix_for_tx,
                    root_owned,
                    now,
                    settings_json
                ],
            )?;
            super::workflows::seed_default_workflow(tx, &id_for_tx, now, actor)?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at: now,
                    project_id: &id_for_tx,
                    work_item_id: None,
                    kind: "project_initialised",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key: None,
                    payload: json!({
                        "name": name_owned,
                        "prefix": prefix_for_tx,
                        "settings": settings,
                    }),
                },
            )?;
            Ok(())
        })?;
        let project = self
            .project_by_name(&id)?
            .ok_or_else(|| Error::Internal("project vanished after insert".into()))?;
        Ok((project.view(&self.path), true))
    }

    pub fn project_view(&self, explicit: Option<&str>) -> Result<ProjectView> {
        Ok(self.require_project(explicit)?.view(&self.path))
    }

    pub fn update_settings(
        &mut self,
        project: Option<&str>,
        settings: Settings,
        actor: &Actor,
    ) -> Result<ProjectView> {
        settings.validate()?;
        let row = self.require_project(project)?;
        let now = now_ms();
        let settings_json = serde_json::to_string(&settings)?;
        let id = row.id.clone();
        let old = row.settings.clone();
        self.with_tx(|tx| {
            tx.execute(
                "UPDATE projects SET settings_json = ?1 WHERE id = ?2",
                params![settings_json, id],
            )?;
            super::events::append_event(
                tx,
                super::events::EventInput {
                    at: now,
                    project_id: &id,
                    work_item_id: None,
                    kind: "settings_updated",
                    actor,
                    prev_version: None,
                    new_version: None,
                    idempotency_key: None,
                    payload: json!({ "previous": old, "next": settings }),
                },
            )?;
            Ok(())
        })?;
        self.project_view(Some(&id))
    }

    /// Machine-readable declarative workflow currently in force.
    /// The project's default workflow (used by `apm workflow`).
    ///
    /// Per-type bindings live in `type_workflows`; this is the fallback for item
    /// types without an explicit binding.
    pub fn workflow(&self, project: Option<&str>) -> Result<workflow::Workflow> {
        let row = self.require_project(project)?;
        let id = row
            .default_workflow_id
            .unwrap_or_else(|| workflow::DEFAULT_WORKFLOW_ID.to_string());
        let version = row
            .default_workflow_version
            .unwrap_or(workflow::DEFAULT_WORKFLOW_VERSION);
        self.load_workflow(&row.id, &id, version)
    }
}

fn normalise_prefix(raw: &str) -> Result<String> {
    let prefix: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    if prefix.len() < 2 || prefix.len() > 8 {
        return Err(Error::invalid(
            "project prefix must be 2-8 alphanumeric characters",
        ));
    }
    Ok(prefix)
}

fn derive_prefix(name: &str) -> Result<String> {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let mut prefix = String::new();
    if words.len() >= 2 {
        for w in words.iter().take(4) {
            if let Some(c) = w.chars().next() {
                prefix.push(c);
            }
        }
    } else if let Some(word) = words.first() {
        prefix.push_str(&word.chars().take(4).collect::<String>());
    }
    if prefix.len() < 2 {
        return Err(Error::invalid(format!(
            "cannot derive a project prefix from '{name}'; pass --prefix"
        )));
    }
    Ok(prefix.to_ascii_uppercase())
}

impl Store {
    /// Internal helper used by the CLI to find a project by name without
    /// failing when the database has none yet.
    pub fn find_project(&self, name: &str) -> Result<Option<ProjectView>> {
        Ok(self.project_by_name(name)?.map(|p| p.view(&self.path)))
    }

    /// Whether the store already has any project rows.
    pub fn has_projects(&self) -> Result<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        Ok(count > 0)
    }
}
