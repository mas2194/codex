//! Model-facing text owned by the harness: lead instructions, continuation steering, status
//! summaries, and the briefs handed to independent gate agents.

use std::fmt::Write as _;

use crate::state::FINAL_MILESTONE_ID;
use crate::state::GateKind;
use crate::state::GateTicket;
use crate::state::MAX_FIX_ROUNDS;
use crate::state::MAX_IDLE_CONTINUATIONS_WITHOUT_PROGRESS;
use crate::state::MilestoneStatus;
use crate::state::Phase;
use crate::state::TeamworkRun;
use crate::state::TicketStatus;
use crate::tools::SUBMIT_VERDICT_TOOL_NAME;

const LEAD_TEMPLATE: &str = include_str!("../templates/lead.md");
const CONTINUATION_TEMPLATE: &str = include_str!("../templates/continuation.md");

pub(crate) fn lead_prompt(goal: &str) -> String {
    LEAD_TEMPLATE.replace("{{goal}}", &escape_xml(goal.trim()))
}

pub(crate) fn usage_prompt() -> String {
    "The user typed `/teamwork` without a goal. Ask them for the goal they want the team to deliver, and give one example such as `/teamwork migrate the REST service from Express to Fastify`. Do not start any work yet.".to_string()
}

pub(crate) fn continuation_prompt(run: &TeamworkRun) -> String {
    CONTINUATION_TEMPLATE
        .replace("{{status}}", &status_markdown(run))
        .replace("{{next_action}}", &next_action(run))
        .replace(
            "{{max_idle}}",
            &MAX_IDLE_CONTINUATIONS_WITHOUT_PROGRESS.to_string(),
        )
}

/// Reminder attached to ordinary user messages while a run is active, so the lead keeps the
/// authoritative state in view after compaction or a long pause.
pub(crate) fn user_turn_reminder(run: &TeamworkRun, resumed: bool) -> String {
    let mut text = String::new();
    if resumed {
        text.push_str("The user replied, so the harness has resumed the blocked `/teamwork` run. Apply their answer, then continue.\n\n");
    } else {
        text.push_str("A `/teamwork` run is active in this thread. Treat the user's message as input to the run unless they ask to stop it.\n\n");
    }
    let _ = write!(
        text,
        "<teamwork_state>\n{}\n</teamwork_state>\n\n<next_action>\n{}\n</next_action>",
        status_markdown(run),
        next_action(run)
    );
    text
}

pub(crate) fn next_action(run: &TeamworkRun) -> String {
    match run.phase {
        Phase::Scoping => "Finish scoping, then call `teamwork_record_brief`.".to_string(),
        Phase::Planning => "Call `teamwork_record_plan` with milestones and non-overlapping tracks, including the model and reasoning effort for each track.".to_string(),
        Phase::Executing => {
            let Some(milestone) = run.current() else {
                return "Call `teamwork_status`.".to_string();
            };
            if milestone.fix_rounds > 0 {
                format!(
                    "Fix the gate findings for milestone `{}` (fix round {} of {MAX_FIX_ROUNDS}), verify the fixes, then call `teamwork_submit_for_review` with milestone_id `{}`.",
                    milestone.id, milestone.fix_rounds, milestone.id
                )
            } else {
                format!(
                    "Implement milestone `{}` by spawning one worker per track, integrate and verify the results, then call `teamwork_submit_for_review` with milestone_id `{}`.",
                    milestone.id, milestone.id
                )
            }
        }
        Phase::Gating | Phase::FinalAudit => {
            let open = run.open_tickets().collect::<Vec<_>>();
            if open.is_empty() {
                return "Call `teamwork_status`.".to_string();
            }
            let tokens = open
                .iter()
                .map(|ticket| format!("`{}` ({})", ticket.token, ticket.kind.title()))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Make sure one independent gate agent is running for each open ticket: {tokens}. Spawn any missing ones without parent history, using the ticket's `spawn_message` from `teamwork_status`. Then wait for them. The harness advances the run when every verdict is in."
            )
        }
        Phase::FinalFix => format!(
            "Fix the Success Auditor's findings (fix round {} of {MAX_FIX_ROUNDS}), verify them, then call `teamwork_submit_for_review` with milestone_id `{FINAL_MILESTONE_ID}`.",
            run.final_fix_rounds
        ),
        Phase::Complete => {
            "The run is complete. Report the outcome to the user in the final output format."
                .to_string()
        }
        Phase::Blocked => format!(
            "The run is blocked: {}. Explain to the user what you need from them, then end your turn.",
            run.blocked_reason.as_deref().unwrap_or("waiting for the user")
        ),
    }
}

pub(crate) fn status_markdown(run: &TeamworkRun) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "Run: {} (revision {})", run.run_id, run.revision);
    let _ = writeln!(text, "Phase: {}", run.phase);
    if let Some(reason) = &run.blocked_reason {
        let _ = writeln!(text, "Blocked reason: {reason}");
    }
    let _ = writeln!(text, "Goal: {}", escape_xml(run.goal.trim()));
    if let Some(brief) = &run.brief {
        let _ = writeln!(text, "Objective: {}", escape_xml(brief.objective.trim()));
        let _ = writeln!(text, "Acceptance criteria:");
        for criterion in &brief.acceptance_criteria {
            let _ = writeln!(
                text,
                "- {}: {} (verify: {})",
                criterion.id,
                escape_xml(&criterion.description),
                escape_xml(&criterion.verification)
            );
        }
    }
    if !run.milestones.is_empty() {
        let _ = writeln!(text, "Milestones:");
        for milestone in &run.milestones {
            let status = match milestone.status {
                MilestoneStatus::Pending => "pending",
                MilestoneStatus::InProgress => "in progress",
                MilestoneStatus::InReview => "in review",
                MilestoneStatus::Done => "done",
            };
            let _ = writeln!(
                text,
                "- {} [{status}] {} (failed review rounds: {})",
                milestone.id,
                escape_xml(&milestone.title),
                milestone.fix_rounds
            );
            for track in &milestone.tracks {
                let _ = writeln!(
                    text,
                    "  - track {} ({}; model: {}; effort: {}): {}",
                    track.id,
                    track.role,
                    track.model.as_deref().unwrap_or("inherit"),
                    track.reasoning_effort.as_deref().unwrap_or("inherit"),
                    track.files.join(", ")
                );
            }
        }
    }
    let relevant_tickets = run
        .gates
        .iter()
        .filter(|ticket| ticket.status == TicketStatus::Open || is_latest_round(run, ticket))
        .collect::<Vec<_>>();
    if !relevant_tickets.is_empty() {
        let _ = writeln!(text, "Gates (latest round):");
        for ticket in relevant_tickets {
            let status = match ticket.status {
                TicketStatus::Open => "open",
                TicketStatus::Passed => "PASS",
                TicketStatus::Failed => "FAIL",
            };
            let _ = writeln!(
                text,
                "- {} {} round {} [{status}] token `{}`",
                ticket.milestone_id.as_deref().unwrap_or(FINAL_MILESTONE_ID),
                ticket.kind.title(),
                ticket.round,
                ticket.token
            );
            if ticket.status == TicketStatus::Failed && !ticket.findings.trim().is_empty() {
                let _ = writeln!(text, "  findings: {}", escape_xml(ticket.findings.trim()));
            }
        }
    }
    text.trim_end().to_string()
}

fn is_latest_round(run: &TeamworkRun, ticket: &GateTicket) -> bool {
    match ticket.milestone_id.as_deref() {
        Some(milestone_id) => run
            .milestones
            .iter()
            .find(|milestone| milestone.id == milestone_id)
            .is_some_and(|milestone| milestone.review_round == ticket.round),
        None => ticket.round == run.final_review_round,
    }
}

/// Recommended model tier for a gate agent. The lead picks the concrete model.
pub(crate) fn gate_model_guidance(kind: GateKind) -> &'static str {
    match kind {
        GateKind::Critic | GateKind::Challenger | GateKind::SuccessAuditor => {
            "strongest available model, `high` reasoning effort (`xhigh` when offered)"
        }
        GateKind::Auditor => "strong model, `high` reasoning effort",
    }
}

/// Builds the standalone task message for an independent gate agent.
pub(crate) fn gate_spawn_message(run: &TeamworkRun, ticket: &GateTicket) -> String {
    let mut context = String::new();
    if let Some(brief) = &run.brief {
        let _ = writeln!(context, "Objective: {}", escape_xml(brief.objective.trim()));
        for requirement in &brief.requirements {
            let _ = writeln!(context, "Requirement: {}", escape_xml(requirement));
        }
        for item in &brief.out_of_scope {
            let _ = writeln!(context, "Out of scope: {}", escape_xml(item));
        }
        let _ = writeln!(context, "Acceptance criteria:");
        for criterion in &brief.acceptance_criteria {
            let _ = writeln!(
                context,
                "- {}: {} (verify: {})",
                criterion.id,
                escape_xml(&criterion.description),
                escape_xml(&criterion.verification)
            );
        }
    }
    let (scope_label, submission) = match ticket.milestone_id.as_deref() {
        Some(milestone_id) => {
            let milestone = run
                .milestones
                .iter()
                .find(|milestone| milestone.id == milestone_id);
            if let Some(milestone) = milestone {
                let _ = writeln!(
                    context,
                    "Milestone under review: {} - {}\nDeliverable: {}",
                    milestone.id,
                    escape_xml(&milestone.title),
                    escape_xml(&milestone.deliverable)
                );
                for track in &milestone.tracks {
                    let _ = writeln!(
                        context,
                        "Track {}: {} (files: {})",
                        track.id,
                        escape_xml(&track.description),
                        track.files.join(", ")
                    );
                }
            }
            (
                format!("milestone `{milestone_id}`"),
                milestone.and_then(|milestone| milestone.last_submission.clone()),
            )
        }
        None => (
            "the complete result of the run".to_string(),
            run.final_submission.clone(),
        ),
    };
    if let Some(submission) = submission {
        let _ = writeln!(
            context,
            "Lead's submission summary (a claim to verify, not evidence): {}",
            escape_xml(submission.trim())
        );
    }

    let instructions = match ticket.kind {
        GateKind::Critic => {
            "- Review the changes for this milestone (for example with `git diff` and by reading the touched files) for correctness bugs, regressions, missing edge cases, and violations of repository conventions.\n- Report only concrete, actionable defects, with file:line and a failure scenario. Do not report style preferences."
        }
        GateKind::Challenger => {
            "- Try to break the result: adversarial inputs, failure paths, concurrency, resource limits, and unstated assumptions.\n- Write or run targeted checks where possible, in a scratch location outside the tracked files. Report each failure with a reproduction."
        }
        GateKind::Auditor => {
            "- Check the milestone's deliverable and the acceptance criteria it affects, one by one.\n- Re-run the verification commands yourself. A criterion counts as verified only when you observed the evidence in this session."
        }
        GateKind::SuccessAuditor => {
            "- Audit the whole result against every acceptance criterion in the brief.\n- Run the full verification suite named in the brief. Treat missing, partial, or indirect evidence as a failure."
        }
    };

    format!(
        "<role>\nYou are the {title} gate for {scope_label} in a /teamwork run led by another agent. You are an independent reviewer: you did not write this code, and you do not edit tracked files.\n</role>\n\n<objective>\nDecide whether {scope_label} meets its bar, and record a PASS or FAIL verdict with the harness.\n</objective>\n\n<context>\n{context}</context>\n\n<instructions>\n{instructions}\n- Do not modify tracked files. If you edit files, the harness rejects your verdict.\n- Keep going until you can give a verdict. Make reasonable assumptions instead of asking questions.\n</instructions>\n\n<verdict>\nWhen you are done, call `{SUBMIT_VERDICT_TOOL_NAME}` exactly once with:\n- gate_token: `{token}`\n- verdict: `pass` or `fail`\n- findings: concrete defects (required for `fail`)\n- evidence: the commands and checks you ran and what they showed (required for `pass`)\n</verdict>\n\n<output_format>\nAfter recording the verdict, reply with: Verdict, Findings (file:line, failure scenario), Evidence (commands run and results).\n</output_format>",
        title = ticket.kind.title(),
        token = ticket.token,
    )
}

pub(crate) fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
#[path = "prompts_tests.rs"]
mod tests;
