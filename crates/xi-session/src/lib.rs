//! Append-only session tree log.
//!
//! A session is a JSONL file of [`Event`]s. Parent links form a tree with one
//! `session_started` root; every other event names a parent that appears
//! earlier in the file. Branching appends a child to an older event, so
//! nothing is ever rewritten.
//!
//! Durability: an append is written as one buffer and synced before it
//! returns. An unterminated final line therefore belongs to an append that
//! was never acknowledged, and [`SessionLog::open`] truncates it. A malformed
//! complete line is corruption and fails the open.
//!
//! The file is locked exclusively while a [`SessionLog`] is alive, so only
//! one process appends to a session.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use xi_protocol::{Event, EventId, EventKind, Timestamp};

/// Errors from opening or appending to a session.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The filesystem failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The session file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Another `SessionLog` holds the file.
    #[error("{} is open elsewhere", .0.display())]
    Locked(PathBuf),
    /// A complete line is not a valid event, or breaks a tree invariant.
    #[error("{}:{line}: {reason}", path.display())]
    Corrupt {
        /// The session file.
        path: PathBuf,
        /// 1-based line number.
        line: usize,
        /// What is wrong with the line.
        reason: String,
    },
    /// The event would break a tree invariant. Nothing was written.
    #[error(transparent)]
    Rejected(#[from] Violation),
    /// The event could not be encoded, for example a non-UTF-8 path.
    #[error("cannot encode event: {0}")]
    Encode(#[source] serde_json::Error),
    /// An earlier write failed and could not be rolled back. Reopen the
    /// session, which truncates the partial line.
    #[error("{}: an earlier write failed and could not be rolled back; reopen the session", .0.display())]
    Broken(PathBuf),
}

/// Tree invariants an event can break.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Violation {
    /// The first event must be a parentless `session_started`.
    #[error("the first event must be session_started with no parent")]
    BadRoot,
    /// Only the first event may lack a parent.
    #[error("event {0} has no parent, but the session already has a root")]
    SecondRoot(EventId),
    /// `session_started` may only appear as the root.
    #[error("event {0} is session_started but not the root")]
    MisplacedStart(EventId),
    /// The parent must already be in the log.
    #[error("parent {0} is not in the session")]
    UnknownParent(EventId),
    /// Event ids are unique within a session.
    #[error("event id {0} is already in the session")]
    DuplicateId(EventId),
    /// A compaction's `kept` must be a strict ancestor that starts a message.
    #[error("compaction {0} keeps {1}, which is not a message on its path")]
    BadKept(EventId, EventId),
}

/// The context visible from one event: the latest compaction summary on its
/// path and the events after it, root first.
#[derive(Debug, Clone, PartialEq)]
pub struct Context<'a> {
    /// Summary of everything before `events`, if the path was compacted.
    pub summary: Option<&'a str>,
    /// Events after the latest compaction, or the whole path from the root.
    pub events: Vec<&'a Event>,
}

/// An open, locked session file and its in-memory index.
#[derive(Debug)]
pub struct SessionLog {
    path: PathBuf,
    file: File,
    /// Bytes of acknowledged events in the file.
    len: u64,
    broken: bool,
    dropped_tail: u64,
    events: Vec<Event>,
    index: HashMap<EventId, usize>,
    children: HashMap<EventId, Vec<EventId>>,
}

impl SessionLog {
    /// Creates a new session file whose root is `root`, which must be
    /// [`EventKind::SessionStarted`].
    ///
    /// # Errors
    /// Fails if the file exists, `root` is not `session_started`, or the
    /// write fails. A failed write removes the file.
    pub fn create(path: impl Into<PathBuf>, root: EventKind) -> Result<Self, Error> {
        let path = path.into();
        if !matches!(root, EventKind::SessionStarted { .. }) {
            return Err(Violation::BadRoot.into());
        }
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| io_error(&path, source))?;
        lock(&file, &path)?;
        let mut log = Self::new(path, file);
        let event = Event {
            id: EventId::generate(),
            parent: None,
            at: Timestamp::now(),
            kind: root,
        };
        if let Err(error) = log.write(event) {
            // Best effort: an empty file would fail to open as "no root".
            let _ = fs::remove_file(&log.path);
            return Err(error);
        }
        Ok(log)
    }

    /// Opens and validates an existing session file, truncating an
    /// unterminated final line.
    ///
    /// # Errors
    /// Fails if the file is locked, unreadable, has no root, or contains a
    /// complete line that is not a valid event in tree order.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|source| io_error(&path, source))?;
        lock(&file, &path)?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| io_error(&path, source))?;
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let dropped_tail = (bytes.len() - complete) as u64;
        if dropped_tail > 0 {
            file.set_len(complete as u64)
                .and_then(|()| file.sync_data())
                .map_err(|source| io_error(&path, source))?;
        }

        let mut log = Self::new(path, file);
        log.len = complete as u64;
        log.dropped_tail = dropped_tail;
        for (offset, line) in bytes[..complete]
            .split_inclusive(|&b| b == b'\n')
            .enumerate()
        {
            let corrupt = |reason: String| Error::Corrupt {
                path: log.path.clone(),
                line: offset + 1,
                reason,
            };
            let event: Event = serde_json::from_slice(&line[..line.len() - 1])
                .map_err(|error| corrupt(error.to_string()))?;
            log.check(&event)
                .map_err(|violation| corrupt(violation.to_string()))?;
            log.insert(event);
        }
        if log.events.is_empty() {
            return Err(Error::Corrupt {
                path: log.path,
                line: 1,
                reason: "session has no root event".to_owned(),
            });
        }
        Ok(log)
    }

    /// Appends a new event as a child of `parent` and returns it once it is
    /// durable.
    ///
    /// # Errors
    /// Fails without writing if `parent` is unknown or `kind` is
    /// `session_started`. Fails if the write or sync fails; the partial write
    /// is rolled back.
    pub fn append(&mut self, parent: EventId, kind: EventKind) -> Result<&Event, Error> {
        let event = Event {
            id: EventId::generate(),
            parent: Some(parent),
            at: Timestamp::now(),
            kind,
        };
        self.check(&event)?;
        self.write(event)
    }

    /// The session file.
    #[must_use]
    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// Bytes of an unterminated final line that [`Self::open`] truncated.
    #[must_use]
    pub fn dropped_tail_bytes(&self) -> u64 {
        self.dropped_tail
    }

    /// The `session_started` event.
    #[must_use]
    pub fn root(&self) -> &Event {
        &self.events[0]
    }

    /// Every event in file order.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Looks up an event.
    #[must_use]
    pub fn get(&self, id: EventId) -> Option<&Event> {
        self.index.get(&id).map(|&i| &self.events[i])
    }

    /// Direct children of an event, in append order.
    #[must_use]
    pub fn children(&self, id: EventId) -> &[EventId] {
        self.children.get(&id).map_or(&[], Vec::as_slice)
    }

    /// Events without children, in file order. Each is the tip of a branch.
    pub fn leaves(&self) -> impl Iterator<Item = &Event> {
        self.events
            .iter()
            .filter(|event| !self.children.contains_key(&event.id))
    }

    /// The path from the root to `id`, inclusive, root first.
    #[must_use]
    pub fn lineage(&self, id: EventId) -> Option<Vec<&Event>> {
        let mut path = Vec::new();
        let mut next = Some(id);
        while let Some(id) = next {
            let event = self.get(id)?;
            path.push(event);
            next = event.parent;
        }
        path.reverse();
        Some(path)
    }

    /// The context visible from `id`: the latest compaction summary on its
    /// path, the events it kept, then the events after that compaction.
    #[must_use]
    pub fn context(&self, id: EventId) -> Option<Context<'_>> {
        let mut path = self.lineage(id)?;
        let compaction = path
            .iter()
            .rposition(|event| matches!(event.kind, EventKind::Compaction { .. }));
        let Some(at) = compaction else {
            return Some(Context {
                summary: None,
                events: path,
            });
        };
        let after = path.split_off(at + 1);
        let EventKind::Compaction { summary, kept, .. } = &path[at].kind else {
            unreachable!("position matched a compaction");
        };
        let kept = kept
            .and_then(|kept| path[..at].iter().position(|event| event.id == kept))
            .map_or(&[][..], |start| &path[start..at]);
        let mut events = kept.to_vec();
        events.extend(after);
        Some(Context {
            summary: Some(summary),
            events,
        })
    }

    /// Whether the path to `id` contains a `tainted` event. Taint never
    /// crosses to sibling branches.
    #[must_use]
    pub fn is_tainted(&self, id: EventId) -> Option<bool> {
        let path = self.lineage(id)?;
        Some(
            path.iter()
                .any(|event| matches!(event.kind, EventKind::Tainted { .. })),
        )
    }

    fn new(path: PathBuf, file: File) -> Self {
        Self {
            path,
            file,
            len: 0,
            broken: false,
            dropped_tail: 0,
            events: Vec::new(),
            index: HashMap::new(),
            children: HashMap::new(),
        }
    }

    fn check(&self, event: &Event) -> Result<(), Violation> {
        if self.index.contains_key(&event.id) {
            return Err(Violation::DuplicateId(event.id));
        }
        let is_start = matches!(event.kind, EventKind::SessionStarted { .. });
        if self.events.is_empty() {
            return if event.parent.is_none() && is_start {
                Ok(())
            } else {
                Err(Violation::BadRoot)
            };
        }
        match event.parent {
            None => Err(Violation::SecondRoot(event.id)),
            Some(_) if is_start => Err(Violation::MisplacedStart(event.id)),
            Some(parent) if !self.index.contains_key(&parent) => {
                Err(Violation::UnknownParent(parent))
            }
            Some(parent) => match event.kind {
                EventKind::Compaction {
                    kept: Some(kept), ..
                } => {
                    let on_path = self.lineage(parent).is_some_and(|path| {
                        path.iter().any(|ancestor| {
                            ancestor.id == kept
                                && matches!(
                                    ancestor.kind,
                                    EventKind::UserMessage { .. }
                                        | EventKind::AssistantMessage { .. }
                                )
                        })
                    });
                    if on_path {
                        Ok(())
                    } else {
                        Err(Violation::BadKept(event.id, kept))
                    }
                }
                _ => Ok(()),
            },
        }
    }

    fn write(&mut self, event: Event) -> Result<&Event, Error> {
        if self.broken {
            return Err(Error::Broken(self.path.clone()));
        }
        let mut line = serde_json::to_vec(&event).map_err(Error::Encode)?;
        line.push(b'\n');
        if let Err(source) = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_data())
        {
            // The file is in append mode, so truncating back puts the next
            // append on a line boundary.
            if self.file.set_len(self.len).is_err() {
                self.broken = true;
            }
            return Err(io_error(&self.path, source));
        }
        self.len += line.len() as u64;
        Ok(self.insert(event))
    }

    fn insert(&mut self, event: Event) -> &Event {
        if let Some(parent) = event.parent {
            self.children.entry(parent).or_default().push(event.id);
        }
        self.index.insert(event.id, self.events.len());
        self.events.push(event);
        &self.events[self.events.len() - 1]
    }
}

/// Takes the exclusive lock, waiting up to half a second for a holder to go
/// away. A lock briefly outlives its owner: a child spawned by any thread in
/// this process holds inherited copies of open descriptors until it execs,
/// so a log reopened right after closing it can see itself as locked.
fn lock(file: &File, path: &Path) -> Result<(), Error> {
    const ATTEMPTS: u32 = 100;
    for attempt in 1..=ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(TryLockError::WouldBlock) if attempt < ATTEMPTS => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => return Err(Error::Locked(path.to_owned())),
            Err(TryLockError::Error(source)) => return Err(io_error(path, source)),
        }
    }
    unreachable!("the last attempt returns")
}

fn io_error(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        source,
    }
}
