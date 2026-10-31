//! AgentDB Runtime — Phase 1 (System State Mirror) + Phase 2 (Permission
//! & Capability Engine) + Phase 3 (Transactional Action Engine) + Phase 4
//! (Event-Sourced Audit Trail) + Phase 5 (MCP-Native Tool Registry) +
//! Phase 6 (The Demo Loop).
//!
//! Exposed as a library (in addition to the `agentdb` binary in
//! `main.rs`) so that integration tests under `tests/` can exercise the
//! real router, ingest functions, permission engine, action engine,
//! event store, tool registry, diagnosis engine, and validators directly
//! — against a real Postgres instance — rather than only testing through
//! the HTTP binary or relying solely on the pure-function unit tests
//! inside each module.

pub mod actions;
pub mod actions_api;
pub mod api;
pub mod auth;
pub mod db;
pub mod diagnosis;
pub mod diagnosis_api;
pub mod events;
pub mod events_api;
pub mod extract;
pub mod ingest;
pub mod models;
pub mod permissions;
pub mod permissions_api;
pub mod query_guard;
pub mod query_util;
pub mod rate_limit;
pub mod tool_registry;
pub mod tool_registry_api;
