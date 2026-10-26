//! Composer actions and their effective key bindings.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// An action available while the composer has focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Send a prompt or accept a completion.
    Send,
    /// Queue a follow-up prompt.
    Queue,
    /// Insert a line break.
    Newline,
    /// Close an overlay or stop the turn.
    Escape,
    /// Close, stop, clear or quit.
    Interrupt,
    /// Delete forward or quit when empty and idle.
    DeleteOrQuit,
    /// Select the previous completion or prompt.
    Up,
    /// Select the next completion or prompt.
    Down,
    /// Complete or open approvals.
    Complete,
    /// Search prompt history.
    Search,
    /// Edit the prompt externally.
    External,
    /// Open the model picker.
    Models,
    /// Cycle reasoning depth.
    Reasoning,
    /// Expand a collapsed result.
    Expand,
    /// Cycle the thinking display.
    Thinking,
    /// Move to the line start.
    Home,
    /// Move to the line end.
    End,
    /// Move one character left.
    Left,
    /// Move one character right.
    Right,
    /// Delete to the line start.
    KillBefore,
    /// Delete to the line end.
    KillAfter,
    /// Delete the preceding word.
    KillWord,
    /// Delete the preceding character.
    Backspace,
    /// Delete the next character.
    Delete,
}

impl Action {
    /// Actions in the order shown by help.
    pub const ALL: &[Action] = &[
        Self::Send,
        Self::Queue,
        Self::Newline,
        Self::Escape,
        Self::Interrupt,
        Self::DeleteOrQuit,
        Self::Up,
        Self::Down,
        Self::Complete,
        Self::Search,
        Self::External,
        Self::Models,
        Self::Reasoning,
        Self::Expand,
        Self::Thinking,
        Self::Home,
        Self::End,
        Self::Left,
        Self::Right,
        Self::KillBefore,
        Self::KillAfter,
        Self::KillWord,
        Self::Backspace,
        Self::Delete,
    ];

    /// The action name used in `tui.toml`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Queue => "queue",
            Self::Newline => "newline",
            Self::Escape => "escape",
            Self::Interrupt => "interrupt",
            Self::DeleteOrQuit => "delete_or_quit",
            Self::Up => "up",
            Self::Down => "down",
            Self::Complete => "complete",
            Self::Search => "search",
            Self::External => "external",
            Self::Models => "models",
            Self::Reasoning => "reasoning",
            Self::Expand => "expand",
            Self::Thinking => "thinking",
            Self::Home => "home",
            Self::End => "end",
            Self::Left => "left",
            Self::Right => "right",
            Self::KillBefore => "kill_before",
            Self::KillAfter => "kill_after",
            Self::KillWord => "kill_word",
            Self::Backspace => "backspace",
            Self::Delete => "delete",
        }
    }

    /// The description shown in help.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::Send => "send; during a turn, steer; accept a completion",
            Self::Queue => "queue a follow-up for after the turn",
            Self::Newline => "new line",
            Self::Escape => "close a menu or overlay; otherwise stop the turn",
            Self::Interrupt => "close overlay, stop turn, clear composer, or quit",
            Self::DeleteOrQuit => "quit when empty and idle; otherwise delete forward",
            Self::Up => "previous completion or workspace prompt",
            Self::Down => "next completion or workspace prompt",
            Self::Complete => "accept a completion; otherwise answer a waiting approval",
            Self::Search => "search prompt history; repeat for older matches",
            Self::External => "edit the prompt in the external editor",
            Self::Models => "open the model picker",
            Self::Reasoning => "cycle reasoning: off, low, medium, high",
            Self::Expand => "expand the newest collapsed result; repeat for older",
            Self::Thinking => "cycle thinking display: full, first line, hidden",
            Self::Home => "line start",
            Self::End => "line end",
            Self::Left => "previous character",
            Self::Right => "next character",
            Self::KillBefore => "delete to line start",
            Self::KillAfter => "delete to line end",
            Self::KillWord => "delete previous word",
            Self::Backspace => "delete previous character",
            Self::Delete => "delete next character",
        }
    }
}

/// One key and its modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    /// The terminal key code.
    pub code: KeyCode,
    /// The required modifiers.
    pub modifiers: KeyModifiers,
}

impl Binding {
    /// Parses `[ctrl+][alt+][shift+]KEY`, with case-insensitive modifiers.
    ///
    /// Keys are a character, enter, tab, esc, backspace, delete, up, down,
    /// left, right, home, end, pageup, pagedown, space, or f1 through f12.
    /// `shift+tab` is a modifier-free `BackTab`. Uppercase letters do not
    /// imply shift; use `shift+a` for that binding.
    ///
    /// # Errors
    ///
    /// Returns a description of an unknown key or modifier.
    pub fn parse(s: &str) -> Result<Binding, String> {
        let mut rest = s;
        let mut modifiers = KeyModifiers::NONE;
        while let Some((prefix, key)) = rest.split_once('+') {
            let modifier = match prefix.to_ascii_lowercase().as_str() {
                "ctrl" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => break,
            };
            if modifiers.contains(modifier) {
                return Err(format!("duplicate modifier in {s:?}"));
            }
            modifiers.insert(modifier);
            rest = key;
        }
        let lower = rest.to_ascii_lowercase();
        let code = match lower.as_str() {
            "enter" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "esc" => KeyCode::Esc,
            "backspace" => KeyCode::Backspace,
            "delete" => KeyCode::Delete,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "space" => KeyCode::Char(' '),
            _ => {
                let mut chars = rest.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if !c.is_control() && !c.is_whitespace() => KeyCode::Char(c),
                    _ => match lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                        Some(n @ 1..=12) => KeyCode::F(n),
                        _ => return Err(format!("invalid key binding {s:?}")),
                    },
                }
            }
        };
        let code = if code == KeyCode::Tab && modifiers.contains(KeyModifiers::SHIFT) {
            modifiers.remove(KeyModifiers::SHIFT);
            KeyCode::BackTab
        } else {
            code
        };
        Ok(Self { code, modifiers })
    }

    /// The canonical string accepted by [`Binding::parse`] and shown in help.
    #[must_use]
    pub fn label(&self) -> String {
        let mut label = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            label.push_str("ctrl+");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            label.push_str("alt+");
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) || self.code == KeyCode::BackTab {
            label.push_str("shift+");
        }
        match self.code {
            KeyCode::Enter => label.push_str("enter"),
            KeyCode::Tab | KeyCode::BackTab => label.push_str("tab"),
            KeyCode::Esc => label.push_str("esc"),
            KeyCode::Backspace => label.push_str("backspace"),
            KeyCode::Delete => label.push_str("delete"),
            KeyCode::Up => label.push_str("up"),
            KeyCode::Down => label.push_str("down"),
            KeyCode::Left => label.push_str("left"),
            KeyCode::Right => label.push_str("right"),
            KeyCode::Home => label.push_str("home"),
            KeyCode::End => label.push_str("end"),
            KeyCode::PageUp => label.push_str("pageup"),
            KeyCode::PageDown => label.push_str("pagedown"),
            KeyCode::Char(' ') => label.push_str("space"),
            KeyCode::Char(c) => label.push(c),
            KeyCode::F(n) => label.push_str(&format!("f{n}")),
            _ => label.push_str("unknown"),
        }
        label
    }

    fn normalized(mut self) -> Self {
        if self.code == KeyCode::BackTab {
            self.modifiers.remove(KeyModifiers::SHIFT);
        }
        self
    }
}

/// The ordered, effective composer key bindings.
#[derive(Clone, Debug)]
pub struct Keymap {
    bindings: Vec<(Binding, Action)>,
}

impl Default for Keymap {
    fn default() -> Self {
        use KeyCode::{
            BackTab, Backspace, Char, Delete, Down, End, Enter, Esc, Home, Left, Right, Tab, Up,
        };
        use KeyModifiers as M;
        let defaults = [
            (Enter, M::NONE, Action::Send),
            (Enter, M::ALT, Action::Queue),
            (Enter, M::SHIFT, Action::Newline),
            (Char('j'), M::CONTROL, Action::Newline),
            (Esc, M::NONE, Action::Escape),
            (Char('c'), M::CONTROL, Action::Interrupt),
            (Char('d'), M::CONTROL, Action::DeleteOrQuit),
            (Up, M::NONE, Action::Up),
            (Down, M::NONE, Action::Down),
            (Tab, M::NONE, Action::Complete),
            (Char('r'), M::CONTROL, Action::Search),
            (Char('g'), M::CONTROL, Action::External),
            (Char('m'), M::ALT, Action::Models),
            (BackTab, M::NONE, Action::Reasoning),
            (Char('o'), M::CONTROL, Action::Expand),
            (Char('t'), M::CONTROL, Action::Thinking),
            (Char('a'), M::CONTROL, Action::Home),
            (Char('e'), M::CONTROL, Action::End),
            (Char('b'), M::CONTROL, Action::Left),
            (Char('f'), M::CONTROL, Action::Right),
            (Left, M::NONE, Action::Left),
            (Right, M::NONE, Action::Right),
            (Home, M::NONE, Action::Home),
            (End, M::NONE, Action::End),
            (Char('u'), M::CONTROL, Action::KillBefore),
            (Char('k'), M::CONTROL, Action::KillAfter),
            (Char('w'), M::CONTROL, Action::KillWord),
            (Backspace, M::ALT, Action::KillWord),
            (Backspace, M::NONE, Action::Backspace),
            (Delete, M::NONE, Action::Delete),
        ];
        Self {
            bindings: defaults
                .into_iter()
                .map(|(code, modifiers, action)| (Binding { code, modifiers }, action))
                .collect(),
        }
    }
}

impl Keymap {
    /// The action for a key, normalizing crossterm's `BackTab` plus shift.
    #[must_use]
    pub fn action(&self, key: KeyEvent) -> Option<Action> {
        let binding = Binding {
            code: key.code,
            modifiers: key.modifiers,
        }
        .normalized();
        self.bindings
            .iter()
            .find(|(b, _)| *b == binding)
            .map(|(_, action)| *action)
    }

    /// Effective bindings of an action, in order.
    #[must_use]
    pub fn bindings(&self, action: Action) -> Vec<Binding> {
        self.bindings
            .iter()
            .filter_map(|(binding, a)| (*a == action).then_some(*binding))
            .collect()
    }

    /// Replaces an action's bindings, taking shared bindings from other actions.
    pub fn bind(&mut self, action: Action, bindings: Vec<Binding>) {
        self.bindings.retain(|(_, a)| *a != action);
        for binding in bindings {
            let binding = binding.normalized();
            self.bindings.retain(|(b, _)| *b != binding);
            self.bindings.push((binding, action));
        }
    }

    /// Help rows in [`Action::ALL`] order, skipping actions without bindings.
    #[must_use]
    pub fn help(&self) -> Vec<(String, &'static str)> {
        Action::ALL
            .iter()
            .filter_map(|action| {
                let bindings = self.bindings(*action);
                (!bindings.is_empty()).then(|| {
                    (
                        bindings
                            .iter()
                            .map(Binding::label)
                            .collect::<Vec<_>>()
                            .join(", "),
                        action.description(),
                    )
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_label_round_trip() {
        for label in [
            "ctrl+r",
            "alt+enter",
            "shift+tab",
            "ctrl+alt+shift+a",
            "A",
            "shift+a",
            "enter",
            "tab",
            "esc",
            "backspace",
            "delete",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "space",
            "f1",
            "f12",
            "+",
            "ctrl++",
            "λ",
        ] {
            let binding = Binding::parse(label).unwrap();
            assert_eq!(binding.label(), label);
            assert_eq!(Binding::parse(&binding.label()).unwrap(), binding);
        }
        assert_eq!(
            Binding::parse("CTRL+Alt+ENTER").unwrap().label(),
            "ctrl+alt+enter"
        );
        assert_eq!(Binding::parse("A").unwrap().modifiers, KeyModifiers::NONE);
        assert_eq!(
            Binding::parse("shift+tab").unwrap(),
            Binding {
                code: KeyCode::BackTab,
                modifiers: KeyModifiers::NONE,
            }
        );
    }

    #[test]
    fn rejects_invalid_keys_and_modifiers() {
        for bad in [
            "",
            "ctrl+",
            "ctrl+ctrl+a",
            "meta+x",
            "two",
            "f0",
            "f13",
            "ctrl+unknown",
            "\n",
        ] {
            assert!(Binding::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn defaults_dispatch_and_normalize_backtab() {
        let map = Keymap::default();
        for action in Action::ALL {
            for binding in map.bindings(*action) {
                assert_eq!(
                    map.action(KeyEvent::new(binding.code, binding.modifiers)),
                    Some(*action)
                );
            }
        }
        for modifiers in [KeyModifiers::NONE, KeyModifiers::SHIFT] {
            assert_eq!(
                map.action(KeyEvent::new(KeyCode::BackTab, modifiers)),
                Some(Action::Reasoning)
            );
        }
        assert_eq!(
            map.action(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(Action::Thinking)
        );
        assert_eq!(
            map.action(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn bind_replaces_and_steals_shared_keys() {
        let mut map = Keymap::default();
        let search = Binding::parse("ctrl+r").unwrap();
        map.bind(Action::Thinking, vec![search]);
        assert!(map.bindings(Action::Search).is_empty());
        assert_eq!(map.bindings(Action::Thinking), [search]);
        assert_eq!(
            map.action(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            map.action(KeyEvent::new(search.code, search.modifiers)),
            Some(Action::Thinking)
        );
        map.bind(Action::Thinking, Vec::new());
        assert!(map.bindings(Action::Thinking).is_empty());
    }

    #[test]
    fn help_follows_action_order_and_joins_bindings() {
        let mut map = Keymap::default();
        let rows = map.help();
        assert_eq!(rows.len(), Action::ALL.len());
        for (row, action) in rows.iter().zip(Action::ALL) {
            assert_eq!(row.1, action.description());
        }
        assert_eq!(rows[2].0, "shift+enter, ctrl+j");
        assert_eq!(rows[15].0, "ctrl+a, home");
        map.bind(Action::Send, Vec::new());
        assert_eq!(map.help()[0].0, "alt+enter");
    }
}
