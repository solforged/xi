//! Glyphs for the deck's chrome: Nerd Font icons, or Unicode and words for
//! fonts without them.
//!
//! Every Nerd Font glyph here is an Octicon from the Nerd Font private use
//! area, one cell wide, and always followed by a space so terminals that
//! draw it a little wide have room. The Unicode set keeps the deck's plain
//! labels, so nothing depends on the font.

use crate::style::Line;

/// Which glyph set the deck draws, from `[display] icons` in `tui.toml`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Icons {
    /// Nerd Font icons.
    #[default]
    Nerd,
    /// Unicode symbols and words, for fonts without Nerd Font glyphs.
    Unicode,
}

/// A thing the deck labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    /// The workspace path.
    Workspace,
    /// The git branch.
    Branch,
    /// The model.
    Model,
    /// The reasoning level.
    Reasoning,
    /// Running subagents.
    Agents,
    /// Asks waiting on you.
    Waiting,
    /// When the session started.
    Clock,
    /// The session's head event.
    Head,
    /// The session's event count.
    Events,
    /// Changed files.
    Changes,
    /// Other sessions.
    Sessions,
    /// Context occupancy.
    Context,
    /// The cache hit rate.
    Cache,
    /// The approval mode.
    Approvals,
    /// The sandbox is on.
    Sandbox,
    /// The sandbox is off.
    SandboxOff,
    /// The workspace is trusted.
    Trusted,
    /// The workspace is untrusted.
    Untrusted,
    /// An unanswered approval.
    Ask,
    /// The attached session.
    Current,
    /// A session with work in flight.
    Running,
    /// A session waiting for an answer.
    Paused,
    /// An inactive session.
    Idle,
}

impl Icons {
    /// Parses a config value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "nerd" => Some(Self::Nerd),
            "unicode" => Some(Self::Unicode),
            _ => None,
        }
    }

    /// The glyph alone, for marks; in Unicode the symbol the deck used
    /// before icons.
    #[must_use]
    pub fn get(self, icon: Icon) -> &'static str {
        let (glyph, before, _) = table(icon);
        match self {
            Self::Nerd => glyph,
            Self::Unicode => before.trim_end(),
        }
    }

    /// `value` labelled with `icon`, the label dim: the glyph and a space
    /// before it, or in Unicode the words the deck used before icons.
    #[must_use]
    pub fn label(self, icon: Icon, value: Line) -> Line {
        let (glyph, before, after) = table(icon);
        match self {
            Self::Nerd => Line::default().dim(glyph).ink(" ").extend(value),
            Self::Unicode => Line::default().dim(before).extend(value).dim(after),
        }
    }

    /// [`Icons::label`] as plain text.
    #[must_use]
    pub fn tag(self, icon: Icon, value: &str) -> String {
        self.label(icon, Line::default().ink(value)).plain()
    }

    /// The glyph of a built-in tool, which replaces its name at the start of
    /// a headline; `None` in Unicode and for other tools, which keep it.
    #[must_use]
    pub fn tool(self, name: &str) -> Option<&'static str> {
        if self == Self::Unicode {
            return None;
        }
        // Octicon names, as `oct-<name>` in the Nerd Font cheat sheet.
        Some(match name {
            "bash" => "\u{f489}",              // terminal
            "read" => "\u{f4a5}",              // file
            "edit" | "hashline" => "\u{f448}", // pencil
            "search" => "\u{f422}",            // search
            "fetch" => "\u{f484}",             // globe
            "task" => "\u{f52e}",              // workflow
            _ => return None,
        })
    }
}

/// The Nerd Font glyph of `icon`, and the words before and after a value
/// that stand in for it in Unicode.
fn table(icon: Icon) -> (&'static str, &'static str, &'static str) {
    // Octicon names, as `oct-<name>` in the Nerd Font cheat sheet.
    match icon {
        Icon::Workspace => ("\u{f413}", "", ""), // file_directory
        Icon::Branch => ("\u{f418}", "", ""),    // git_branch
        Icon::Model => ("\u{f51b}", "", ""),     // sparkle_fill
        Icon::Reasoning => ("\u{f400}", "think ", ""), // light_bulb
        Icon::Agents => ("\u{f4fd}", "agents ", " running"), // people
        Icon::Waiting => ("\u{f49a}", "? ", " waiting"), // bell
        Icon::Clock => ("\u{f43a}", "", ""),     // clock
        Icon::Head => ("\u{f417}", "head #", ""), // git_commit
        Icon::Events => ("\u{f4ed}", "", ""),    // log
        Icon::Changes => ("\u{f440}", "", ""),   // diff
        Icon::Sessions => ("\u{f442}", "", ""),  // comment_discussion
        Icon::Context => ("\u{f463}", "ctx ", ""), // meter
        Icon::Cache => ("\u{f49b}", "cache ", ""), // cache
        Icon::Approvals => ("\u{f45e}", "approvals ", ""), // checklist
        Icon::Sandbox => ("\u{f49c}", "◇ ", ""), // shield
        Icon::SandboxOff => ("\u{f513}", "", ""), // shield_x
        Icon::Trusted => ("\u{f4a1}", "", ""),   // verified
        Icon::Untrusted => ("\u{f4a3}", "", ""), // unverified
        Icon::Ask => ("\u{f420}", "? ", ""),     // question
        Icon::Current => ("\u{f444}", "● ", ""), // dot_fill
        Icon::Running => ("\u{f46a}", "┤ ", ""), // sync
        Icon::Paused => ("\u{f49a}", "? ", ""),  // bell
        Icon::Idle => ("\u{f4c3}", "· ", ""),    // dot
    }
}
