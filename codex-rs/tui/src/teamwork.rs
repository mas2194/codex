//! TUI side of the `/teamwork` slash command.
//!
//! The workflow itself is enforced by the harness (`codex-teamwork-extension`), which recognizes a
//! user message that starts with `/teamwork`. The TUI only forwards the command text and shows
//! usage help.

use crate::chatwidget::UserMessage;

pub(crate) const TEAMWORK_COMMAND_PREFIX: &str = "/teamwork ";
pub(crate) const TEAMWORK_USAGE: &str = "Usage: /teamwork <goal> | status | stop";
pub(crate) const TEAMWORK_USAGE_HINT: &str =
    "Example: /teamwork migrate the REST service from Express to Fastify with full test coverage";
pub(crate) const TEAMWORK_SUBAGENTS_DISABLED_HINT: &str = "Subagents are disabled. The harness requires independent gate agents, so the run will block at its first review gate. Enable subagents in /experimental.";

/// Prepends the literal `/teamwork ` command so the harness can recognize the message, shifting
/// text elements so they keep pointing at the same placeholders.
pub(crate) fn prefix_teamwork_command(message: &mut UserMessage) {
    message.text.insert_str(0, TEAMWORK_COMMAND_PREFIX);
    for element in &mut message.text_elements {
        element.byte_range.start += TEAMWORK_COMMAND_PREFIX.len();
        element.byte_range.end += TEAMWORK_COMMAND_PREFIX.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_protocol::user_input::ByteRange;
    use codex_protocol::user_input::TextElement;
    use pretty_assertions::assert_eq;

    #[test]
    fn prefix_keeps_text_elements_aligned() {
        let mut message = UserMessage {
            text: "fix [Image #1] layout".to_string(),
            local_images: Vec::new(),
            remote_image_urls: Vec::new(),
            text_elements: vec![TextElement::new(
                ByteRange { start: 4, end: 14 },
                Some("[Image #1]".to_string()),
            )],
            mention_bindings: Vec::new(),
        };
        prefix_teamwork_command(&mut message);
        assert_eq!(message.text, "/teamwork fix [Image #1] layout");
        let range = message.text_elements[0].byte_range;
        assert_eq!(&message.text[range.start..range.end], "[Image #1]");
    }
}
