//! TUI side of the `/swarm` slash command.
//!
//! The swarm itself is run by the harness (`codex-swarm-extension`), which recognizes a user
//! message that starts with `/swarm`. The TUI only forwards the command text and shows usage help.

use crate::chatwidget::UserMessage;

pub(crate) const SWARM_COMMAND_PREFIX: &str = "/swarm ";
pub(crate) const SWARM_USAGE: &str =
    "Usage: /swarm <task> | board | status | say <message with @mentions> | stop";
pub(crate) const SWARM_USAGE_HINT: &str =
    "Example: /swarm add OAuth login with backend, frontend, and end-to-end tests";
pub(crate) const SWARM_SUBAGENTS_DISABLED_HINT: &str = "Subagents are disabled. A swarm needs one subagent per member, so the coordinator cannot spawn its workers and verifiers. Enable subagents in /experimental.";

/// Prepends the literal `/swarm ` command so the harness can recognize the message, shifting text
/// elements so they keep pointing at the same placeholders.
pub(crate) fn prefix_swarm_command(message: &mut UserMessage) {
    message.text.insert_str(0, SWARM_COMMAND_PREFIX);
    for element in &mut message.text_elements {
        element.byte_range.start += SWARM_COMMAND_PREFIX.len();
        element.byte_range.end += SWARM_COMMAND_PREFIX.len();
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
            text: "board [Image #1] review".to_string(),
            local_images: Vec::new(),
            remote_image_urls: Vec::new(),
            text_elements: vec![TextElement::new(
                ByteRange { start: 6, end: 16 },
                Some("[Image #1]".to_string()),
            )],
            mention_bindings: Vec::new(),
        };
        prefix_swarm_command(&mut message);
        assert_eq!(message.text, "/swarm board [Image #1] review");
        let range = message.text_elements[0].byte_range;
        assert_eq!(&message.text[range.start..range.end], "[Image #1]");
    }
}
