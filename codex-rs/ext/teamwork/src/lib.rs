//! Harness-level `/teamwork`: a lead agent coordinates sub-agents through milestones, and the
//! harness enforces the workflow.
//!
//! The extension owns the run as a state machine ([`state`]). The model advances it only through
//! tools, and the harness enforces:
//! - a scoping brief with verifiable acceptance criteria before planning;
//! - parallel tracks with non-overlapping file ownership;
//! - independent Critic, Challenger, and Auditor verdicts for every milestone, recorded by
//!   descendant agents that did not edit files (never by the lead);
//! - bounded fix rounds and a final Success Auditor before completion;
//! - automatic continuation of the lead while the run is active, with a no-progress breaker;
//! - persistence, so an unfinished run survives a restart.

mod extension;
mod prompts;
mod service;
pub mod state;
mod tools;

pub use extension::TeamworkExtensionConfig;
pub use extension::install;
pub use service::TeamworkService;
