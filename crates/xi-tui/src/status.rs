//! The status line: standing state, fitted to the width by priority.
//!
//! When space runs out the segment with the highest priority number shortens
//! first, then drops. Priority 0 never drops: context, taint, waiting
//! approvals and trust stay to the end. The desktop status line and phone
//! chips use the same order.

use crate::style::Line;

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

/// Drop order: higher numbers shorten and then drop before lower numbers
/// change. Priority 0 only shortens, after all optional segments have dropped.
#[must_use]
pub fn priority(name: &str) -> u8 {
    match name {
        "context" | "taint" | "approvals" | "trust" | "sandbox-off" => 0,
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

/// How a fitted item is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    Full,
    Short,
    Dropped,
}

/// Shortens and then drops items until `fits` accepts their forms. `items`
/// holds each item's priority and whether it has a short form. The item
/// with the highest priority number changes first, the earlier one on a tie.
/// Priority 0 never drops, and shortens only once every optional item is
/// gone. Returns each item's form and the dropped indices in drop order.
/// The status line and frame hints share this order.
pub(crate) fn shrink(
    items: &[(u8, bool)],
    fits: impl Fn(&[Form]) -> bool,
) -> (Vec<Form>, Vec<usize>) {
    let mut forms = vec![Form::Full; items.len()];
    let mut dropped = Vec::new();
    while !fits(&forms) {
        let optional = (0..items.len())
            .filter(|&i| forms[i] != Form::Dropped && items[i].0 > 0)
            .max_by_key(|&i| (items[i].0, std::cmp::Reverse(i)));
        if let Some(i) = optional {
            if items[i].1 && forms[i] == Form::Full {
                forms[i] = Form::Short;
            } else {
                forms[i] = Form::Dropped;
                dropped.push(i);
            }
            continue;
        }
        // Safety labels keep their full wording until only safety remains.
        match (0..items.len()).find(|&i| forms[i] == Form::Full && items[i].1) {
            Some(i) => forms[i] = Form::Short,
            None => break,
        }
    }
    (forms, dropped)
}

fn join(segments: &[Segment], forms: &[Form], side: Side, sep: &str) -> Line {
    let mut line = Line::default();
    let shown = segments
        .iter()
        .zip(forms)
        .filter(|(s, form)| s.side == side && **form != Form::Dropped);
    for (i, (segment, form)) in shown.enumerate() {
        if i > 0 {
            line = line.dim(sep);
        }
        let value = match (&segment.short, form) {
            (Some(short), Form::Short) => short,
            _ => &segment.value,
        };
        line = line.extend(value.clone());
    }
    line
}

/// Fits segments into `cols` cells. If even the shortest safety state cannot
/// fit, it is visibly truncated rather than replaced with an empty row.
#[must_use]
pub fn fit(segments: Vec<Segment>, cols: usize) -> Fitted {
    let items: Vec<_> = segments
        .iter()
        .map(|s| (priority(s.name), s.short.is_some()))
        .collect();
    let width = |forms: &[Form]| {
        let left = join(&segments, forms, Side::Left, "  ").width();
        let right = join(&segments, forms, Side::Right, " · ").width();
        2 + left + right + if left > 0 && right > 0 { 2 } else { 0 }
    };
    let (forms, dropped) = shrink(&items, |forms| width(forms) <= cols);
    let inner = cols.saturating_sub(2);
    let left = join(&segments, &forms, Side::Left, "  ");
    let right = join(&segments, &forms, Side::Right, " · ");
    let gap = inner.saturating_sub(left.width() + right.width());
    let row = left.ink(" ".repeat(gap)).extend(right).truncate(inner);
    Fitted {
        line: Line::default().ink(" ").extend(row).ink(" ").truncate(cols),
        dropped: dropped.into_iter().map(|i| segments[i].name).collect(),
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
    fn lower_priority_segments_drop_before_important_labels_shorten() {
        let fitted = fit(segments(), 60);
        assert_eq!(
            fitted.dropped,
            ["sandbox", "cache", "branch", "reasoning", "cost"]
        );
        let text = fitted.line.plain();
        assert!(text.starts_with(" ledger"), "{text}");
        assert!(text.contains("sonnet-5.5"), "{text}");
        assert!(text.contains("◆ tainted · ? 1 waiting"), "{text}");
        assert_eq!(fitted.line.width(), 60);
    }

    #[test]
    fn a_segment_shortens_before_it_drops() {
        let segments = || {
            vec![
                Segment::new("sandbox", Line::default().dim("◇ sandbox"))
                    .short(Line::default().dim("◇")),
                Segment::new("context", Line::default().dim("ctx 24%")),
            ]
        };
        let fitted = fit(segments(), 13);
        assert_eq!(fitted.line.plain(), " ◇ · ctx 24% ");
        assert!(fitted.dropped.is_empty());
        let fitted = fit(segments(), 12);
        assert_eq!(fitted.line.plain(), "    ctx 24% ");
        assert_eq!(fitted.dropped, ["sandbox"]);
    }

    #[test]
    fn an_exact_fit_does_not_reserve_a_gap_for_an_absent_left_side() {
        let fitted = fit(
            vec![Segment::new("context", Line::default().dim("ctx 24%"))],
            9,
        );
        assert_eq!(fitted.line.plain(), " ctx 24% ");
        assert!(fitted.dropped.is_empty());
    }

    #[test]
    fn safety_overflow_is_visible_and_every_width_is_bounded() {
        for cols in 0..=120 {
            let mut segs = segments();
            segs.push(Segment::new("trust", Line::default().brass("untrusted")));
            let fitted = fit(segs, cols);
            assert_eq!(fitted.line.width(), cols, "width {cols}");
            assert!(fitted.dropped.iter().all(|name| priority(name) > 0));
            let text = fitted.line.plain();
            if cols >= 3 {
                assert!(!text.trim().is_empty(), "width {cols}");
            }
            if cols >= 30 {
                for label in ["ctx 24%", "◆", "?", "untrusted"] {
                    assert!(text.contains(label), "width {cols}: {text}");
                }
            } else if cols >= 3 {
                assert!(text.contains('…'), "width {cols}: {text}");
            }
        }
    }

    #[test]
    fn unicode_labels_and_monochrome_output_preserve_safety_marks() {
        let fitted = fit(
            vec![
                Segment::new("workspace", Line::default().ink("~/src/帳簿")).left(),
                Segment::new("taint", Line::default().murex("◆ tainted")),
                Segment::new("approvals", Line::default().brass("? 1 waiting")),
            ],
            36,
        );
        let mut plain = String::new();
        fitted.line.write_ansi(&mut plain, false, None);
        assert_eq!(plain, fitted.line.plain());
        assert!(plain.contains("◆ tainted · ? 1 waiting"));
        assert_eq!(fitted.line.width(), 36);
        assert!(!plain.contains('\x1b'));
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
