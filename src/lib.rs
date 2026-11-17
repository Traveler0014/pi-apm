//! AgentPM (`apm`) — a local-first, agent-native work ledger.
//!
//! The library is the product: the CLI is a thin shell over `Store`, and the Pi
//! extension shells out to the CLI. Keep the invariants here, not in the CLI:
//!
//! 1. every work-item mutation requires a live claim token (humans exempt);
//! 2. every mutation is version-checked, so a lost race is a `version_conflict`,
//!    never a silent overwrite;
//! 3. heartbeats, run syncs and progress are three different clocks;
//! 4. side-run writes are idempotent per (session, run, item, operation).

pub mod dashboard;
pub mod db;
pub mod error;
pub mod mcp;
pub mod model;
pub mod rest;
pub mod session_store;
pub mod store;
pub mod workflow;

pub use dashboard::{DashboardOptions, DoctorReport};
pub use error::{Error, Result};
pub use model::*;
pub use store::Store;
pub use store::claims::{ClaimListing, ClaimPolicy, ClaimRequest, SessionClaim};
pub use store::items::{ItemPatch, ListFilter, NewItem, ProgressOutcome, RunOutcome, SyncOutcome};
pub use store::misc::AttentionRequest;
pub use workflow::Workflow;
