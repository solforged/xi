//! Width and height tiers.
//!
//! xi lays itself out from the size of its own pane in cells, which every
//! terminal reports the same way. The width picks a tier: a [`Tier::Slit`]
//! under 72 columns, a [`Tier::Pane`] up to 119, and a [`Tier::Sheet`] from
//! 120, where output stays on a 120-column sheet at the left and prose wraps
//! at 88. Under 20 rows the live region shrinks too ([`Measure::short`]).
//! The deck (alternate screen with the log and a side panel) is a
//! [`Layout`] choice on top. `layout = "auto"`, the default, takes it when
//! xi starts wide enough for the panel beside the sheet ([`DECK_FROM`]).
//! The panel grows with the window only up to [`PANEL_MAX`]; further
//! columns go to the log, so the panel stays against the right edge.

/// Columns below which the client is a slit.
pub const SLIT_BELOW: usize = 72;
/// Columns from which output is held to a sheet.
pub const SHEET_FROM: usize = 120;
/// Width of the sheet.
pub const SHEET: usize = 120;
/// Width prose and thinking wrap at on the sheet.
pub const PROSE: usize = 88;
/// Rows below which the live region shrinks.
pub const SHORT_BELOW: usize = 20;
/// The narrowest side panel the deck shows beside the log.
pub const PANEL_MIN: usize = 36;
/// The widest side panel beside the log. It stays against the right edge;
/// columns past it go to the log, so resizing a wide terminal does not
/// stretch the panel.
pub const PANEL_MAX: usize = 64;
/// The size from which `layout = "auto"` chooses the deck at start: the
/// sheet with its gutter and margin, a separator and the narrowest panel.
pub const DECK_FROM: (usize, usize) = (SHEET + 3 + PANEL_MIN, 30);

/// Width tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Under 72 columns: a brass rule and a prompt for the composer, file
    /// names and outcomes in headlines, no durations.
    Slit,
    /// 72 to 119 columns: every row spans the pane.
    Pane,
    /// 120 columns and up: output stays on a 120-column sheet, prose wraps
    /// at 88, and the composer and footer sit on the sheet.
    Sheet,
}

impl Tier {
    /// The tier `cols` picks.
    #[must_use]
    pub fn of(cols: usize) -> Self {
        if cols < SLIT_BELOW {
            Self::Slit
        } else if cols < SHEET_FROM {
            Self::Pane
        } else {
            Self::Sheet
        }
    }

    /// The name shown in notes and settings.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Slit => "slit",
            Self::Pane => "pane",
            Self::Sheet => "sheet",
        }
    }
}

/// How the client owns the terminal, from `[display] layout` in `tui.toml`
/// or `/layout`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    /// Scrollback is the log; never enters the alternate screen by itself.
    Inline,
    /// The alternate screen with the log and a side panel.
    Deck,
    /// The deck when xi starts at [`DECK_FROM`] or larger, else inline.
    #[default]
    Auto,
}

impl Layout {
    /// Parses a config or command value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "inline" => Some(Self::Inline),
            "deck" => Some(Self::Deck),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// The config value.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Deck => "deck",
            Self::Auto => "auto",
        }
    }

    /// Whether this layout opens the deck at `cols`×`rows`.
    #[must_use]
    pub fn deck_at(self, cols: usize, rows: usize) -> bool {
        match self {
            Self::Inline => false,
            Self::Deck => true,
            Self::Auto => cols >= DECK_FROM.0 && rows >= DECK_FROM.1,
        }
    }
}

/// The measurements one frame draws with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measure {
    /// The width tier.
    pub tier: Tier,
    /// Under 20 rows: the live region takes at most half the rows, frames
    /// show five rows and their hints, and full thinking streams one row.
    pub short: bool,
    /// Columns of the pane drawn into.
    pub cols: usize,
    /// Rows of the pane drawn into.
    pub rows: usize,
    /// Columns output rows span and outcomes align to: the sheet's 120 on
    /// the sheet, else `cols`.
    pub out: usize,
    /// Columns prose, thinking and prompts wrap at, before their two-cell
    /// indent: 88 on the sheet, else `out - 2`.
    pub prose: usize,
}

impl Measure {
    /// The measure of a `cols`×`rows` pane.
    #[must_use]
    pub fn new(cols: usize, rows: usize) -> Self {
        let tier = Tier::of(cols);
        let out = match tier {
            Tier::Sheet => SHEET,
            _ => cols,
        };
        let prose = match tier {
            Tier::Sheet => PROSE,
            _ => out.saturating_sub(2),
        };
        Self {
            tier,
            short: rows < SHORT_BELOW,
            cols,
            rows,
            out,
            prose,
        }
    }

    /// Whether headlines carry durations and token counts.
    #[must_use]
    pub fn durations(self) -> bool {
        self.tier != Tier::Slit
    }

    /// Streamed prose rows kept in the live region.
    #[must_use]
    pub fn stream_rows(self) -> usize {
        if self.short { 1 } else { 6 }
    }

    /// Composer rows shown before it scrolls.
    #[must_use]
    pub fn composer_rows(self) -> usize {
        if self.short { 3 } else { 8 }
    }

    /// Rows the live region may take.
    #[must_use]
    pub fn live_rows(self) -> usize {
        if self.short {
            (self.rows / 2).max(3)
        } else {
            self.rows.saturating_sub(1)
        }
    }

    /// Rows a frame may take out of `available`: five body rows, the title
    /// and the hints when short.
    #[must_use]
    pub fn frame_rows(self, available: usize) -> usize {
        if self.short {
            available.min(7)
        } else {
            available
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_picks_the_tier_and_the_sheet_holds_output_and_prose() {
        assert_eq!(Measure::new(60, 36).tier, Tier::Slit);
        assert_eq!(Measure::new(71, 36).tier, Tier::Slit);
        assert_eq!(Measure::new(72, 36).tier, Tier::Pane);
        assert_eq!(Measure::new(119, 36).tier, Tier::Pane);
        assert_eq!(Measure::new(120, 36).tier, Tier::Sheet);
        let m = Measure::new(250, 36);
        assert_eq!((m.out, m.prose), (120, 88));
        assert!(m.durations());
        let m = Measure::new(82, 36);
        assert_eq!((m.out, m.prose), (82, 80));
        let m = Measure::new(60, 36);
        assert_eq!((m.out, m.prose), (60, 58));
        assert!(!m.durations());
    }

    #[test]
    fn short_applies_on_top_of_any_tier() {
        let m = Measure::new(124, 17);
        assert_eq!(m.tier, Tier::Sheet);
        assert!(m.short);
        assert_eq!(m.live_rows(), 8);
        assert_eq!(m.stream_rows(), 1);
        assert_eq!(m.composer_rows(), 3);
        assert_eq!(m.frame_rows(12), 7);
        let m = Measure::new(124, 20);
        assert!(!m.short);
        assert_eq!(m.live_rows(), 19);
        assert_eq!(m.frame_rows(12), 12);
    }

    #[test]
    fn auto_opens_the_deck_only_at_start_size() {
        assert!(Layout::Auto.deck_at(159, 30));
        assert!(!Layout::Auto.deck_at(158, 30));
        assert!(!Layout::Auto.deck_at(159, 29));
        assert_eq!(Layout::default(), Layout::Auto);
        assert!(Layout::Deck.deck_at(80, 24));
        assert!(!Layout::Inline.deck_at(250, 36));
        assert_eq!(Layout::parse("deck"), Some(Layout::Deck));
        assert_eq!(Layout::parse("wide"), None);
    }
}
