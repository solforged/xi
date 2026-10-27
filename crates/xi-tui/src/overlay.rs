//! Overlays: everything that asks, lets you pick, or shows a view.
//!
//! An overlay owns its content and keys; [`crate::frame`] owns borders,
//! slots, scrolling and hints. Overlays form a stack, but only the top one
//! is drawn, titled with the levels under it as a breadcrumb such as
//! `settings › models`. `esc` steps back one level and `ctrl+c` closes the
//! whole stack.

use std::any::Any;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use xi_protocol::Timestamp;
use xi_protocol::methods::client::{ModelEntry, SessionDetails, SessionSummary};

use crate::approvals::{Approvals, Choice};
use crate::frame::{self, Body, Frame, Hint};
use crate::history::History;
use crate::keys::{Action, Keymap};
use crate::settings;
use crate::style::Line;
use crate::transcript::Thinking;
use crate::tree::Node;

/// What overlays may read while drawing or handling a key.
pub(crate) struct Ctx<'a> {
    pub keys: &'a Keymap,
    pub history: &'a History,
    pub info: &'a SessionDetails,
    pub thinking: Thinking,
    pub tui_path: Option<&'a Path>,
    pub now: Timestamp,
    /// Terminal rows, for paging.
    pub rows: usize,
    /// The name of the overlay under this one, which `esc` returns to.
    pub parent: Option<&'static str>,
}

/// What a key did.
pub(crate) enum Outcome {
    /// Redraw and keep the keys.
    Stay,
    /// Step back one level.
    Close,
    /// Something only the client's owner can do; the overlay stays until it
    /// has run.
    Send(Box<Command>),
}

/// Work an overlay hands to the client, which owns the connection.
pub(crate) enum Command {
    /// Answer the shown ask.
    Answer(Choice),
    /// Check out a tree node.
    Checkout(Node),
    /// Switch the session's model.
    SetModel(ModelEntry),
    /// Attach to or open another session.
    Resume(SessionSummary),
    /// List sessions of this workspace, or of all of them.
    Sessions { all: bool },
    /// A change from the settings view.
    Setting(settings::Action),
    /// Put a prompt from history in the composer.
    UsePrompt(String),
}

/// One overlay in the stack.
pub(crate) trait Overlay: Any {
    /// The overlay's name in a breadcrumb.
    fn name(&self) -> &'static str;
    /// The border, title, info and hints.
    fn frame(&self, ctx: &Ctx) -> Frame;
    /// The content, `inner` cells wide.
    fn body(&self, inner: usize, ctx: &Ctx) -> Body;
    /// Handles a key; `action` is its keymap action.
    fn key(&mut self, key: KeyEvent, action: Option<Action>, ctx: &Ctx) -> Outcome;
}

/// What a key does in a list with a filter.
pub(crate) enum ListKey {
    Choose,
    Close,
    Tab,
    Up,
    Down,
    Older,
    Backspace,
    Type(char),
    Other,
}

/// Plain characters type into the filter; everything else goes through the
/// keymap, so the frame's hints and the keys agree.
pub(crate) fn list_key(key: KeyEvent, action: Option<Action>) -> ListKey {
    if let KeyCode::Char(c) = key.code
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return ListKey::Type(c);
    }
    match action {
        Some(Action::Send) => ListKey::Choose,
        Some(Action::Escape) => ListKey::Close,
        Some(Action::Complete) => ListKey::Tab,
        Some(Action::Up) => ListKey::Up,
        Some(Action::Down) => ListKey::Down,
        Some(Action::Search) => ListKey::Older,
        Some(Action::Backspace) => ListKey::Backspace,
        _ => ListKey::Other,
    }
}

/// The open overlays, bottom first. Only the top one is drawn and gets
/// keys.
#[derive(Default)]
pub(crate) struct Overlays(Vec<Box<dyn Overlay>>);

impl Overlays {
    /// Whether nothing is open, so the composer has the keys.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Opens an overlay on top.
    pub fn push(&mut self, overlay: Box<dyn Overlay>) {
        self.0.push(overlay);
    }

    /// Steps back one level.
    pub fn pop(&mut self) {
        self.0.pop();
    }

    /// Closes every overlay.
    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// The name of the overlay under the top one.
    fn parent(&self) -> Option<&'static str> {
        self.0.len().checked_sub(2).map(|i| self.0[i].name())
    }

    /// Whether the top overlay is a `T`.
    pub fn on_top<T: Overlay>(&self) -> bool {
        self.0
            .last()
            .is_some_and(|overlay| (&**overlay as &dyn Any).is::<T>())
    }

    /// The topmost `T` in the stack.
    pub fn get<T: Overlay>(&self) -> Option<&T> {
        self.0
            .iter()
            .rev()
            .find_map(|overlay| (&**overlay as &dyn Any).downcast_ref::<T>())
    }

    /// The topmost `T` in the stack, mutably.
    pub fn get_mut<T: Overlay>(&mut self) -> Option<&mut T> {
        self.0
            .iter_mut()
            .rev()
            .find_map(|overlay| (&mut **overlay as &mut dyn Any).downcast_mut::<T>())
    }

    /// Closes every `T` and what was opened on top of it.
    pub fn close<T: Overlay>(&mut self) {
        if let Some(at) = self
            .0
            .iter()
            .position(|overlay| (&**overlay as &dyn Any).is::<T>())
        {
            self.0.truncate(at);
        }
    }

    /// Passes a key to the top overlay.
    pub fn key(&mut self, key: KeyEvent, action: Option<Action>, ctx: &mut Ctx) -> Outcome {
        ctx.parent = self.parent();
        match self.0.last_mut() {
            Some(top) => top.key(key, action, ctx),
            None => Outcome::Stay,
        }
    }

    /// Draws the top overlay `cols` wide and at most `rows` tall, titled
    /// with its breadcrumb. While asks wait under another overlay, `tab`
    /// answers them, so that overlay's own `tab` hint gives way.
    pub fn draw(
        &self,
        ctx: &mut Ctx,
        waiting: usize,
        cols: usize,
        rows: usize,
    ) -> (Vec<Line>, Option<(usize, usize)>) {
        let Some(top) = self.0.last() else {
            return (Vec::new(), None);
        };
        ctx.parent = self.parent();
        let mut framed = top.frame(ctx);
        let mut title = Line::default();
        for overlay in &self.0[..self.0.len() - 1] {
            title = title.dim(format!("{} › ", overlay.name()));
        }
        framed.title = title.extend(framed.title);
        if waiting > 0
            && !self.on_top::<Approvals>()
            && let Some(tab) = ctx.keys.key(Action::Complete)
        {
            let prefix = format!("{tab} ");
            framed.hints.retain(|hint| !hint.text.starts_with(&prefix));
            framed.hints.push(
                Hint::new(format!("{tab} answers {waiting} waiting"), 1)
                    .short(format!("{tab} ?{waiting}")),
            );
        }
        let body = top.body(frame::inner(cols), ctx);
        frame::draw(&framed, body, cols, rows)
    }
}
