use std::path::{Path, PathBuf};
use std::time::Instant;

use xi_client::client::ClientNotification;
use xi_protocol::methods::client::EventParams;
use xi_protocol::model::Reasoning;

use crate::editor::{Form, Shape};
use crate::layout::Tier;
use crate::screen::Screen;
use crate::status::{Segment, fit};
use crate::style::{Line, Role, lr};
use crate::transcript::{SPINNER, blocks_text, tokens};
use crate::{App, lineage};

/// The footer's form of a level.
pub(crate) fn short_reasoning(reasoning: Reasoning) -> &'static str {
    match reasoning {
        Reasoning::Minimal => "min",
        Reasoning::Medium => "med",
        level => level.name(),
    }
}

pub(crate) fn home(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

impl App {
    /// Prints the header and replays the head's branch, for resumed sessions.
    pub(crate) fn attach(&mut self) {
        self.title = crate::latest_title(&self.launch.attached.events);
        self.prompt = crate::first_prompt(&self.launch.attached.events);
        self.title_written = false;
        let cols = self.measure().out;
        let info = &self.launch.attached.info;
        let mut header = vec![
            Line::default()
                .brass("ξ ")
                .strong(Role::Ink, "xi")
                .dim(format!(
                    " {} · {}",
                    env!("CARGO_PKG_VERSION"),
                    home(&info.cwd)
                ))
                .truncate(cols),
        ];
        if !info.trusted {
            header.push(
                Line::default()
                    .ink("  ")
                    .brass("untrusted")
                    .dim(" · repo config held back · xi trust")
                    .truncate(cols),
            );
        }
        let session = info.session;
        let head = info.head;
        // The deck's own header names the workspace, and its panel the trust.
        if self.deck.is_none() {
            self.commit_lines(header);
        }
        self.seen.clear();
        self.launch
            .attached
            .events
            .retain(|event| self.seen.insert(event.id));
        self.resolved = self
            .launch
            .attached
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                xi_protocol::EventKind::ApprovalResolved { call, .. }
                | xi_protocol::EventKind::ToolResult { call, .. } => Some(call.clone()),
                _ => None,
            })
            .collect();
        let events = lineage(&self.launch.attached.events, head);
        let replayed = events.len().saturating_sub(1);
        for event in events {
            let n = ClientNotification::Event(EventParams { session, event });
            let lines = self.transcript.apply(&n, self.measure());
            self.commit_lines(lines);
        }
        if self.launch.attached.running {
            self.started = Some(Instant::now());
        } else {
            let lines = self.transcript.flush(self.measure());
            self.commit_lines(lines);
            self.transcript.end_turn();
        }
        self.transcript.partial(&self.launch.attached.partial);
        let parent = self.launch.attached.info.session;
        for child in &self.launch.attached.children {
            for event in &child.events {
                self.transcript.child(
                    parent,
                    &EventParams {
                        session: child.session,
                        event: event.clone(),
                    },
                );
            }
        }
        for pending in &self.launch.attached.pending_approvals {
            self.transcript.apply(
                &ClientNotification::ApprovalPending(pending.clone()),
                self.measure(),
            );
        }
        if replayed > 0 {
            let note = self.transcript.note(vec![Line::default().dim(format!(
                "  resumed · {replayed} events · {}",
                home(&self.launch.attached.info.path)
            ))]);
            self.commit_lines(note);
        }
        if !self.transcript.asks().is_empty() {
            self.open_own_approvals();
        }
    }

    pub(crate) fn footer(&self, cols: usize) -> Line {
        fit(self.segments(), cols).line
    }

    /// The footer's segments, left ones first.
    pub(crate) fn segments(&self) -> Vec<Segment> {
        let d = |s: String| Line::default().dim(s);
        let info = &self.launch.attached.info;
        let workspace = home(&info.cwd);
        let base = info
            .cwd
            .file_name()
            .map_or_else(|| workspace.clone(), |n| n.to_string_lossy().into_owned());
        let mut segs = vec![
            Segment::new("workspace", Line::default().ink(workspace))
                .short(Line::default().ink(base))
                .left(),
        ];
        if let Some(branch) = self.git.label(&info.cwd) {
            segs.push(Segment::new("branch", Line::default().dim(branch)).left());
        }
        let model = &info.model;
        segs.push(
            Segment::new(
                "model",
                Line::default().ink(format!("{}/{}", model.provider, model.model)),
            )
            .short(Line::default().ink(&model.model)),
        );
        let reasoning = match &info.model_info {
            Some(model) if model.reasoning.is_empty() => Line::default().dim("no think"),
            Some(model) => Line::default()
                .dim("think ")
                .ink(short_reasoning(model.resolve(info.reasoning))),
            None => Line::default()
                .dim("think ")
                .ink(short_reasoning(info.reasoning)),
        };
        segs.push(Segment::new("reasoning", reasoning));
        let context = info
            .model_info
            .as_ref()
            .filter(|model| model.context_window > 0)
            .map_or_else(
                || "–".into(),
                |model| {
                    format!(
                        "{}%",
                        u128::from(self.transcript.context) * 100
                            / u128::from(model.context_window)
                    )
                },
            );
        segs.push(Segment::new(
            "context",
            Line::default().dim("ctx ").ink(context),
        ));
        let session = self.transcript.session;
        let cache = if session.sent() > 0 {
            format!(
                "{}%",
                u128::from(session.cache_read) * 100 / u128::from(session.sent())
            )
        } else {
            "–".into()
        };
        segs.push(Segment::new(
            "cache",
            Line::default().dim("cache ").ink(cache),
        ));
        if let Some(cost) = session.cost(info.model_info.as_ref().and_then(|m| m.pricing)) {
            segs.push(Segment::new(
                "cost",
                Line::default().ink(format!("${cost:.2}")),
            ));
        }
        // Show the worker's actual launch policy, not the current default.
        segs.push(if info.sandbox_enabled {
            Segment::new("sandbox", Line::default().dim("◇ sandbox"))
                .short(Line::default().dim("◇"))
        } else {
            Segment::new("sandbox-off", Line::default().brass("sandbox off"))
                .short(Line::default().brass("sbx off"))
        });
        let running = self.transcript.children().running();
        if running > 0 {
            segs.push(
                Segment::new(
                    "children",
                    d("agents ".into()).ink(format!("{running} running")),
                )
                .short(d("ag ".into()).ink(running.to_string())),
            );
        }
        if !info.trusted {
            segs.push(Segment::new("trust", Line::default().brass("untrusted")));
        }
        if self.transcript.tainted {
            segs.push(
                Segment::new("taint", Line::default().murex("◆ tainted"))
                    .short(Line::default().murex("◆")),
            );
        }
        let asks = self.waiting();
        if asks > 0 {
            segs.push(
                Segment::new(
                    "approvals",
                    Line::default().brass(format!("? {asks} waiting · tab")),
                )
                .short(Line::default().brass(format!("?{asks}"))),
            );
        }
        segs
    }

    /// The live region, its cursor, and how many of its last rows the deck
    /// keeps against the bottom: frames, the composer and its status row.
    pub(crate) fn live(&self) -> (Vec<Line>, Option<(usize, usize)>, usize) {
        let m = self.measure();
        let cols = m.out;
        let deck = self.deck.is_some();
        // Frames keep their room by shrinking streamed rows and the composer.
        // Short panes omit the collapsed composer so title and hints survive.
        let framed = !self.overlays.is_empty();
        let mut lines = if framed {
            self.transcript.live_under_frame(m, self.frame)
        } else {
            self.transcript.live(m, self.frame)
        };
        let transcript_rows = lines.len();
        // The deck shows the turn's activity under its composer, which a
        // frame collapses.
        let activity = self.running().then(|| self.activity(m));
        if let Some(activity) = activity.clone().filter(|_| !deck || framed) {
            if !lines.is_empty() && !framed {
                lines.push(Line::default());
            }
            lines.push(activity.dim(" · esc stop").truncate(cols));
        }
        for content in &self.steering {
            lines.push(lr(
                Line::default()
                    .ink("  ")
                    .brass("» steer ")
                    .ink(blocks_text(content).replace('\n', " ")),
                Line::default().dim("at the next request"),
                cols,
            ));
        }
        for content in &self.queued {
            lines.push(lr(
                Line::default()
                    .ink("  ")
                    .dim("» queued ")
                    .ink(blocks_text(content).replace('\n', " ")),
                Line::default().dim("after this turn"),
                cols,
            ));
        }
        let hint = if self.running() {
            "enter steers · alt+enter queues · esc stops"
        } else {
            "enter sends · ctrl+j new line · /help"
        };
        let border = if self.transcript.tainted {
            Role::Murex
        } else {
            Role::Brass
        };
        let (composer, (row, col)) = if framed && m.short {
            (Vec::new(), (0, 0))
        } else if framed {
            let draft = self.editor.text().lines().next().unwrap_or_default();
            let back = if self.overlays.len() == 1 {
                "esc returns here"
            } else {
                "ctrl+c returns here"
            };
            let line = lr(
                Line::default().dim(format!("ξ {draft}")),
                Line::default().dim(back),
                cols,
            );
            (vec![line], (0, 0))
        } else if deck {
            let form = Form {
                rows: m.composer_rows(),
                shape: Shape::Bar,
            };
            let (mut block, cursor) = self.editor.render(cols, border, "", form);
            let bar = || Line::default().span(border, false, "┃");
            block.push(bar().ink("  ").extend(self.model_line()).raise());
            block.push(bar().raise());
            (block, cursor)
        } else {
            let form = Form {
                rows: m.composer_rows(),
                shape: if m.tier == Tier::Slit {
                    Shape::Rule
                } else {
                    Shape::Box
                },
            };
            self.editor.render(cols, border, hint, form)
        };
        let attachment_rows = usize::from(!self.images.is_empty());
        // The frame gets what is left after the composer, the status line
        // and the turn's own rows; streamed rows above it scroll off first.
        let reserved = composer.len() + 1 + attachment_rows + lines.len() - transcript_rows;
        let overlay_rows = self.rows.saturating_sub(1 + reserved);
        let fixed = composer.len() + 1 + attachment_rows;
        let frame_rows = if m.short {
            m.frame_rows(overlay_rows.max(4))
                .min(m.live_rows().saturating_sub(fixed))
        } else {
            m.frame_rows(overlay_rows.max(4))
        };
        let dock_from = lines.len();
        let mut ctx = crate::overlays::ctx!(self);
        let (rows, cursor) = self
            .overlays
            .draw(&mut ctx, self.waiting(), cols, frame_rows);
        let overlay_cursor = cursor.map(|(row, col)| (lines.len() + row, col));
        lines.extend(rows);
        if !framed && let Some(menu) = &self.completion {
            let menu_rows = overlay_rows.min(8).min(frame_rows);
            lines.extend(menu.render(cols, menu_rows));
        }
        if !self.images.is_empty() {
            let count = self.images.len();
            let noun = if count == 1 { "image" } else { "images" };
            lines.push(
                Line::default()
                    .brass(format!("  {count} {noun} attached"))
                    .truncate(cols),
            );
        }
        let top = lines.len();
        lines.extend(composer);
        if !deck {
            lines.push(self.footer(cols));
        } else if !framed {
            lines.push(lr(
                activity.unwrap_or_default(),
                Line::default().dim(hint),
                cols,
            ));
        }
        let cursor = overlay_cursor.or_else(|| (!framed).then_some((top + row, col)));
        let hidden = lines.len().saturating_sub(m.live_rows());
        if hidden > 0 {
            lines.drain(..hidden);
        }
        let cursor = cursor.and_then(|(row, col)| row.checked_sub(hidden).map(|row| (row, col)));
        let dock = lines.len().min(lines.len() + hidden - dock_from);
        (lines, cursor, dock)
    }

    /// The turn's spinner, what it does and for how long, and its tokens
    /// when the measure has room.
    fn activity(&self, m: crate::layout::Measure) -> Line {
        let elapsed = self.started.map_or(0, |s| s.elapsed().as_secs());
        let turn = self.transcript.turn;
        let what = if self.transcript.thinking() {
            "thinking"
        } else {
            "working"
        };
        let mut line = Line::default()
            .brass(SPINNER[self.frame % SPINNER.len()])
            .dim(format!(" {what} · {elapsed}s"));
        if m.durations() && turn.sent() > 0 {
            line = line.dim(format!(
                " · ↑ {} ↓ {}",
                tokens(turn.sent()),
                tokens(turn.output)
            ));
        }
        line
    }

    /// The model and its reasoning level, under the deck's composer.
    fn model_line(&self) -> Line {
        let info = &self.launch.attached.info;
        let model = Line::default().ink(format!("{}/{}", info.model.provider, info.model.model));
        match &info.model_info {
            Some(model_info) if model_info.reasoning.is_empty() => model.dim(" · no think"),
            model_info => {
                let level = model_info
                    .as_ref()
                    .map_or(info.reasoning, |m| m.resolve(info.reasoning));
                model.dim(" · ").dim(short_reasoning(level))
            }
        }
    }

    pub(crate) fn draw<W: std::io::Write>(
        &mut self,
        screen: &mut Screen<W>,
    ) -> std::io::Result<()> {
        if std::mem::take(&mut self.clear_scrollback) {
            screen.clear();
        }
        for control in self.controls.drain(..) {
            screen.control(&control)?;
        }
        if !self.title_written {
            screen.control(&crate::title_control(self.title.as_deref()))?;
            self.title_written = true;
        }
        if self.deck.is_some() {
            return self.draw_deck(screen);
        }
        let (live, cursor, _) = self.live();
        let commit = std::mem::take(&mut self.commit);
        screen.draw(&commit, &live, cursor, self.measure().live_rows())
    }
}
