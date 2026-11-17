//! Dashboard and doctor.
//!
//! The dashboard is a set of orthogonal *views*, not a single sort order: an
//! item can be both "waiting for a human" and "no substantive progress for two
//! days". Priority is encoded in bucket order, never by hiding information.

use std::collections::BTreeMap;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::*;
use crate::store::events::EVENT_SELECT;
use crate::store::*;

#[derive(Debug, Clone, Default)]
pub struct DashboardOptions {
    pub no_progress_after_seconds: Option<i64>,
    pub done_recent_seconds: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub db_path: String,
    pub project: Option<String>,
    pub project_count: i64,
    pub open_items: i64,
    pub active_claims: i64,
    pub expired_unreleased: Vec<ClaimView>,
    pub at_risk_items: Vec<String>,
    pub open_attention: i64,
    pub pending_diagnostics: Vec<EventView>,
    pub recent_events: Vec<EventView>,
    pub workflow_valid: bool,
    pub warnings: Vec<String>,
}

impl Store {
    pub fn dashboard(
        &self,
        project: Option<&str>,
        at: i64,
        opts: DashboardOptions,
    ) -> Result<Dashboard> {
        let project_row = self.require_project(project)?;
        let mut settings = project_row.settings.clone();
        if let Some(v) = opts.no_progress_after_seconds {
            settings.no_progress_after_seconds = v;
        }
        if let Some(v) = opts.done_recent_seconds {
            settings.done_recent_seconds = v;
        }

        let mut stmt = self.conn.prepare(&format!(
            "{ITEM_SELECT} WHERE i.project_id = ?1 AND (i.state_category != 'closed'
                OR (i.done_at IS NOT NULL AND i.done_at >= ?2))
             ORDER BY i.number"
        ))?;
        let rows = stmt
            .query_map(
                params![project_row.id, at - settings.done_recent_ms()],
                RawItem::from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let items = hydrate_items(&self.conn, rows, at, &settings)?;
        let recovery_ids: std::collections::HashSet<String> =
            self.items_with_pending_diagnostic()?.into_iter().collect();

        let mut buckets = Buckets::default();
        let mut totals: BTreeMap<String, usize> = BTreeMap::new();
        let mut open_count = 0usize;

        for item in items {
            if item.is_open() {
                open_count += 1;
            }
            *totals
                .entry(format!("status:{}", item.status.as_str()))
                .or_default() += 1;
            *totals
                .entry(format!("health:{}", item.health.as_str()))
                .or_default() += 1;

            if item.waiting_on_human || !item.attention.is_empty() {
                buckets.waiting_human.push(item.clone());
            }
            if recovery_ids.contains(&item.id) {
                buckets.recovery_failed.push(item.clone());
            }
            if item.health.is_at_risk() {
                buckets.lost_lease.push(item.clone());
            }
            if item.is_open() {
                let last_progress = item
                    .last_progress_at
                    .as_deref()
                    .map(str::to_string)
                    .unwrap_or_else(|| item.created_at.clone());
                let reference = parse_rfc3339_millis(&last_progress).unwrap_or(at);
                if at - reference >= settings.no_progress_after_ms() {
                    buckets.stalled_no_progress.push(item.clone());
                }
                if !item.blocked_by.is_empty() && item.blocked_by.iter().any(|d| d.is_open()) {
                    buckets.waiting_dependencies.push(item.clone());
                }
                // "In progress" means work is actually being held right now:
                // either the workflow says so, or a live lease is attached.
                if matches!(item.status_category.as_str(), "active" | "review")
                    || matches!(item.health, Health::Active | Health::Stale)
                {
                    buckets.in_progress.push(item.clone());
                }
                // Ready and not actually held: the queue of work anyone may pick up.
                if item.status_category == "open"
                    && !matches!(item.health, Health::Active | Health::Stale)
                {
                    buckets.ready_unclaimed.push(item.clone());
                }
            } else if item.status_category == "closed" && item.cancelled_at.is_none() {
                buckets.recently_done.push(item.clone());
            }
        }

        sort_buckets(&mut buckets);
        totals.insert("open_items".into(), open_count);
        totals.insert("waiting_human".into(), buckets.waiting_human.len());
        totals.insert("at_risk".into(), buckets.lost_lease.len());

        Ok(Dashboard {
            generated_at: ts(at),
            project: project_row.name,
            project_id: project_row.id,
            settings,
            totals,
            buckets,
        })
    }

    pub fn doctor(&self, project: Option<&str>, at: i64) -> Result<DoctorReport> {
        let project_row = self.require_project(project)?;
        let db_path = self.path.display().to_string();

        let project_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))?;
        let open_items: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM work_items WHERE project_id = ?1 AND state_category != 'closed'",
            params![project_row.id],
            |r| r.get(0),
        )?;
        let active_claims: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM claims c JOIN work_items i ON i.id = c.work_item_id
             WHERE i.project_id = ?1 AND c.status = 'active'",
            params![project_row.id],
            |r| r.get(0),
        )?;
        let open_attention: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM attention_requests WHERE project_id = ?1 AND status = 'open'",
            params![project_row.id],
            |r| r.get(0),
        )?;

        let expired = {
            let sql = format!("{CLAIM_SELECT} WHERE status = 'active' AND expires_at <= ?1");
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params![at], RawClaim::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::new();
            for claim in rows {
                let item = require_raw_item(&self.conn, &claim.work_item_id)?;
                out.push(claim.view(&item.identifier, at));
            }
            out
        };

        let mut at_risk = Vec::new();
        for id in self.items_with_pending_diagnostic()? {
            at_risk.push(id);
        }

        let pending_diagnostics = self.pending_diagnostics(&project_row.id)?;
        let recent_events = self.project_events(&project_row.id, 20)?;

        // Validate every stored workflow version (migration imports legacy
        // documents, so a broken row is a doctor problem, not a silent fallback).
        let workflow_valid = self
            .list_workflows(Some(&project_row.id))
            .map(|list| !list.is_empty())
            .unwrap_or(false);

        let mut warnings = Vec::new();
        if !expired.is_empty() {
            warnings.push(format!(
                "{} claim(s) expired but still marked active; run `apm expire`",
                expired.len()
            ));
        }
        if !pending_diagnostics.is_empty() {
            warnings.push(format!(
                "{} side-run diagnostic(s) report an unrecovered claim",
                pending_diagnostics.len()
            ));
        }
        if !workflow_valid {
            warnings.push(
                "project workflow JSON is invalid; status transitions will fail with invalid_input"
                    .into(),
            );
        }

        Ok(DoctorReport {
            db_path,
            project: Some(project_row.name),
            project_count,
            open_items,
            active_claims,
            expired_unreleased: expired,
            at_risk_items: at_risk,
            open_attention,
            pending_diagnostics,
            recent_events,
            workflow_valid,
            warnings,
        })
    }

    fn pending_diagnostics(&self, project_id: &str) -> Result<Vec<EventView>> {
        let sql = format!(
            "{EVENT_SELECT} WHERE e.project_id = ?1 AND e.kind = 'diagnostic'
             ORDER BY e.id DESC LIMIT 20"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![project_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for id in rows {
            if let Some(ev) = self.event_by_id(id)? {
                out.push(ev);
            }
        }
        Ok(out)
    }
}

fn parse_rfc3339_millis(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn sort_buckets(b: &mut Buckets) {
    b.waiting_human.sort_by(|a, c| {
        let blocking = |i: &WorkItemView| i.attention.iter().any(|a| a.blocking);
        blocking(c)
            .cmp(&blocking(a))
            .then_with(|| c.updated_at.cmp(&a.updated_at))
    });
    b.lost_lease.sort_by(|a, c| a.updated_at.cmp(&c.updated_at));
    b.stalled_no_progress
        .sort_by(|a, c| a.last_progress_at.cmp(&c.last_progress_at));
    b.in_progress
        .sort_by(|a, c| c.updated_at.cmp(&a.updated_at));
    b.waiting_dependencies.sort_by_key(|i| i.number);
    b.ready_unclaimed.sort_by_key(|i| i.number);
    b.recently_done.sort_by(|a, c| c.done_at.cmp(&a.done_at));
    b.recovery_failed
        .sort_by(|a, c| a.updated_at.cmp(&c.updated_at));
}
