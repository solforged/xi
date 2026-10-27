use std::path::{Path, PathBuf};
use std::time::Instant;

use xi_client::client::ClientNotification;
use xi_protocol::Timestamp;
use xi_protocol::methods::client::EventParams;
use xi_protocol::model::Reasoning;

use crate::screen::Screen;
use crate::status::{Segment, fit};
use crate::style::{Line, Role, lr};
use crate::transcript::{SPINNER, blocks_text, tokens};
use crate::{App, Focus, lineage};

/// The footer's form of a level.
fn short_reasoning(reasoning: Reasoning) -> &'static str {
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
                .truncate(self.cols),
        ];
        if !info.trusted {
            header.push(
                Line::default()
                    .ink("  ")
                    .brass("untrusted")
                    .dim(" · repo config held back · xi trust")
                    .truncate(self.cols),
            );
        }
        self.commit.extend(header);
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
        let events = lineage(&self.launch.attached.events, info.head);
        let replayed = events.len().saturating_sub(1);
        for event in events {
            let n = ClientNotification::Event(EventParams {
                session: info.session,
                event,
            });
            let lines = self.transcript.apply(&n, self.cols);
            self.commit.extend(lines);
        }
        if self.launch.attached.running {
            self.started = Some(Instant::now());
        } else {
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
                self.cols,
            );
        }
        if replayed > 0 {
            let note = self.transcript.note(vec![Line::default().dim(format!(
                "  resumed · {replayed} events · {}",
                home(&info.path)
            ))]);
            self.commit.extend(note);
        }
        if !self.transcript.asks().is_empty() {
            self.open_own_approvals();
        }
    }

    pub(crate) fn footer(&self, cols: usize) -> Line {
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
        // This is the worker's fail-closed execution policy, not a health
        // check: a sandbox setup failure prevents the tool from running.
        segs.push(
            Segment::new("sandbox", Line::default().dim("◇ sandbox"))
                .short(Line::default().dim("◇")),
        );
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
        let asks = self.transcript.asks().len();
        if asks > 0 {
            segs.push(
                Segment::new(
                    "approvals",
                    Line::default().brass(format!("? {asks} waiting · tab")),
                )
                .short(Line::default().brass(format!("?{asks}"))),
            );
        }
        fit(segs, cols).line
    }

    pub(crate) fn live(&self) -> (Vec<Line>, Option<(usize, usize)>) {
        let cols = self.cols;
        let mut lines = self.transcript.live(cols, self.frame);
        if self.focus == Focus::Approval
            && let Some(approvals) = &self.approvals
        {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.extend(approvals.render(cols, self.session_id()));
        }
        if self.focus == Focus::Tree
            && let Some(tree) = &self.tree
        {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let composer = self.editor.render(cols, Role::Dim, "").0.len();
            let rows = self
                .rows
                .saturating_sub(composer + 3 + usize::from(!self.images.is_empty()));
            lines.extend(tree.render(cols, rows, Timestamp::now()));
        }
        if self.running() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
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
            if turn.sent() > 0 {
                line = line.dim(format!(
                    " · ↑ {} ↓ {}",
                    tokens(turn.sent()),
                    tokens(turn.output)
                ));
            }
            lines.push(line.dim(" · esc stop").truncate(cols));
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
        let attachment_rows = usize::from(!self.images.is_empty());
        let overlay_rows = self
            .rows
            .saturating_sub(self.editor.render(cols, Role::Dim, "").0.len() + 3 + attachment_rows);
        if self.focus == Focus::Models
            && let Some(picker) = &self.picker
        {
            lines.extend(picker.render(cols, overlay_rows));
        }
        if self.focus == Focus::Sessions
            && let Some(sessions) = &self.sessions
        {
            lines.extend(sessions.render(cols, overlay_rows, Timestamp::now()));
        }
        let mut overlay_cursor = None;
        if self.focus == Focus::Settings
            && let Some(settings) = &self.settings
        {
            let info = &self.launch.attached.info;
            let (overlay, cursor) = settings.render(
                cols,
                overlay_rows.saturating_sub(lines.len()),
                &info.model,
                info.reasoning,
                self.transcript.thinking_display(),
                self.launch.tui_config.as_deref(),
            );
            overlay_cursor = cursor.map(|(row, col)| (lines.len() + row, col));
            lines.extend(overlay);
        }
        if self.focus == Focus::Search
            && let Some(search) = &self.search
        {
            lines.extend(
                search
                    .render(&self.history, cols)
                    .into_iter()
                    .take(overlay_rows),
            );
        }
        if self.focus == Focus::Composer
            && let Some(menu) = &self.completion
        {
            lines.extend(menu.render(cols, overlay_rows.min(8)));
        }
        let border = if self.focus != Focus::Composer {
            Role::Dim
        } else if self.transcript.tainted {
            Role::Murex
        } else {
            Role::Brass
        };
        let hint = if self.running() {
            "enter steers · alt+enter queues · esc stops"
        } else {
            "enter sends · ctrl+j new line · /help"
        };
        if !self.images.is_empty() {
            let count = self.images.len();
            let noun = if count == 1 { "image" } else { "images" };
            lines.push(
                Line::default()
                    .brass(format!("  {count} {noun} attached"))
                    .truncate(cols),
            );
        }
        let (composer, (row, col)) = self.editor.render(cols, border, hint);
        let top = lines.len();
        lines.extend(composer);
        lines.push(self.footer(cols));
        let cursor =
            overlay_cursor.or_else(|| (self.focus == Focus::Composer).then_some((top + row, col)));
        (lines, cursor)
    }

    pub(crate) fn draw<W: std::io::Write>(
        &mut self,
        screen: &mut Screen<W>,
    ) -> std::io::Result<()> {
        for control in self.controls.drain(..) {
            screen.control(&control)?;
        }
        let (live, cursor) = self.live();
        let commit = std::mem::take(&mut self.commit);
        screen.draw(&commit, &live, cursor, self.rows.saturating_sub(1))
    }
}
