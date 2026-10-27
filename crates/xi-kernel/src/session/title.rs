//! Session titles: automatic naming with the `tiny` model, and renames.
//!
//! Two automatic passes name a session whose client asked for titles. The
//! first runs beside the turn of the session's first eligible prompt and sees
//! only that prompt. The second runs once that prompt's first completed turn
//! ends, and also sees the current title and the turn's final reply. A tainted
//! head skips it, so fetched content never names a session. Results append at
//! the next model boundary, or at once when the session is idle. A user title
//! settles naming for good.

use tokio_util::sync::CancellationToken;
use xi_protocol::event::{ModelRef, StopReason, TitleSource, Usage};
use xi_protocol::model::{ModelRole, Reasoning};
use xi_protocol::{Block, Event, EventKind};
use xi_provider::{Message, Request};

use crate::config::Config;
use crate::{Error, Result, Session, State, TurnOutcome};

/// The longest title, in characters.
pub const MAX_CHARS: usize = 80;
/// The longest generated title, in characters.
const AUTO_CHARS: usize = 60;
/// The longest prompt or reply excerpt sent to the model, in characters.
const EXCERPT_CHARS: usize = 2000;
/// Output budget, leaving room for a reasoning model's lowest level.
const MAX_TOKENS: u32 = 1024;

/// A session's naming state.
#[derive(Debug, Default)]
pub(crate) struct Titles {
    current: Option<String>,
    /// A user title exists; automatic titles never replace it.
    user: bool,
    /// The prompt the automatic passes name, once chosen.
    anchor: Option<String>,
    /// No further automatic pass runs.
    settled: bool,
    /// Bumped per automatic request; results from older requests are dropped.
    generation: u64,
    cancel: Option<CancellationToken>,
}

impl Titles {
    /// Restores naming state from a log. Any earlier title settles naming,
    /// so reopening a session never repeats a pass.
    pub(crate) fn from_events<'a>(events: impl IntoIterator<Item = &'a Event>) -> Self {
        let mut titles = Self::default();
        for event in events {
            if let EventKind::SessionTitled { title, by, .. } = &event.kind {
                titles.current = Some(title.clone());
                titles.user |= *by == TitleSource::User;
                titles.settled = true;
            }
        }
        titles
    }
}

/// A title waiting for the session log.
#[derive(Debug)]
pub(crate) struct Update {
    title: String,
    by: TitleSource,
    model: Option<ModelRef>,
    usage: Option<Usage>,
    generation: u64,
}

/// Validates and trims a user-assigned session title.
pub(crate) fn validate(title: &str) -> Result<&str> {
    let title = title.trim();
    if title.is_empty() || title.contains(['\n', '\r']) || title.chars().count() > MAX_CHARS {
        return Err(Error::Config(format!(
            "a title is one line of 1 to {MAX_CHARS} characters"
        )));
    }
    Ok(title)
}

impl Session {
    /// Sets the user's title: at once when idle, otherwise at the running
    /// turn's next model boundary. Automatic naming stops for good.
    ///
    /// # Errors
    /// Returns `Config` unless the trimmed title is one line of 1 to 80
    /// characters, and log errors when appending it.
    pub async fn rename(&self, title: &str) -> Result<()> {
        let title = validate(title)?;
        self.deliver(Update {
            title: title.into(),
            by: TitleSource::User,
            model: None,
            usage: None,
            generation: 0,
        })
        .await
    }

    async fn deliver(&self, update: Update) -> Result<()> {
        let (reply, rx) = tokio::sync::oneshot::channel();
        self.idle_operation(super::actor::IdleCommand::Title(update, reply), rx)
            .await
    }

    /// Appends titles that arrived while a turn held the log.
    pub(crate) async fn apply_titles(&self, state: &mut State) -> Result<()> {
        let updates = self.title_updates().await;
        updates
            .into_iter()
            .try_for_each(|update| self.apply_title(state, update))
    }

    pub(crate) fn apply_title(&self, state: &mut State, update: Update) -> Result<()> {
        let titles = &mut state.titles;
        if update.by == TitleSource::User {
            titles.user = true;
            titles.settled = true;
            if let Some(cancel) = titles.cancel.take() {
                cancel.cancel();
            }
        } else if titles.user
            || update.generation != titles.generation
            || titles
                .current
                .as_ref()
                .is_some_and(|current| current.to_lowercase() == update.title.to_lowercase())
        {
            return Ok(());
        }
        titles.current = Some(update.title.clone());
        self.append(
            state,
            EventKind::SessionTitled {
                title: update.title,
                by: update.by,
                model: update.model,
                usage: update.usage,
            },
        )
    }

    fn auto_titles(&self, state: &State) -> bool {
        self.0.options.auto_title
            && state.config.title.auto
            && !state.titles.user
            && !state.titles.settled
    }

    /// At a turn's start: chooses the prompt to name and requests the first
    /// title, unless that request would hold up the turn's own on a limited
    /// endpoint. The second pass then names the session alone.
    pub(crate) fn start_titles(&self, state: &mut State) {
        if !self.auto_titles(state) || state.titles.anchor.is_some() {
            return;
        }
        let Some(prompt) = state.log.events().iter().find_map(eligible) else {
            return;
        };
        state.titles.anchor = Some(prompt.clone());
        if !shares_limited_endpoint(&state.config) {
            self.spawn_title(
                state,
                TitleSource::FirstPrompt,
                format!("Request:\n{prompt}"),
            );
        }
    }

    /// At a turn's end: names the session again from the named prompt's
    /// first completed turn, unless fetched content reached the head.
    pub(crate) fn finish_titles(&self, state: &mut State, outcome: &TurnOutcome) {
        if !self.auto_titles(state) || outcome.failed || outcome.cancelled {
            return;
        }
        let Some(prompt) = state.titles.anchor.clone() else {
            return;
        };
        state.titles.settled = true;
        if state.log.is_tainted(state.head) == Some(true) {
            return;
        }
        let input = format!(
            "Request:\n{prompt}\n\nCurrent title: {}\n\nFinal reply:\n{}",
            state.titles.current.as_deref().unwrap_or("none"),
            excerpt(&outcome.text),
        );
        self.spawn_title(state, TitleSource::FirstTurn, input);
    }

    /// Requests a title in the background, superseding any in flight. When
    /// `tiny` is the session's own model, its connected provider serves the
    /// request. Missing credentials or an unknown model leave the title as
    /// it was.
    fn spawn_title(&self, state: &mut State, by: TitleSource, input: String) {
        let model = state.config.role(ModelRole::Tiny).clone();
        let (provider, info) = match &state.backend {
            Some(backend) if model == state.config.model => {
                (backend.provider.clone(), backend.model.clone())
            }
            _ => {
                let registry = state.config.registry();
                let Some(info) = registry.model(&model).cloned() else {
                    return;
                };
                let Ok(provider) = registry.connect(&model) else {
                    return;
                };
                (provider, info)
            }
        };
        let request = Request {
            model: model.model.clone(),
            system: include_str!("title.md").into(),
            messages: vec![Message::User(vec![Block::Text { text: input }])],
            tools: Vec::new(),
            max_tokens: MAX_TOKENS.min(info.max_output),
            reasoning: info.resolve(Reasoning::Off),
            cache_key: None,
            web_search: false,
        };
        let titles = &mut state.titles;
        if let Some(cancel) = titles.cancel.take() {
            cancel.cancel();
        }
        titles.generation += 1;
        let generation = titles.generation;
        let cancel = CancellationToken::new();
        titles.cancel = Some(cancel.clone());
        let config = state.config.clone();
        let session = self.clone();
        tokio::spawn(async move {
            let response = session
                .stream(provider.as_ref(), &request, &config, &model, &cancel, false)
                .await;
            let Ok(response) = response else {
                return;
            };
            if response.stop != StopReason::EndTurn {
                return;
            }
            let text: String = response
                .content
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let Some(title) = normalize(&text) else {
                return;
            };
            let update = Update {
                title,
                by,
                model: Some(model),
                usage: Some(response.usage),
                generation,
            };
            // A failed append leaves the log unwritable; the turn reports it.
            let _ = session.deliver(update).await;
        });
    }
}

/// The text of a user message worth naming: at least four words or twenty
/// characters, so "hi" or "continue" waits for a later prompt.
fn eligible(event: &Event) -> Option<String> {
    let EventKind::UserMessage { content } = &event.kind else {
        return None;
    };
    let text = xi_protocol::skill::typed(content)
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (text.split_whitespace().count() >= 4 || text.chars().count() >= 20).then(|| excerpt(text))
}

fn excerpt(text: &str) -> String {
    text.trim().chars().take(EXCERPT_CHARS).collect()
}

/// Whether the `tiny` model shares the session model's endpoint and that
/// endpoint has a `[[daemon.limit]]`, so a title request would queue the
/// turn's next request behind it.
fn shares_limited_endpoint(config: &Config) -> bool {
    let registry = config.registry();
    let endpoint = |model: &ModelRef| {
        registry
            .provider(&model.provider)
            .map(|provider| provider.base_url.clone())
    };
    let Some(tiny) = endpoint(config.role(ModelRole::Tiny)) else {
        return false;
    };
    endpoint(&config.model).as_ref() == Some(&tiny)
        && config
            .daemon
            .limit
            .iter()
            .any(|limit| limit.endpoint == tiny)
}

/// The model's reply as a title: its first line without markdown, quotes,
/// a `Title:` label or a trailing period, cut at a word to 60 characters.
fn normalize(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line.trim_start_matches('#').trim_start();
    let line = match line.get(..6) {
        Some(label) if label.eq_ignore_ascii_case("title:") => &line[6..],
        _ => line,
    };
    let line = line
        .trim()
        .trim_matches(['"', '`', '*', '“', '”'])
        .trim()
        .trim_end_matches('.')
        .trim_end();
    let mut title = String::new();
    for word in line.split_whitespace() {
        let width = title.chars().count() + usize::from(!title.is_empty()) + word.chars().count();
        if width > AUTO_CHARS {
            break;
        }
        if !title.is_empty() {
            title.push(' ');
        }
        title.push_str(word);
    }
    if title.is_empty() {
        title = line.chars().take(AUTO_CHARS).collect();
    }
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn normalize_keeps_one_clean_line_within_sixty_characters() {
        for (reply, title) in [
            (
                "Fix the flaky daemon test",
                Some("Fix the flaky daemon test"),
            ),
            (
                "\n## Title: \"Route bash through the proxy.\"\nExtra",
                Some("Route bash through the proxy"),
            ),
            ("**Add session titles**", Some("Add session titles")),
            ("   \n\n", None),
            ("\"\"", None),
        ] {
            assert_eq!(normalize(reply).as_deref(), title, "{reply:?}");
        }
        let long = "word ".repeat(30);
        let cut = normalize(&long).unwrap();
        assert!(cut.chars().count() <= 60 && cut.ends_with("word"), "{cut}");
        let unbroken = "x".repeat(90);
        assert_eq!(normalize(&unbroken).unwrap().chars().count(), 60);
    }
}
