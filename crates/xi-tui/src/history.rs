//! Workspace prompt history and reverse substring search.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::style::{Line, Role};

const MAX_ENTRIES: usize = 5_000;

#[derive(Serialize, Deserialize)]
struct Entry {
    cwd: PathBuf,
    text: String,
}

/// Prompt history shared by workspaces, optionally persisted as JSONL.
pub struct History {
    path: Option<PathBuf>,
    cwd: PathBuf,
    entries: Vec<Entry>,
}

impl History {
    /// Loads prompt history. A missing file is empty; any other failure returns
    /// its error with an empty history, never a partially parsed history.
    #[must_use]
    pub fn load(path: Option<PathBuf>, cwd: &Path) -> (History, Option<io::Error>) {
        let mut history = Self {
            path,
            cwd: cwd.to_path_buf(),
            entries: Vec::new(),
        };
        let Some(path) = &history.path else {
            return (history, None);
        };
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return (history, None),
            Err(error) => return (history, Some(error)),
        };
        let entries: io::Result<Vec<Entry>> = BufReader::new(file)
            .lines()
            .map(|line| {
                serde_json::from_str(&line?)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .collect();
        match entries {
            Ok(entries) => {
                history.entries = entries;
                (history, None)
            }
            Err(error) => (history, Some(error)),
        }
    }

    /// Records nonblank text unless it exactly repeats the newest prompt.
    /// Appends one JSONL record, atomically retaining the newest 5,000 when full.
    pub fn push(&mut self, text: &str) -> io::Result<()> {
        if text.trim().is_empty() || self.entries.last().is_some_and(|entry| entry.text == text) {
            return Ok(());
        }
        let entry = Entry {
            cwd: self.cwd.clone(),
            text: text.to_owned(),
        };
        if let Some(path) = &self.path {
            let parent = parent_dir(path);
            fs::create_dir_all(parent)?;
            let mut file = OpenOptions::new().create(true).append(true).open(path)?;
            write_entry(&mut file, &entry)?;
            file.flush()?;
        }
        self.entries.push(entry);
        if self.entries.len() > MAX_ENTRIES {
            self.entries.drain(..self.entries.len() - MAX_ENTRIES);
            if let Some(path) = &self.path {
                let mut temporary = tempfile::NamedTempFile::new_in(parent_dir(path))?;
                for entry in &self.entries {
                    write_entry(&mut temporary, entry)?;
                }
                temporary.flush()?;
                temporary.persist(path).map_err(|error| error.error)?;
            }
        }
        Ok(())
    }

    /// This workspace's prompts in oldest-first order.
    #[must_use]
    pub fn workspace(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| entry.cwd == self.cwd)
            .map(|entry| entry.text.clone())
            .collect()
    }

    /// Case-insensitive substring matches, deduplicated by exact prompt text.
    /// Current-workspace matches precede other workspaces, newest first in each.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<&str> {
        let query = query.to_lowercase();
        let mut seen = HashSet::new();
        let mut matches = Vec::new();
        for local in [true, false] {
            for entry in self.entries.iter().rev() {
                if (entry.cwd == self.cwd) == local
                    && (query.is_empty() || entry.text.to_lowercase().contains(&query))
                    && seen.insert(entry.text.as_str())
                {
                    matches.push(entry.text.as_str());
                }
            }
        }
        matches
    }
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn write_entry(writer: &mut impl Write, entry: &Entry) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, entry).map_err(io::Error::other)?;
    writer.write_all(b"\n")
}

/// An incremental history search, preserving the draft for cancellation.
pub struct Search {
    query: String,
    index: usize,
    draft: String,
}

impl Search {
    /// Starts a search with an empty query and saves the current draft.
    #[must_use]
    pub fn new(draft: String) -> Search {
        Self {
            query: String::new(),
            index: 0,
            draft,
        }
    }

    /// The substring being searched for.
    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Adds a query character and returns to the newest match.
    pub fn input(&mut self, c: char) {
        self.query.push(c);
        self.index = 0;
    }

    /// Removes a query character and returns to the newest match.
    pub fn backspace(&mut self) {
        self.query.pop();
        self.index = 0;
    }

    /// Selects the next older match, remaining at the oldest when exhausted.
    pub fn older(&mut self) {
        self.index = self.index.saturating_add(1);
    }

    /// The selected match, if any.
    #[must_use]
    pub fn current<'a>(&self, history: &'a History) -> Option<&'a str> {
        let matches = history.search(&self.query);
        matches
            .get(self.index.min(matches.len().saturating_sub(1)))
            .copied()
    }

    /// The draft saved when search opened.
    #[must_use]
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Renders a search header and the selected prompt, highlighting the query.
    #[must_use]
    pub fn render(&self, history: &History, cols: usize) -> Vec<Line> {
        let mut lines = vec![
            Line::default()
                .dim("history search · ")
                .brass(&self.query)
                .truncate(cols),
        ];
        let Some(text) = self.current(history) else {
            lines.push(Line::default().dim("no matches").truncate(cols));
            return lines;
        };
        for row in text.split('\n') {
            lines.push(highlight(row, &self.query).truncate(cols));
        }
        lines
    }
}

fn highlight(text: &str, query: &str) -> Line {
    if query.is_empty() {
        return Line::default().ink(text);
    }
    let query = query.to_lowercase();
    let Some(start) = text.to_lowercase().find(&query) else {
        return Line::default().ink(text);
    };
    let end = start + query.len();
    let mut lowered = 0;
    let mut first = None;
    let mut last = 0;
    for (offset, c) in text.char_indices() {
        let next = lowered + c.to_lowercase().map(char::len_utf8).sum::<usize>();
        if lowered < end && next > start {
            first.get_or_insert(offset);
            last = offset + c.len_utf8();
        }
        lowered = next;
        if lowered >= end {
            break;
        }
    }
    let Some(first) = first else {
        return Line::default().ink(text);
    };
    Line::default()
        .ink(&text[..first])
        .strong(Role::Brass, &text[first..last])
        .ink(&text[last..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_persists_prompts_and_skips_blanks_and_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/prompts.jsonl");
        let (mut history, error) = History::load(Some(path.clone()), Path::new("/workspace"));
        assert!(error.is_none());
        history.push(" \n ").unwrap();
        history.push("first\nsecond").unwrap();
        history.push("first\nsecond").unwrap();
        history.push("third").unwrap();
        let (reloaded, error) = History::load(Some(path.clone()), Path::new("/workspace"));
        assert!(error.is_none());
        assert_eq!(reloaded.workspace(), ["first\nsecond", "third"]);
        let records: Vec<serde_json::Value> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["cwd"], "/workspace");
        assert_eq!(records[0]["text"], "first\nsecond");
    }

    #[test]
    fn trimming_rewrites_only_the_newest_five_thousand() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompts.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        for i in 0..MAX_ENTRIES {
            write_entry(
                &mut file,
                &Entry {
                    cwd: PathBuf::from("/workspace"),
                    text: i.to_string(),
                },
            )
            .unwrap();
        }
        drop(file);
        let (mut history, _) = History::load(Some(path.clone()), Path::new("/workspace"));
        history.push("newest").unwrap();
        let (history, error) = History::load(Some(path), Path::new("/workspace"));
        assert!(error.is_none());
        assert_eq!(history.entries.len(), MAX_ENTRIES);
        assert_eq!(history.entries[0].text, "1");
        assert_eq!(history.entries[MAX_ENTRIES - 1].text, "newest");
    }

    #[test]
    fn workspace_and_search_prioritize_local_newest_and_deduplicate() {
        let (mut history, _) = History::load(None, Path::new("/here"));
        for (cwd, text) in [
            ("/here", "Find old"),
            ("/elsewhere", "find elsewhere"),
            ("/here", "Find shared"),
            ("/here", "unrelated"),
            ("/elsewhere", "Find shared"),
            ("/here", "FIND newest"),
            ("/elsewhere", "find newest elsewhere"),
        ] {
            history.entries.push(Entry {
                cwd: cwd.into(),
                text: text.into(),
            });
        }
        assert_eq!(
            history.workspace(),
            ["Find old", "Find shared", "unrelated", "FIND newest"]
        );
        assert_eq!(
            history.search("fInD"),
            [
                "FIND newest",
                "Find shared",
                "Find old",
                "find newest elsewhere",
                "find elsewhere"
            ]
        );
        assert!(history.search("missing").is_empty());
    }

    #[test]
    fn failed_load_discards_partial_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompts.jsonl");
        fs::write(&path, "{\"cwd\":\"/here\",\"text\":\"valid\"}\nnot json\n").unwrap();
        let (history, error) = History::load(Some(path), Path::new("/here"));
        assert_eq!(error.unwrap().kind(), io::ErrorKind::InvalidData);
        assert!(history.workspace().is_empty());
    }

    #[test]
    fn search_older_stops_at_last_and_backspace_resets_selection() {
        let (mut history, _) = History::load(None, Path::new("/here"));
        history.push("old ξ").unwrap();
        history.push("new ξ").unwrap();
        let mut search = Search::new("saved draft".into());
        search.input('ξ');
        assert_eq!(search.current(&history), Some("new ξ"));
        search.older();
        search.older();
        assert_eq!(search.current(&history), Some("old ξ"));
        search.backspace();
        assert_eq!(search.query(), "");
        assert_eq!(search.current(&history), Some("new ξ"));
        assert_eq!(search.draft(), "saved draft");
        search.input('x');
        assert_eq!(search.current(&history), None);
    }

    #[test]
    fn search_render_bounds_width_and_highlights_unicode_case_insensitively() {
        let (mut history, _) = History::load(None, Path::new("/here"));
        history.push("hello Ξ and more\nsecond line").unwrap();
        let mut search = Search::new(String::new());
        search.input('ξ');
        let lines = search.render(&history, 20);
        assert!(lines.iter().all(|line| line.width() <= 20));
        assert!(
            lines[1]
                .0
                .iter()
                .any(|span| span.text == "Ξ" && span.role == Role::Brass)
        );
        assert!(
            search
                .render(&history, 0)
                .iter()
                .all(|line| line.width() == 0)
        );
    }
}
