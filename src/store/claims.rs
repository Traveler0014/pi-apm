//! Claims: the mutual-exclusion primitive for agent writes.
//!
//! A claim is a lease with a bearer token. The database stores only
//! `sha256(token)`; the plaintext is returned exactly once when the claim is
//! issued and can be persisted to a 0600 session file (see `session_store`) so a
//! side-run can reuse the same claim without weakening the rule that every write
//! needs a valid token.
//!
//! Guarantees:
//!
//! * at most one `active` claim per item (partial unique index + `BEGIN IMMEDIATE`);
//! * a write with an expired lease is rejected, even if the token is correct;
//! * takeover of a *live* claim by an agent requires the same session that
//!   already held it (recovery) — otherwise only a human may force it;
//! * `RecoverOnly` policy (used by side-runs) can only re-acquire an item whose
//!   latest claim by this session is still `active` or `expired`; a released,
//!   superseded or never-claimed item may not be recovered by a side-run;
//! * idempotent replays never contain a plaintext token, because the token is
//!   not part of the stored response.

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::*;

#[derive(Debug, Clone)]
pub struct ClaimRequest {
    pub lease_seconds: Option<i64>,
    pub takeover: bool,
    pub reason: Option<String>,
    pub policy: ClaimPolicy,
}

impl Default for ClaimRequest {
    fn default() -> Self {
        ClaimRequest {
            lease_seconds: None,
            takeover: false,
            reason: None,
            policy: ClaimPolicy::Normal,
        }
    }
}

/// `Normal` may claim any unclaimed item. `RecoverOnly` is used by run-end
/// side-runs: it can only recover a lease this session still owns or lost to
/// expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimPolicy {
    Normal,
    RecoverOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaim {
    pub item: WorkItemView,
    pub claim: ClaimView,
}

/// The idempotency payload for claim/heartbeat: deliberately *without* the
/// plaintext token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ClaimReplay {
    pub claim: ClaimView,
    pub takeover: bool,
}

impl Store {
    pub fn claim(
        &mut self,
        selector: &str,
        actor: &Actor,
        req: ClaimRequest,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<ClaimIssued> {
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        let settings = project_row.settings.clone();
        let lease = match req.lease_seconds {
            Some(l) => l,
            None => settings.lease_seconds,
        };
        if !(30..=86_400).contains(&lease) {
            return Err(Error::invalid("lease must be between 30 and 86400 seconds"));
        }
        let item_id = require_raw_item(&self.conn, selector)?.id;
        let idem = Idem::new("claim.acquire", idempotency_key)
            .item(Some(&item_id))
            .request(json!({
                "lease_seconds": lease,
                "takeover": req.takeover,
                "reason": req.reason,
                "policy": match req.policy {
                    ClaimPolicy::Normal => "normal",
                    ClaimPolicy::RecoverOnly => "recover-only",
                },
                "actor": actor.name,
                "actor_kind": actor.kind.as_str(),
                "session": actor.session_id,
                "run": actor.run_id,
            }));

        let mut issued_token: Option<String> = None;
        let replay = self.idempotent(idem, |tx| {
            let raw = require_raw_item(tx, selector)?;
            if !raw.status.is_open() {
                return Err(Error::ClaimInvalid {
                    reason: format!(
                        "work item {} is {} and cannot be claimed (reopen it first)",
                        raw.identifier,
                        raw.status.as_str()
                    ),
                });
            }

            // Snapshot what this session last held *before* any state change below,
            // so `recover-only` can tell "lost my lease" from "I gave it up".
            let prior = previous_claim_by_session(tx, &raw.id, actor.session_id.as_deref())?;
            let prior_recoverable = matches!(
                prior.as_ref().map(|c| c.status.as_str()),
                Some("active") | Some("expired")
            );

            // Expire stale claims lazily so the partial unique index frees up.
            let mut lease_expired = false;
            if let Some(active) = active_claim(tx, &raw.id)?
                && active.expires_at <= at
            {
                mark_claim(tx, &active, "expired", at, None, Some("lease expired"))?;
                lease_expired = true;
                append_claim_event(
                    tx,
                    &project_row.id,
                    &raw,
                    "claim_expired",
                    actor,
                    at,
                    json!({
                        "claim_id": active.id,
                        "holder": active.holder,
                        "session_id": active.session_id,
                        "expires_at": ts(active.expires_at),
                    }),
                    None,
                )?;
            }

            let existing = active_claim(tx, &raw.id)?;
            let mut superseded = false;

            if let Some(existing) = &existing {
                let same_session = match (&existing.session_id, &actor.session_id) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if same_session {
                    // Same session lost its token (crash/reload): supersede its
                    // own claim rather than deadlock. Another agent's live claim
                    // is never stolen this way.
                    superseded = true;
                    mark_claim(
                        tx,
                        existing,
                        "superseded",
                        at,
                        None,
                        Some("superseded by same session re-claim"),
                    )?;
                } else if actor.is_human() && req.takeover {
                    superseded = true;
                    mark_claim(
                        tx,
                        existing,
                        "superseded",
                        at,
                        None,
                        Some(req.reason.as_deref().unwrap_or("human force takeover")),
                    )?;
                    append_claim_event(
                        tx,
                        &project_row.id,
                        &raw,
                        "claim_superseded",
                        actor,
                        at,
                        json!({
                            "claim_id": existing.id,
                            "holder": existing.holder,
                            "reason": req.reason,
                            "forced": true,
                        }),
                        None,
                    )?;
                } else {
                    return Err(Error::ClaimConflict {
                        item: raw.identifier.clone(),
                        holder: existing.holder.clone(),
                        expires_at: ts(existing.expires_at),
                    });
                }
            }

            let takeover = lease_expired || superseded;

            // A side-run may only recover a lease this session still owns or lost
            // to expiry; never a new task, and never something it released.
            if req.policy == ClaimPolicy::RecoverOnly && !prior_recoverable {
                return Err(Error::Forbidden(format!(
                    "side-run may only recover a lease this session still owns or lost to expiry; \
                     {} was never claimed by session {} or was released/superseded",
                    raw.identifier,
                    actor.session_id.as_deref().unwrap_or("(none)")
                )));
            }

            let (token, token_hash, token_prefix) = new_token();
            let claim_id = uuid::Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO claims
                 (id, work_item_id, token_hash, token_prefix, holder, holder_kind, session_id,
                  run_id, status, lease_seconds, leased_at, heartbeat_at, expires_at,
                  released_at, release_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?10, ?10, ?11, NULL, NULL)",
                params![
                    claim_id,
                    raw.id,
                    token_hash,
                    token_prefix,
                    actor.name,
                    actor.kind.as_str(),
                    actor.session_id,
                    actor.run_id,
                    lease,
                    at,
                    at + lease * 1000
                ],
            )?;
            tx.execute(
                "UPDATE work_items SET last_heartbeat_at = ?1 WHERE id = ?2",
                params![at, raw.id],
            )?;

            let kind = if takeover {
                "claim_taken_over"
            } else {
                "claimed"
            };
            append_claim_event(
                tx,
                &project_row.id,
                &raw,
                kind,
                actor,
                at,
                json!({
                    "claim_id": claim_id,
                    "lease_seconds": lease,
                    "expires_at": ts(at + lease * 1000),
                    "recovered": req.policy == ClaimPolicy::RecoverOnly,
                    "reason": req.reason,
                }),
                idempotency_key,
            )?;

            let claim = active_claim(tx, &raw.id)?
                .ok_or_else(|| Error::Internal("claim vanished after insert".into()))?;
            issued_token = Some(token);
            Ok(ClaimReplay {
                claim: claim.view(&raw.identifier, at),
                takeover,
            })
        })?;

        let replayed = issued_token.is_none();
        Ok(ClaimIssued {
            claim: replay.claim,
            token: issued_token.unwrap_or_default(),
            takeover: replay.takeover,
            replayed,
        })
    }

    /// Extend the lease. The token never changes and is never stored.
    pub fn heartbeat(
        &mut self,
        selector: &str,
        token: &str,
        extend_seconds: Option<i64>,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<ClaimIssued> {
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        let item_id = require_raw_item(&self.conn, selector)?.id;
        // The token itself is a secret, so it is not part of the fingerprint.
        let idem = Idem::new("claim.renew", idempotency_key)
            .item(Some(&item_id))
            .request(json!({
                "extend_seconds": extend_seconds,
                "actor": actor.name,
                "session": actor.session_id,
            }));
        let mut executed = false;
        let replay = self.idempotent(idem, |tx| {
            let raw = require_raw_item(tx, selector)?;
            let claim = verify_claim(tx, &raw.id, token, at)?;
            executed = true;
            let lease = extend_seconds.unwrap_or(claim.lease_seconds);
            if !(30..=86_400).contains(&lease) {
                return Err(Error::invalid("lease must be between 30 and 86400 seconds"));
            }
            let expires_at = at + lease * 1000;
            tx.execute(
                "UPDATE claims SET heartbeat_at = ?1, expires_at = ?2, lease_seconds = ?3
                 WHERE id = ?4",
                params![at, expires_at, lease, claim.id],
            )?;
            tx.execute(
                "UPDATE work_items SET last_heartbeat_at = ?1 WHERE id = ?2",
                params![at, raw.id],
            )?;
            append_claim_event(
                tx,
                &project_row.id,
                &raw,
                "claim_renewed",
                actor,
                at,
                json!({
                    "claim_id": claim.id,
                    "lease_seconds": lease,
                    "expires_at": ts(expires_at),
                }),
                idempotency_key,
            )?;
            let claim = active_claim(tx, &raw.id)?
                .ok_or_else(|| Error::Internal("claim vanished after renew".into()))?;
            Ok(ClaimReplay {
                claim: claim.view(&raw.identifier, at),
                takeover: false,
            })
        })?;
        Ok(ClaimIssued {
            claim: replay.claim,
            token: token.to_string(),
            takeover: false,
            replayed: !executed,
        })
    }

    pub fn release(
        &mut self,
        selector: &str,
        token: &str,
        reason: Option<&str>,
        actor: &Actor,
        at: i64,
        idempotency_key: Option<&str>,
    ) -> Result<ClaimView> {
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        let item_id = require_raw_item(&self.conn, selector)?.id;
        let idem = Idem::new("claim.release", idempotency_key)
            .item(Some(&item_id))
            .request(json!({ "reason": reason, "actor": actor.name }));
        self.idempotent(idem, |tx| {
            let raw = require_raw_item(tx, selector)?;
            let claim = verify_claim(tx, &raw.id, token, at)?;
            mark_claim(tx, &claim, "released", at, None, reason)?;
            append_claim_event(
                tx,
                &project_row.id,
                &raw,
                "claim_released",
                actor,
                at,
                json!({ "claim_id": claim.id, "reason": reason }),
                idempotency_key,
            )?;
            let claim = latest_claim(tx, &raw.id)?
                .ok_or_else(|| Error::Internal("claim vanished after release".into()))?;
            Ok(claim.view(&raw.identifier, at))
        })
    }

    /// Human-only: drop another holder's claim without a token.
    pub fn force_release(
        &mut self,
        selector: &str,
        reason: &str,
        actor: &Actor,
        at: i64,
    ) -> Result<ClaimView> {
        if !actor.is_human() {
            return Err(Error::Forbidden(
                "only a human actor may force-release a claim".into(),
            ));
        }
        if reason.trim().is_empty() {
            return Err(Error::invalid("force release requires a reason"));
        }
        let project_row = {
            let raw = require_raw_item(&self.conn, selector)?;
            self.require_project(Some(&raw.project_id))?
        };
        self.with_tx(|tx| {
            let raw = require_raw_item(tx, selector)?;
            let claim = active_claim(tx, &raw.id)?.ok_or_else(|| Error::ClaimInvalid {
                reason: format!("{} has no active claim", raw.identifier),
            })?;
            mark_claim(tx, &claim, "released", at, None, Some(reason))?;
            append_claim_event(
                tx,
                &project_row.id,
                &raw,
                "claim_released",
                actor,
                at,
                json!({ "claim_id": claim.id, "reason": reason, "forced": true }),
                None,
            )?;
            let claim = latest_claim(tx, &raw.id)?
                .ok_or_else(|| Error::Internal("claim vanished after release".into()))?;
            Ok(claim.view(&raw.identifier, at))
        })
    }

    /// Active claims for a session, optionally including expired-but-unreleased
    /// ones so a side-run can see what it may recover.
    pub fn session_claims(
        &self,
        session: &str,
        include_expired: bool,
        at: i64,
    ) -> Result<Vec<SessionClaim>> {
        let statuses = if include_expired {
            "('active', 'expired')"
        } else {
            "('active')"
        };
        let sql = format!(
            "SELECT c.id FROM claims c
             WHERE c.session_id = ?1 AND c.status IN {statuses}
               AND c.id = (SELECT c2.id FROM claims c2 WHERE c2.work_item_id = c.work_item_id
                           ORDER BY c2.leased_at DESC, c2.rowid DESC LIMIT 1)
             ORDER BY c.leased_at DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt
            .query_map(params![session], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = Vec::new();
        for id in ids {
            let claim = self
                .conn
                .query_row(
                    &format!("{CLAIM_SELECT} WHERE id = ?1"),
                    params![id],
                    RawClaim::from_row,
                )
                .optional()?;
            let Some(claim) = claim else { continue };
            let item = require_raw_item(&self.conn, &claim.work_item_id)?;
            let settings = self.settings_for(&item.project_id)?;
            let view = hydrate_item(&self.conn, &item, at, &settings)?;
            out.push(SessionClaim {
                item: view,
                claim: claim.view(&item.identifier, at),
            });
        }
        Ok(out)
    }

    /// Sweep every expired lease in one transaction.
    ///
    /// The conditional `UPDATE ... WHERE status='active' AND expires_at<=?`
    /// means a lease renewed between scan and sweep is never clobbered: the
    /// statement simply matches no row.
    pub fn expire_claims(&mut self, at: i64) -> Result<Vec<ClaimView>> {
        self.with_tx(|tx| {
            let ids: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM claims WHERE status = 'active' AND expires_at <= ?1",
                )?;
                stmt.query_map(params![at], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut out = Vec::new();
            for id in ids {
                if let Some(view) = expire_one_in_tx(tx, &id, at)? {
                    out.push(view);
                }
            }
            Ok(out)
        })
    }

    /// Expire one claim by id, guarded. Exposed so tests can prove the
    /// scan-then-renew race cannot be mis-handled: a renewed claim returns `None`
    /// and stays `active`.
    pub fn expire_claim(&mut self, claim_id: &str, at: i64) -> Result<Option<ClaimView>> {
        self.with_tx(|tx| expire_one_in_tx(tx, claim_id, at))
    }
}

/// Conditional expiry inside a transaction. Returns `None` when the claim was
/// renewed (or already expired/released) after the scan.
fn expire_one_in_tx(tx: &Transaction<'_>, claim_id: &str, at: i64) -> Result<Option<ClaimView>> {
    let changed = tx.execute(
        "UPDATE claims SET status = 'expired', release_reason = COALESCE(release_reason, 'lease expired')
         WHERE id = ?1 AND status = 'active' AND expires_at <= ?2",
        params![claim_id, at],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    let claim = tx
        .query_row(
            &format!("{CLAIM_SELECT} WHERE id = ?1"),
            params![claim_id],
            RawClaim::from_row,
        )
        .optional()?
        .ok_or_else(|| Error::Internal("expired claim vanished".into()))?;
    let item = require_raw_item(tx, &claim.work_item_id)?;
    let actor = Actor::agent("apm").with_session(claim.session_id.clone());
    append_claim_event(
        tx,
        &item.project_id,
        &item,
        "claim_expired",
        &actor,
        at,
        json!({
            "claim_id": claim.id,
            "holder": claim.holder,
            "session_id": claim.session_id,
            "expires_at": ts(claim.expires_at),
        }),
        None,
    )?;
    Ok(Some(claim.view(&item.identifier, at)))
}

/// Verify a bearer token against the item's live claim.
pub(crate) fn verify_claim(
    conn: &rusqlite::Connection,
    item_id: &str,
    token: &str,
    at: i64,
) -> Result<RawClaim> {
    let claim = active_claim(conn, item_id)?.ok_or_else(|| Error::ClaimRequired {
        item: item_id.to_string(),
    })?;
    if claim.expires_at <= at {
        return Err(Error::ClaimInvalid {
            reason: format!(
                "lease expired at {}; re-claim with `apm claim <item> --takeover --reason ...`",
                ts(claim.expires_at)
            ),
        });
    }
    if hash_token(token) != claim.token_hash {
        return Err(Error::ClaimInvalid {
            reason: "token does not match the current active claim".into(),
        });
    }
    Ok(claim)
}

/// Used by every work-item write. Humans may write without a claim (the human
/// management path); agents may not.
pub(crate) fn require_write_claim(
    conn: &rusqlite::Connection,
    item: &RawItem,
    token: Option<&str>,
    actor: &Actor,
    at: i64,
) -> Result<Option<RawClaim>> {
    if actor.is_human() && token.is_none() {
        return Ok(None);
    }
    let token = token.ok_or_else(|| Error::ClaimRequired {
        item: item.identifier.clone(),
    })?;
    Ok(Some(verify_claim(conn, &item.id, token, at)?))
}

fn previous_claim_by_session(
    conn: &rusqlite::Connection,
    item_id: &str,
    session: Option<&str>,
) -> Result<Option<RawClaim>> {
    let Some(session) = session else {
        return Ok(None);
    };
    let sql = format!(
        "{CLAIM_SELECT} WHERE work_item_id = ?1 AND session_id = ?2
         ORDER BY leased_at DESC, rowid DESC LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![item_id, session], RawClaim::from_row)
        .optional()?)
}

fn mark_claim(
    tx: &Transaction<'_>,
    claim: &RawClaim,
    status: &str,
    at: i64,
    released_at: Option<i64>,
    reason: Option<&str>,
) -> Result<()> {
    let released = released_at.or_else(|| if status == "released" { Some(at) } else { None });
    tx.execute(
        "UPDATE claims SET status = ?1, released_at = COALESCE(?2, released_at),
            release_reason = COALESCE(?3, release_reason)
         WHERE id = ?4",
        params![status, released, reason, claim.id],
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_claim_event(
    tx: &Transaction<'_>,
    project_id: &str,
    item: &RawItem,
    kind: &str,
    actor: &Actor,
    at: i64,
    payload: serde_json::Value,
    idempotency_key: Option<&str>,
) -> Result<i64> {
    super::events::append_event(
        tx,
        super::events::EventInput {
            at,
            project_id,
            work_item_id: Some(&item.id),
            kind,
            actor,
            prev_version: None,
            new_version: None,
            idempotency_key,
            payload,
        },
    )
}

pub(crate) fn new_token() -> (String, String, String) {
    let token = format!(
        "apm_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let prefix = token.chars().skip(4).take(8).collect::<String>();
    let hash = hash_token(&token);
    (token, hash, prefix)
}

pub(crate) fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Returned by `apm claims --json` for the side-run and `/apm-doctor`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimListing {
    pub session: Option<String>,
    pub claims: Vec<SessionClaim>,
    /// item_id -> claim token. Only populated for `--with-tokens`, and only for
    /// claims this session acquired through the CLI (tokens are never stored in
    /// the database in plaintext). Local-only: same trust boundary as the DB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<std::collections::BTreeMap<String, String>>,
    /// True when `--with-tokens` was requested but suppressed because the caller
    /// is a run-end side-run (`APM_SIDE_RUN=1`): a side-run has no legitimate
    /// reason to read plaintext tokens, and its output is model-facing.
    #[serde(default)]
    pub tokens_suppressed: bool,
}
