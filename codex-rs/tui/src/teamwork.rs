//! Prompt construction for the `/teamwork` slash command.
//!
//! `/teamwork <goal>` asks the current (root) agent to act as a team lead: it scopes the goal,
//! plans milestones, delegates non-overlapping tracks to sub-agents, and verifies every milestone
//! through independent review gates. The lead picks the model and reasoning effort for each
//! sub-agent itself, so the command only supplies the orchestration instructions.

use codex_protocol::user_input::ByteRange;
use codex_protocol::user_input::TextElement;

const TEAMWORK_PROMPT_TEMPLATE: &str = include_str!("../assets/prompt_for_teamwork_command.md");
const GOAL_PLACEHOLDER: &str = "{{GOAL}}";

pub(crate) const TEAMWORK_USAGE: &str = "Usage: /teamwork <goal>";
pub(crate) const TEAMWORK_USAGE_HINT: &str =
    "Example: /teamwork migrate the REST service from Express to Fastify with full test coverage";
pub(crate) const TEAMWORK_SUBAGENTS_DISABLED_HINT: &str = "Subagents are disabled, so the lead agent will run every phase itself. Enable subagents in /experimental to let it delegate.";

/// Builds the lead-agent prompt for `goal`, shifting `text_elements` (which index into `goal`)
/// so they keep pointing at the same placeholders inside the returned prompt.
pub(crate) fn build_teamwork_prompt(
    goal: &str,
    text_elements: Vec<TextElement>,
) -> (String, Vec<TextElement>) {
    let (prefix, suffix) = TEAMWORK_PROMPT_TEMPLATE
        .split_once(GOAL_PLACEHOLDER)
        .unwrap_or((TEAMWORK_PROMPT_TEMPLATE, ""));
    let goal = goal.trim_end();
    let mut prompt = String::with_capacity(prefix.len() + goal.len() + suffix.len());
    prompt.push_str(prefix);
    prompt.push_str(goal);
    prompt.push_str(suffix);

    let offset = prefix.len();
    let text_elements = text_elements
        .into_iter()
        .filter(|element| element.byte_range.end <= goal.len())
        .map(|element| {
            element.map_range(|range| ByteRange {
                start: range.start + offset,
                end: range.end + offset,
            })
        })
        .collect();
    (prompt, text_elements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn template_has_exactly_one_goal_placeholder() {
        assert_eq!(
            TEAMWORK_PROMPT_TEMPLATE.matches(GOAL_PLACEHOLDER).count(),
            1
        );
    }

    #[test]
    fn goal_is_embedded_in_user_goal_block() {
        let (prompt, _) = build_teamwork_prompt("port the parser to Rust\n", Vec::new());
        assert!(prompt.contains("<user_goal>\nport the parser to Rust\n</user_goal>"));
        assert!(!prompt.contains(GOAL_PLACEHOLDER));
    }

    #[test]
    fn text_elements_are_shifted_into_the_prompt() {
        let goal = "fix [Image #1] layout";
        let element = TextElement::new(
            ByteRange { start: 4, end: 14 },
            Some("[Image #1]".to_string()),
        );
        let (prompt, elements) = build_teamwork_prompt(goal, vec![element]);
        assert_eq!(elements.len(), 1);
        let range = elements[0].byte_range;
        assert_eq!(&prompt[range.start..range.end], "[Image #1]");
    }
}
