use super::*;
use crate::state::AcceptanceCriterion;
use crate::state::Brief;
use crate::state::MilestonePlan;
use crate::state::Track;

fn run_in_gating() -> (TeamworkRun, Vec<GateTicket>) {
    let mut run = TeamworkRun::new("run12345678", "lead", "make <it> fast & safe");
    run.record_brief(Brief {
        objective: "Speed up parsing".to_string(),
        requirements: Vec::new(),
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "ac1".to_string(),
            description: "benchmarks improve".to_string(),
            verification: "cargo bench".to_string(),
        }],
        out_of_scope: Vec::new(),
        assumptions: Vec::new(),
    })
    .expect("brief");
    run.record_plan(vec![MilestonePlan {
        id: "m1".to_string(),
        title: "Fast path".to_string(),
        deliverable: "fast path for ASCII".to_string(),
        tracks: vec![Track {
            id: "t1".to_string(),
            description: "ascii fast path".to_string(),
            files: vec!["src/lexer.rs".to_string()],
            role: "worker".to_string(),
            model: Some("fast-model".to_string()),
            reasoning_effort: Some("low".to_string()),
        }],
    }])
    .expect("plan");
    let tickets = run
        .submit_for_review("m1", "added fast path".to_string())
        .expect("submit");
    (run, tickets)
}

#[test]
fn lead_prompt_embeds_escaped_goal() {
    let prompt = lead_prompt("make <it> fast & safe");
    assert!(prompt.contains("<user_goal>\nmake &lt;it&gt; fast &amp; safe\n</user_goal>"));
    assert!(!prompt.contains("{{"));
}

#[test]
fn continuation_prompt_has_no_placeholders() {
    let (run, _) = run_in_gating();
    let prompt = continuation_prompt(&run);
    assert!(!prompt.contains("{{"));
    assert!(prompt.contains("Phase: gating"));
}

#[test]
fn gate_message_names_token_and_verdict_tool() {
    let (run, tickets) = run_in_gating();
    let message = gate_spawn_message(&run, &tickets[0]);
    assert!(message.contains(&tickets[0].token));
    assert!(message.contains(SUBMIT_VERDICT_TOOL_NAME));
    assert!(message.contains("<role>"));
    assert!(message.contains("Critic gate for milestone `m1`"));
    assert!(message.contains("ac1: benchmarks improve"));
}

#[test]
fn status_lists_track_model_choices() {
    let (run, _) = run_in_gating();
    let status = status_markdown(&run);
    assert!(status.contains("model: fast-model; effort: low"));
    assert!(status.contains("Critic round 1 [open]"));
}
