//! Error type shared by the library and the CLI.
//!
//! Every variant carries a stable machine-readable `code`, documented in
//! `docs/cli-reference.md` (section "Error model and exit codes"), plus the process exit code the CLI
//! uses. Agents are expected to branch on `code`, never on the message text.

use serde_json::json;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// A work item cannot exist without takeover-grade context.
    #[error("missing context: {0}")]
    MissingContext(String),

    #[error("{entity} not found: {id}")]
    NotFound { entity: &'static str, id: String },

    /// Optimistic concurrency: the caller lost a race.
    #[error("version conflict: expected version {expected}, current version is {actual}")]
    VersionConflict { expected: i64, actual: i64 },

    /// No claim at all for this work item.
    #[error("claim required: work item {item} has no active claim; run `apm claim {item}` first")]
    ClaimRequired { item: String },

    /// A claim exists but the presented token/lease is not usable.
    #[error("claim invalid: {reason}")]
    ClaimInvalid { reason: String },

    /// Another holder owns a live claim.
    #[error("claim conflict: work item {item} is claimed by {holder} (expires {expires_at})")]
    ClaimConflict {
        item: String,
        holder: String,
        expires_at: String,
    },

    #[error("workflow violation: {0}")]
    WorkflowViolation(String),

    #[error("completion evidence required: {0}")]
    CompletionEvidenceRequired(String),

    #[error("dependency error: {0}")]
    Dependency(String),

    /// The same idempotency key was reused for a different operation, item, or
    /// request payload. Replaying would silently return the wrong result.
    #[error("idempotency conflict: key {key} was already used for a different request")]
    IdempotencyConflict { key: String },

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidInput(_) => "invalid_input",
            Error::MissingContext(_) => "missing_context",
            Error::NotFound { .. } => "not_found",
            Error::VersionConflict { .. } => "version_conflict",
            Error::ClaimRequired { .. } => "claim_required",
            Error::ClaimInvalid { .. } => "claim_invalid",
            Error::ClaimConflict { .. } => "claim_conflict",
            Error::WorkflowViolation(_) => "workflow_violation",
            Error::CompletionEvidenceRequired(_) => "completion_evidence_required",
            Error::Dependency(_) => "dependency_invalid",
            Error::IdempotencyConflict { .. } => "idempotency_conflict",
            Error::Forbidden(_) => "forbidden",
            Error::Db(_) => "database_error",
            Error::Serde(_) => "serialization_error",
            Error::Internal(_) => "internal_error",
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Error::InvalidInput(_) | Error::MissingContext(_) => 2,
            Error::NotFound { .. } => 3,
            Error::VersionConflict { .. } => 4,
            Error::ClaimRequired { .. }
            | Error::ClaimInvalid { .. }
            | Error::ClaimConflict { .. } => 5,
            Error::WorkflowViolation(_)
            | Error::CompletionEvidenceRequired(_)
            | Error::Dependency(_) => 6,
            Error::IdempotencyConflict { .. } => 2,
            Error::Forbidden(_) => 7,
            Error::Db(_) | Error::Serde(_) | Error::Internal(_) => 1,
        }
    }

    /// Structured, agent-consumable error payload (`--json` mode).
    pub fn to_json(&self) -> serde_json::Value {
        let details = match self {
            Error::VersionConflict { expected, actual } => {
                json!({ "expected": expected, "actual": actual })
            }
            Error::ClaimConflict {
                item,
                holder,
                expires_at,
            } => json!({ "item": item, "holder": holder, "expires_at": expires_at }),
            Error::ClaimRequired { item } => json!({ "item": item }),
            Error::IdempotencyConflict { key } => json!({ "key": key }),
            Error::NotFound { entity, id } => json!({ "entity": entity, "id": id }),
            _ => json!({}),
        };
        json!({
            "ok": false,
            "error": {
                "code": self.code(),
                "message": self.to_string(),
                "exit_code": self.exit_code(),
                "details": details,
            }
        })
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidInput(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_and_exit_codes_are_stable() {
        assert_eq!(Error::invalid("x").code(), "invalid_input");
        assert_eq!(Error::invalid("x").exit_code(), 2);
        assert_eq!(
            Error::NotFound {
                entity: "item",
                id: "x".into()
            }
            .exit_code(),
            3
        );
        assert_eq!(
            Error::VersionConflict {
                expected: 1,
                actual: 2
            }
            .to_json()["error"]["details"]["actual"],
            json!(2)
        );
    }
}
