//! Command and workspace-file completion for the composer.

use std::cmp::Reverse;
use std::ops::Range;
use std::path::Path;

use unicode_width::UnicodeWidthStr;
use xi_protocol::manifest::CommandSpec;

use crate::style::{Line, Role};

/// The token being completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A slash command at the start of the prompt.
    Command,
    /// A plain-text workspace file mention.
    File,
}

/// The current completion query and the complete token to replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trigger {
    /// The source of completion candidates.
    pub kind: Kind,
    /// The whole token's byte range, including its `/` or `@` prefix.
    pub range: Range<usize>,
    /// The token text before the cursor, without the prefix.
    pub query: String,
}

/// Recognizes a slash command in the first token, or an `@` token at a
/// whitespace-delimited word start. An invalid byte cursor has no trigger.
#[must_use]
pub fn trigger(text: &str, cursor: usize) -> Option<Trigger> {
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    if start == cursor {
        return None;
    }
    let end = text[cursor..]
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map_or(text.len(), |(i, _)| cursor + i);
    let kind = match text.as_bytes()[start] {
        b'/' if start == 0 => Kind::Command,
        b'@' => Kind::File,
        _ => return None,
    };
    Some(Trigger {
        kind,
        range: start..end,
        query: text[start + 1..cursor].to_owned(),
    })
}

/// A completion candidate and the text inserted when it is accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The command name or relative file path shown in the menu.
    pub label: String,
    /// The command description, or an empty string for a file.
    pub detail: String,
    /// The replacement token, including its prefix and a trailing space.
    pub insert: String,
}

struct IndexedFile {
    path: String,
    folded: String,
    filename: usize,
}

/// A bounded, gitignore-aware index of workspace file paths.
#[derive(Default)]
pub struct FileIndex {
    paths: Vec<IndexedFile>,
}

impl FileIndex {
    /// Walks files under `root`, including dotfiles but never `.git` contents.
    /// This blocks and should run on a blocking task. Unreadable entries are skipped.
    #[must_use]
    pub fn build(root: &Path, limit: usize) -> FileIndex {
        let mut paths = Vec::new();
        if limit == 0 {
            return Self { paths };
        }
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build();
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(root) else {
                continue;
            };
            let path = relative.to_string_lossy().into_owned();
            let folded = path.to_lowercase();
            let filename = folded.rfind('/').map_or(0, |i| i + 1);
            paths.push(IndexedFile {
                path,
                folded,
                filename,
            });
            if paths.len() == limit {
                break;
            }
        }
        paths.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        Self { paths }
    }

    /// The number of indexed files.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether no files were indexed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Matching completion candidates and the currently selected row.
pub struct Menu {
    kind: Kind,
    range: Range<usize>,
    items: Vec<Item>,
    selected: usize,
}

impl Menu {
    /// Matches command names, preferring prefixes over fuzzy subsequences.
    #[must_use]
    pub fn commands(trigger: &Trigger, commands: &[CommandSpec]) -> Option<Menu> {
        if trigger.kind != Kind::Command {
            return None;
        }
        let query = trigger.query.to_lowercase();
        let mut matches: Vec<_> = commands
            .iter()
            .filter_map(|command| {
                let folded = command.name.to_lowercase();
                let score = fuzzy_score(&folded, &query)?;
                Some((!folded.starts_with(&query), Reverse(score), command))
            })
            .collect();
        matches.sort_unstable_by(|a, b| (&a.0, &a.1, &a.2.name).cmp(&(&b.0, &b.1, &b.2.name)));
        let items = matches
            .into_iter()
            .map(|(_, _, command)| Item {
                label: format!("/{}", command.name),
                detail: command.description.clone(),
                insert: format!("/{} ", command.name),
            })
            .collect();
        Self::new(trigger, items)
    }

    /// Fuzzy-matches paths, preferring file-name matches and contiguous runs.
    /// At most 50 candidates are returned.
    #[must_use]
    pub fn files(trigger: &Trigger, index: &FileIndex) -> Option<Menu> {
        if trigger.kind != Kind::File {
            return None;
        }
        let query = trigger.query.to_lowercase();
        let mut matches: Vec<_> = index
            .paths
            .iter()
            .filter_map(|file| {
                let score = fuzzy_score(&file.folded, &query)?;
                let filename_score = fuzzy_score(&file.folded[file.filename..], &query);
                Some((
                    filename_score.is_none(),
                    Reverse(filename_score.unwrap_or(score)),
                    file,
                ))
            })
            .collect();
        matches.sort_unstable_by(|a, b| (&a.0, &a.1, &a.2.path).cmp(&(&b.0, &b.1, &b.2.path)));
        let items = matches
            .into_iter()
            .take(50)
            .map(|(_, _, file)| Item {
                label: file.path.clone(),
                detail: String::new(),
                insert: format!("@{} ", file.path),
            })
            .collect();
        Self::new(trigger, items)
    }

    fn new(trigger: &Trigger, items: Vec<Item>) -> Option<Menu> {
        if items.is_empty() {
            None
        } else {
            Some(Self {
                kind: trigger.kind,
                range: trigger.range.clone(),
                items,
                selected: 0,
            })
        }
    }

    /// Moves up, stopping at the first candidate.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Moves down, stopping at the last candidate.
    pub fn down(&mut self) {
        self.selected = (self.selected + 1).min(self.items.len().saturating_sub(1));
    }

    /// The candidate that accepting the menu will insert.
    #[must_use]
    pub fn selected(&self) -> Option<&Item> {
        self.items.get(self.selected)
    }

    /// The complete token range to replace on acceptance.
    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Renders at most `rows` width-bounded candidates, keeping selection
    /// visible. Command descriptions share one column after the longest
    /// label, so it holds still while the selection scrolls.
    #[must_use]
    pub fn render(&self, cols: usize, rows: usize) -> Vec<Line> {
        let start = self.selected.saturating_add(1).saturating_sub(rows);
        let column = self
            .items
            .iter()
            .map(|item| item.label.width())
            .max()
            .unwrap_or(0);
        self.items
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
            .map(|(i, item)| {
                let active = i == self.selected;
                let role = if active { Role::Brass } else { Role::Ink };
                let mut line = Line::default()
                    .span(role, active, if active { "› " } else { "  " })
                    .span(role, active, &item.label);
                if self.kind == Kind::Command && !item.detail.is_empty() {
                    let pad = column - item.label.width() + 2;
                    line = line.ink(" ".repeat(pad)).dim(&item.detail);
                }
                line.truncate(cols)
            })
            .collect()
    }
}

// Greedy subsequence matching rewards word boundaries and consecutive characters,
// then breaks ties in favour of shorter candidates and earlier matches.
fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let mut wanted = query.chars().peekable();
    let mut score = 0;
    let mut previous_match = None;
    let mut previous = None;
    for (position, c) in candidate.chars().enumerate() {
        if wanted.peek() == Some(&c) {
            wanted.next();
            score += 10;
            if previous_match == position.checked_sub(1) && previous_match.is_some() {
                score += 20;
            }
            if position == 0
                || previous.is_some_and(|c: char| matches!(c, '/' | '_' | '-' | '.' | ' '))
            {
                score += 12;
            }
            score -= i64::try_from(position).unwrap_or(i64::MAX / 4);
            previous_match = Some(position);
            if wanted.peek().is_none() {
                return Some(score - i64::try_from(candidate.len()).unwrap_or(i64::MAX / 4));
            }
        }
        previous = Some(c);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn command(name: &str) -> CommandSpec {
        CommandSpec {
            name: name.into(),
            description: format!("Use {name}"),
        }
    }

    #[test]
    fn triggers_handle_commands_mentions_unicode_and_cursor_edges() {
        let cases = [
            ("/", 1, Some((Kind::Command, 0..1, ""))),
            ("/model arg", 3, Some((Kind::Command, 0..6, "mo"))),
            ("/model arg", 6, Some((Kind::Command, 0..6, "model"))),
            ("/model arg", 7, None),
            (" /model", 7, None),
            ("text\n/model", 11, None),
            ("email@host", 10, None),
            (
                "hello @src/lib.rs tail",
                10,
                Some((Kind::File, 6..17, "src")),
            ),
            ("@", 1, Some((Kind::File, 0..1, ""))),
            ("ξ @ξ.rs", 6, Some((Kind::File, 3..9, "ξ"))),
            ("@ξ", 2, None),
            ("/", 0, None),
            ("", 1, None),
            ("/help\nmore", 10, None),
        ];
        for (text, cursor, expected) in cases {
            let actual = trigger(text, cursor).map(|t| (t.kind, t.range, t.query));
            let expected = expected.map(|(kind, range, query)| (kind, range, query.to_owned()));
            assert_eq!(actual, expected, "{text:?} at {cursor}");
        }
    }

    #[test]
    fn command_ranking_prefers_prefixes_then_contiguous_matches() {
        let trigger = trigger("/co", 3).unwrap();
        let commands = [
            command("checkout"),
            command("compact"),
            command("copy"),
            command("cancel-output"),
        ];
        let menu = Menu::commands(&trigger, &commands).unwrap();
        assert_eq!(
            menu.items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["/copy", "/compact", "/cancel-output", "/checkout"]
        );
        assert_eq!(menu.selected().unwrap().insert, "/copy ");
        assert_eq!(menu.range(), 0..3);
        assert!(
            Menu::commands(
                &Trigger {
                    query: "absent".into(),
                    ..trigger
                },
                &commands
            )
            .is_none()
        );
    }

    #[test]
    fn file_walk_respects_ignores_dotfiles_git_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/config"), "secret").unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "ignored").unwrap();
        fs::write(dir.path().join(".visible"), "dotfile").unwrap();
        fs::write(dir.path().join("main.rs"), "source").unwrap();
        let index = FileIndex::build(dir.path(), 100);
        let paths: Vec<_> = index.paths.iter().map(|file| file.path.as_str()).collect();
        assert_eq!(paths, [".gitignore", ".visible", "main.rs"]);
        assert_eq!(index.len(), 3);
        assert_eq!(FileIndex::build(dir.path(), 1).len(), 1);
        assert!(FileIndex::build(dir.path(), 0).is_empty());
    }

    #[test]
    fn file_ranking_prefers_filename_then_contiguous_runs_and_limits_results() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("main")).unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        for path in [
            "main/other.rs",
            "src/main.rs",
            "src/my_awesome_idea_name.rs",
        ] {
            fs::write(dir.path().join(path), "").unwrap();
        }
        for i in 0..60 {
            fs::write(dir.path().join(format!("extra-{i}.txt")), "").unwrap();
        }
        let index = FileIndex::build(dir.path(), 100);
        let menu = Menu::files(&trigger("@main", 5).unwrap(), &index).unwrap();
        assert_eq!(
            menu.items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            [
                "src/main.rs",
                "src/my_awesome_idea_name.rs",
                "main/other.rs"
            ]
        );
        assert_eq!(menu.selected().unwrap().insert, "@src/main.rs ");
        assert_eq!(
            Menu::files(&trigger("@", 1).unwrap(), &index)
                .unwrap()
                .items
                .len(),
            50
        );
        assert!(Menu::files(&trigger("@missing", 8).unwrap(), &index).is_none());
    }

    #[test]
    fn menu_navigation_stops_at_edges_and_render_keeps_selection_visible() {
        let commands = [command("one"), command("two"), command("three")];
        let mut menu = Menu::commands(&trigger("/", 1).unwrap(), &commands).unwrap();
        menu.up();
        assert_eq!(menu.selected().unwrap().label, "/one");
        for _ in 0..5 {
            menu.down();
        }
        assert_eq!(menu.selected().unwrap().label, "/two");
        let lines = menu.render(12, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| line.width() <= 12));
        assert!(lines.last().unwrap().plain().starts_with("› /two"));
        assert!(
            lines
                .last()
                .unwrap()
                .0
                .iter()
                .any(|span| span.role == Role::Brass)
        );
        assert!(menu.render(12, 0).is_empty());
        assert!(menu.render(0, 2).iter().all(|line| line.width() == 0));
        // Descriptions line up after the longest label, `/three`.
        let starts: Vec<_> = menu
            .render(40, 3)
            .iter()
            .map(|line| {
                let text = line.plain();
                text[..text.find("Use ").unwrap()].chars().count()
            })
            .collect();
        assert_eq!(starts, [10, 10, 10]);
        menu.up();
        assert_eq!(menu.selected().unwrap().label, "/three");
    }
}
