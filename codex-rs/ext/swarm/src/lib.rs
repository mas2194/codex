//! Harness-level `/swarm`: named agents split a task, work in parallel, and coordinate on a
//! shared message board, modeled on Google Antigravity's `/swarm`.
//!
//! The thread that types `/swarm <task>` becomes the coordinator. It records a roster of named
//! workers and verifiers, spawns one sub-agent per member, and parks. The harness ([`state`]) then
//! owns the swarm:
//! - an append-only board where agents address each other as `@name`, `@group`, `@workers`,
//!   `@verifiers`, `@coordinator`, or `@all`;
//! - mention-driven delivery: an agent whose turn ended parks at zero token cost and is woken
//!   with exactly the messages that mention it;
//! - a valid roster: unique names, non-overlapping worker files, and one verifier for at most
//!   four workers;
//! - adversarial verification: only an assigned verifier that never edited files can pass a
//!   worker, failures go back to the worker, and repeated failures block for the coordinator;
//! - file ownership tracking for `apply_patch` edits, with violations reported on the board;
//! - completion only after every active worker is verified;
//! - persistence, plus a live Markdown board transcript under the state directory.

mod delivery;
mod extension;
mod prompts;
mod service;
pub mod state;
mod tools;

pub use extension::SwarmExtensionConfig;
pub use extension::install;
pub use service::SwarmService;
