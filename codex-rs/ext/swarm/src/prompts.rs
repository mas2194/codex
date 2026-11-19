//! Model-facing text owned by the harness: coordinator instructions, member briefs, board
//! deliveries, continuation nudges, and status and board renderings.

use std::fmt::Write as _;
use std::path::Path;

use crate::state::Activity;
use crate::state::BoardMessage;
use crate::state::COORDINATOR;
use crate::state::MAX_COORDINATOR_NUDGES;
use crate::state::MAX_FIX_ROUNDS;
use crate::state::Member;
use crate::state::Phase;
use crate::state::Role;
use crate::state::SwarmRun;
use crate::state::Verdict;

const COORDINATOR_TEMPLATE: &str = include_str!("../templates/coordinator.md");
const MEMBER_TEMPLATE: &str = include_str!("../templates/member.md");
const DELIVERY_TEMPLATE: &str = include_str!("../templates/delivery.md");

pub(crate) fn coordinator_prompt(task: &str, board_file: Option<&Path>) -> String {
    let hint = board_file.map_or_else(String::new, |path| {
        format!(" or by following `{}`", path.display())
    });
    COORDINATOR_TEMPLATE
        .replace("{{board_file_hint}}", &hint)
        .replace("{{task}}", &escape_xml(task.trim()))
}

pub(crate) fn usage_prompt() -> String {
    "The user typed `/swarm` without a task. Ask them for the task the swarm should deliver, and give one example such as `/swarm add OAuth login with backend, frontend, and tests`. Mention the subcommands `/swarm board`, `/swarm status`, `/swarm say <message with @mentions>`, and `/swarm stop`. Do not start any work yet.".to_string()
}

fn role_instructions(run: &SwarmRun, member: &Member) -> String {
    let verifiers = run
        .members
        .iter()
        .filter(|other| {
            other.activity != Activity::Retired && other.verifies.contains(&member.name)
        })
        .map(|other| format!("@{}", other.name))
        .collect::<Vec<_>>()
        .join(", ");
    match member.role {
        Role::Worker => format!(
            "<scope>You own and may edit only: {}. Other agents edit other files in parallel. Do not revert or overwrite changes you did not make. The harness records edits outside your files as violations.</scope>\n\n<instructions>\n- Keep going until your assignment is complete. Make reasonable assumptions instead of asking, and state them.\n- When the work is done and your checks pass, call `swarm_report_done` with what changed, which files, and the checks you ran with their results. The harness sends your report to your verifier ({}).\n- If your verifier reports defects, fix them and call `swarm_report_done` again. After {MAX_FIX_ROUNDS} failed rounds the coordinator takes over.\n</instructions>",
            member.files.join(", "),
            verifiers,
        ),
        Role::Verifier => format!(
            "<scope>You verify: {}. You never edit files; an edit disqualifies you as a verifier.</scope>\n\n<instructions>\n- Wait until a worker you verify reports done. The harness wakes you with the report.\n- Verify adversarially: read the diff, run the relevant build, tests, and linters, and try edge cases and failure paths. Assume the work is wrong until the evidence shows otherwise.\n- Record your verdict with `swarm_verify`. `pass` needs evidence (commands and results). `fail` needs findings with file, defect, and how to reproduce it; the harness sends them to the worker.\n- Judge only what the worker owns and its assignment. Report problems in other areas to `@coordinator`.\n</instructions>",
            if member.verifies.is_empty() {
                "workers assigned by the harness".to_string()
            } else {
                member
                    .verifies
                    .iter()
                    .map(|worker| format!("@{worker}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ),
    }
}

/// Standalone brief the coordinator passes unchanged to `spawn_agent` for one member.
pub(crate) fn member_spawn_message(run: &SwarmRun, member: &Member) -> String {
    MEMBER_TEMPLATE
        .replace("{{name}}", &member.name)
        .replace("{{role}}", member.role.as_str())
        .replace("{{token}}", &member.join_token)
        .replace("{{assignment}}", &escape_xml(&member.assignment))
        .replace("{{task}}", &escape_xml(&run.task))
        .replace("{{roster}}", &roster_lines(run))
        .replace("{{role_instructions}}", &role_instructions(run, member))
}

fn roster_lines(run: &SwarmRun) -> String {
    let mut lines = vec![format!("- @{COORDINATOR}: coordinator")];
    for member in run
        .members
        .iter()
        .filter(|member| member.activity != Activity::Retired)
    {
        let mut line = format!("- @{} ({})", member.name, member.role.as_str());
        if !member.groups.is_empty() {
            let _ = write!(
                line,
                " groups: {}",
                member
                    .groups
                    .iter()
                    .map(|group| format!("@{group}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        match member.role {
            Role::Worker => {
                let _ = write!(line, " owns: {}", member.files.join(", "));
            }
            Role::Verifier => {
                let _ = write!(line, " verifies: {}", member.verifies.join(", "));
            }
        }
        let _ = write!(line, " - {}", one_line(&member.assignment, 120));
        lines.push(line);
    }
    escape_xml(&lines.join("\n"))
}

/// Turn input that hands a woken agent the messages addressed to it.
pub(crate) fn delivery_prompt(run: &SwarmRun, name: &str, messages: &[BoardMessage]) -> String {
    let messages = messages
        .iter()
        .map(render_message)
        .collect::<Vec<_>>()
        .join("\n\n");
    let status = if name == COORDINATOR {
        format!("{}\n\nNext: {}", status_markdown(run), next_action(run))
    } else {
        run.member(name)
            .map_or_else(String::new, member_status_line)
    };
    DELIVERY_TEMPLATE
        .replace("{{name}}", name)
        .replace("{{messages}}", &escape_xml(&messages))
        .replace("{{status}}", &escape_xml(&status))
}

pub(crate) fn nudge_prompt(run: &SwarmRun, instruction: &str) -> String {
    format!(
        "The `/swarm` run is still active, so the harness resumed you.\n\n<swarm_state>\n{}\n</swarm_state>\n\n<next_action>\n{}\n</next_action>\n\nWork from the harness state, not from memory. After {MAX_COORDINATOR_NUDGES} resumptions in a row without any change to the swarm, the harness stops resuming you and waits for the user.",
        escape_xml(&status_markdown(run)),
        escape_xml(instruction)
    )
}

/// Reminder attached to ordinary user messages while a swarm is active.
pub(crate) fn user_turn_reminder(run: &SwarmRun) -> String {
    format!(
        "A `/swarm` run is active in this thread and you are its coordinator. Treat the user's message as input to the swarm unless they ask to stop it. Relay instructions to members with `swarm_post`.\n\n<swarm_state>\n{}\n</swarm_state>\n\n<next_action>\n{}\n</next_action>",
        escape_xml(&status_markdown(run)),
        escape_xml(&next_action(run))
    )
}

pub(crate) fn next_action(run: &SwarmRun) -> String {
    match run.phase {
        Phase::Forming => "Inspect the repository, then call `swarm_define_roster`.".to_string(),
        Phase::Finished => "The swarm is finished. Report the outcome to the user.".to_string(),
        Phase::Running if run.all_workers_verified() => {
            "Every active worker is verified. Run the final checks and call `swarm_finish`."
                .to_string()
        }
        Phase::Running => {
            let invited = run
                .members
                .iter()
                .filter(|member| member.activity == Activity::Invited)
                .map(|member| format!("@{}", member.name))
                .collect::<Vec<_>>();
            if invited.is_empty() {
                "Members are working. End your turn to park; the harness wakes you when someone mentions @coordinator.".to_string()
            } else {
                format!(
                    "Spawn the members that have not joined ({}) with their spawn_message, then end your turn to park.",
                    invited.join(", ")
                )
            }
        }
    }
}

fn member_status_line(member: &Member) -> String {
    let mut line = format!(
        "@{} ({}, {})",
        member.name,
        member.role.as_str(),
        member.activity.as_str()
    );
    if let Some(progress) = member.progress {
        let _ = write!(line, ", work: {}", progress.as_str());
        if member.fix_rounds > 0 {
            let _ = write!(
                line,
                ", failed rounds: {}/{MAX_FIX_ROUNDS}",
                member.fix_rounds
            );
        }
    }
    match member.role {
        Role::Worker => {
            let _ = write!(line, ", owns: {}", member.files.join(", "));
        }
        Role::Verifier => {
            let _ = write!(line, ", verifies: {}", member.verifies.join(", "));
        }
    }
    if let Some(model) = &member.model {
        let _ = write!(line, ", model: {model}");
    }
    if let Some(effort) = &member.reasoning_effort {
        let _ = write!(line, ", effort: {effort}");
    }
    if let Some(verdict) = &member.last_verdict {
        let verdict_text = match verdict.verdict {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
        };
        let _ = write!(
            line,
            ", last verdict: {verdict_text} by @{}",
            verdict.verifier
        );
    }
    line
}

/// Authoritative summary of the run for the coordinator.
pub(crate) fn status_markdown(run: &SwarmRun) -> String {
    let mut text = format!(
        "Swarm `{}`: phase {}, board messages {}, revision {}\nTask: {}\n",
        run.id,
        run.phase.as_str(),
        run.latest_seq(),
        run.revision,
        one_line(&run.task, 300)
    );
    if run.members.is_empty() {
        text.push_str("Roster: none yet\n");
    } else {
        text.push_str("Roster:\n");
        for member in &run.members {
            let _ = writeln!(text, "- {}", member_status_line(member));
        }
    }
    if !run.violations.is_empty() {
        text.push_str("File ownership violations:\n");
        for violation in &run.violations {
            let _ = writeln!(
                text,
                "- `{}` edited `{}`: {}",
                violation.agent, violation.path, violation.reason
            );
        }
    }
    if run.stalled {
        text.push_str("The harness stopped resuming the coordinator after repeated turns without progress. A user message restarts it.\n");
    }
    if let Some(summary) = &run.final_summary {
        let _ = writeln!(text, "Final summary: {}", one_line(summary, 400));
    }
    text.trim_end().to_string()
}

fn render_message(message: &BoardMessage) -> String {
    let to = if message.recipients.is_empty() {
        "board".to_string()
    } else {
        message
            .recipients
            .iter()
            .map(|recipient| format!("@{recipient}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    format!(
        "#{} [{}] from @{} to {}\n{}",
        message.seq,
        message.kind.as_str(),
        message.from,
        to,
        message.body
    )
}

/// The board as Markdown, optionally only the latest `limit` messages.
pub(crate) fn board_markdown(run: &SwarmRun, limit: Option<usize>) -> String {
    let mut text = format!(
        "# Swarm board\n\nTask: {}\nPhase: {}\n\n## Members\n\n",
        one_line(&run.task, 300),
        run.phase.as_str()
    );
    let _ = writeln!(text, "- @{COORDINATOR} (coordinator)");
    for member in &run.members {
        let _ = writeln!(text, "- {}", member_status_line(member));
    }
    text.push_str("\n## Messages\n\n");
    let skip = limit.map_or(0, |limit| run.board.len().saturating_sub(limit));
    if skip > 0 {
        let _ = writeln!(text, "_{skip} earlier messages omitted._\n");
    }
    if run.board.is_empty() {
        text.push_str("_No messages yet._\n");
    }
    for message in run.board.iter().skip(skip) {
        let _ = writeln!(text, "{}\n", render_message(message));
    }
    text
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        line
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

pub(crate) fn escape_xml(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MemberSpec;
    use crate::state::MessageKind;

    fn run_with_roster() -> SwarmRun {
        let mut run = SwarmRun::new("r1".into(), "coord".into(), "Add <OAuth> login".into());
        run.define_roster(vec![
            MemberSpec {
                name: "api".into(),
                role: Role::Worker,
                assignment: "Build the backend".into(),
                files: vec!["server/".into()],
                groups: vec!["backend".into()],
                verifies: Vec::new(),
                model: Some("fast".into()),
                reasoning_effort: Some("low".into()),
            },
            MemberSpec {
                name: "check".into(),
                role: Role::Verifier,
                assignment: "Verify the backend".into(),
                files: Vec::new(),
                groups: Vec::new(),
                verifies: Vec::new(),
                model: None,
                reasoning_effort: None,
            },
        ])
        .expect("roster");
        run
    }

    #[test]
    fn coordinator_prompt_escapes_the_task() {
        let prompt = coordinator_prompt("ship <it>", Some(Path::new("/tmp/b.md")));
        assert!(prompt.contains("<user_task>\nship &lt;it&gt;\n</user_task>"));
        assert!(prompt.contains("`/tmp/b.md`"));
        assert!(!prompt.contains("{{"));
    }

    #[test]
    fn spawn_message_carries_token_scope_and_roster() {
        let run = run_with_roster();
        let api = run.member("api").expect("api");
        let message = member_spawn_message(&run, api);
        assert!(message.contains(&api.join_token));
        assert!(message.contains("You own and may edit only: server/"));
        assert!(message.contains("@check (verifier) verifies: api"));
        assert!(message.contains("Add &lt;OAuth&gt; login"));
        assert!(!message.contains("{{"));

        let check = run.member("check").expect("check");
        let message = member_spawn_message(&run, check);
        assert!(message.contains("You verify: @api"));
    }

    #[test]
    fn delivery_lists_messages_for_the_member() {
        let mut run = run_with_roster();
        run.post("check", &[], MessageKind::Question, "@api which port?")
            .expect("post");
        let pending = run.pending_for("api");
        let prompt = delivery_prompt(&run, "api", &pending);
        assert!(prompt.contains("from @check to @api"));
        assert!(prompt.contains("which port?"));
        assert!(prompt.contains("work: in_progress"));
    }

    #[test]
    fn board_markdown_limits_messages() {
        let mut run = run_with_roster();
        for index in 0..5 {
            run.post("api", &[], MessageKind::Note, &format!("note {index}"))
                .expect("post");
        }
        let board = board_markdown(&run, Some(2));
        assert!(board.contains("note 4"));
        assert!(!board.contains("note 2"));
        assert!(board.contains("earlier messages omitted"));
    }
}
