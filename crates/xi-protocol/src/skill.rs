//! Explicit skill invocations in user messages.
//!
//! A prompt whose first text block starts with `/skill:NAME` invokes the
//! skill `NAME`. The kernel stores the message with the skill's instructions
//! as an extra leading block and the typed blocks unchanged after it, so
//! clients can show and resend what the user typed with [`typed`].

use crate::content::Block;

/// Prefix of a prompt that invokes a skill, as in `/skill:review the diff`.
pub const PREFIX: &str = "/skill:";

const OPEN: &str = "<skill_content name=";
const CLOSE: &str = "</skill_content>";

/// Splits `/skill:NAME ARGS` into the name and the trimmed arguments.
/// `None` when the text does not invoke a skill or names none.
#[must_use]
pub fn invocation(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix(PREFIX)?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()).then(|| (name, args.trim()))
}

/// The leading block that carries a skill's instructions into the
/// conversation. `directory` is the folder holding the skill's `SKILL.md`.
#[must_use]
pub fn instructions(name: &str, directory: &str, body: &str) -> Block {
    let name = serde_json::to_string(name).expect("string serializes");
    Block::Text {
        text: format!(
            "{OPEN}{name}>\n{}\n\nSkill directory: {directory}\nRelative paths in this skill are relative to the skill directory.\n{CLOSE}",
            body.trim()
        ),
    }
}

/// The blocks the user typed: an expanded invocation without its injected
/// instructions, and any other message unchanged.
#[must_use]
pub fn typed(content: &[Block]) -> &[Block] {
    match content {
        [Block::Text { text: injected }, Block::Text { text }, ..]
            if injected.starts_with(OPEN)
                && injected.ends_with(CLOSE)
                && invocation(text).is_some() =>
        {
            &content[1..]
        }
        _ => content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Block {
        Block::Text { text: text.into() }
    }

    #[test]
    fn invocation_splits_name_and_trimmed_arguments() {
        assert_eq!(
            invocation("/skill:review  the diff\nplease "),
            Some(("review", "the diff\nplease"))
        );
        assert_eq!(invocation("/skill:review"), Some(("review", "")));
        assert_eq!(invocation("/skill: review"), None);
        assert_eq!(invocation("please /skill:review"), None);
    }

    #[test]
    fn typed_drops_only_injected_instructions_before_an_invocation() {
        let prompt = text("/skill:review the diff");
        let expanded = vec![
            instructions("review", "/skills/review", "Check </skill_content> twice."),
            prompt.clone(),
        ];
        assert_eq!(typed(&expanded), std::slice::from_ref(&prompt));
        // Pasted wrapper text without an invocation after it stays visible.
        let pasted = vec![
            instructions("review", "/skills/review", "body"),
            text("not a command"),
        ];
        assert_eq!(typed(&pasted), pasted.as_slice());
        assert_eq!(
            typed(std::slice::from_ref(&prompt)),
            std::slice::from_ref(&prompt)
        );
    }
}
