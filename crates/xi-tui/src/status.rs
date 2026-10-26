//! The status line: standing state, fitted to the width by priority.
//!
//! When space runs out the segment with the highest priority number shortens
//! first, then drops. Priority 0 never drops: context, taint, waiting
//! approvals and trust stay to the end. The desktop status line and phone
//! chips use the same order.

use crate::style::{Line, lr};

/// Which end of the status line a segment sits at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Where you are: workspace, branch.
    Left,
    /// What state you're in.
    Right,
}

/// One item of standing state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Stable name, also the key in [`priority`].
    pub name: &'static str,
    /// Full form.
    pub value: Line,
    /// Shorter form, used before the segment drops.
    pub short: Option<Line>,
    /// Which end it sits at.
    pub side: Side,
}

impl Segment {
    /// A right-side segment.
    #[must_use]
    pub fn new(name: &'static str, value: Line) -> Self {
        Self {
            name,
            value,
            short: None,
            side: Side::Right,
        }
    }

    /// Adds a shorter form.
    #[must_use]
    pub fn short(mut self, short: Line) -> Self {
        self.short = Some(short);
        self
    }

    /// Moves the segment to the left end.
    #[must_use]
    pub fn left(mut self) -> Self {
        self.side = Side::Left;
        self
    }
}

/// Drop order: higher numbers shorten and drop first; 0 never drops. Every
/// short form is used before any segment drops.
#[must_use]
pub fn priority(name: &str) -> u8 {
    match name {
        "context" | "taint" | "approvals" | "trust" => 0,
        "children" => 1,
        "model" => 2,
        "workspace" => 3,
        "cost" => 4,
        "reasoning" => 5,
        "branch" => 6,
        "cache" => 7,
        "sandbox" => 8,
        _ => 9,
    }
}

/// A fitted status line.
#[derive(Debug)]
pub struct Fitted {
    /// The row, exactly `cols` wide when anything fits.
    pub line: Line,
    /// Names of dropped segments, in drop order.
    pub dropped: Vec<&'static str>,
}

struct Slot {
    segment: Segment,
    short: bool,
}

impl Slot {
    fn value(&self) -> &Line {
        match (&self.segment.short, self.short) {
            (Some(short), true) => short,
            _ => &self.segment.value,
        }
    }
}

fn join(slots: &[Slot], side: Side, sep: &str) -> Line {
    let mut line = Line::default();
    for (i, slot) in slots.iter().filter(|s| s.segment.side == side).enumerate() {
        if i > 0 {
            line = line.dim(sep);
        }
        line = line.extend(slot.value().clone());
    }
    line
}

/// Fits segments into `cols` cells.
#[must_use]
pub fn fit(segments: Vec<Segment>, cols: usize) -> Fitted {
    let mut slots: Vec<Slot> = segments
        .into_iter()
        .map(|segment| Slot {
            segment,
            short: false,
        })
        .collect();
    let mut dropped = Vec::new();
    let width = |slots: &[Slot]| {
        2 + join(slots, Side::Left, "  ").width() + 2 + join(slots, Side::Right, " · ").width()
    };
    while width(&slots) > cols {
        let rank = |(i, s): &(usize, &Slot)| (priority(s.segment.name), std::cmp::Reverse(*i));
        let shorten = slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.segment.short.is_some() && !s.short)
            .max_by_key(rank)
            .map(|(i, _)| i);
        if let Some(i) = shorten {
            slots[i].short = true;
            continue;
        }
        let drop = slots
            .iter()
            .enumerate()
            .filter(|(_, s)| priority(s.segment.name) > 0)
            .max_by_key(rank)
            .map(|(i, _)| i);
        match drop {
            Some(i) => dropped.push(slots.remove(i).segment.name),
            None => break,
        }
    }
    let inner = cols.saturating_sub(2);
    let row = lr(
        join(&slots, Side::Left, "  "),
        join(&slots, Side::Right, " · "),
        inner,
    );
    Fitted {
        line: Line::default().ink(" ").extend(row).ink(" ").truncate(cols),
        dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments() -> Vec<Segment> {
        let d = |s: &str| Line::default().dim(s);
        vec![
            Segment::new("workspace", Line::default().ink("~/src/ledger"))
                .short(Line::default().ink("ledger"))
                .left(),
            Segment::new("branch", d("main*")).left(),
            Segment::new("model", Line::default().ink("sonnet-5.5"))
                .short(Line::default().ink("s-5.5")),
            Segment::new("reasoning", d("think med")),
            Segment::new("context", d("ctx 24%")),
            Segment::new("cache", d("cache 94%")),
            Segment::new("cost", Line::default().ink("$0.29")),
            Segment::new("sandbox", d("◇ sandbox")).short(d("◇")),
            Segment::new("taint", Line::default().murex("◆ tainted"))
                .short(Line::default().murex("◆")),
            Segment::new("approvals", Line::default().brass("? 1 waiting"))
                .short(Line::default().brass("?1")),
        ]
    }

    #[test]
    fn everything_fits_when_there_is_room() {
        let fitted = fit(segments(), 120);
        assert!(fitted.dropped.is_empty());
        assert_eq!(fitted.line.width(), 120);
    }

    #[test]
    fn short_forms_are_used_before_segments_drop_in_priority_order() {
        let fitted = fit(segments(), 60);
        assert_eq!(fitted.dropped, ["sandbox", "cache", "branch"]);
        let text = fitted.line.plain();
        assert!(text.starts_with(" ledger"), "{text}");
        assert!(
            text.contains("think med") && text.contains("$0.29"),
            "{text}"
        );
        assert!(text.contains("◆ · ?1"), "{text}");
        assert_eq!(fitted.line.width(), 60);
    }

    #[test]
    fn safety_state_never_drops() {
        let fitted = fit(segments(), 24);
        let text = fitted.line.plain();
        assert!(text.contains("ctx 24%"), "{text}");
        assert!(text.contains('◆') && text.contains("?1"), "{text}");
        assert!(fitted.line.width() <= 24);
    }
}
