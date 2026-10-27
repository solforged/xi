//! Typed policy subjects and approval answers.

use super::*;
use std::fmt;

/// A policy action, shared by live grants, persisted decisions and replay.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum ActionKey {
    /// Reading a canonical path.
    Read(PathBuf),
    /// Writing a canonical path.
    Write(PathBuf),
    /// Executing a command line.
    Exec(String),
    /// Connecting to a host, with zero denoting a tool's port-independent crossing.
    Net { host: String, port: u16 },
    /// Invoking a tool.
    Tool(String),
}

impl ActionKey {
    pub(crate) fn kind(&self) -> ActionKind {
        match self {
            Self::Read(_) => ActionKind::Read,
            Self::Write(_) => ActionKind::Write,
            Self::Exec(_) => ActionKind::Exec,
            Self::Net { .. } => ActionKind::Net,
            Self::Tool(_) => ActionKind::Tool,
        }
    }
    pub(crate) fn subject(&self) -> String {
        match self {
            Self::Read(path) | Self::Write(path) => path.display().to_string(),
            Self::Exec(subject) | Self::Tool(subject) => subject.clone(),
            Self::Net { host, .. } => host.clone(),
        }
    }
    pub(super) fn parse(action: &str) -> Option<Self> {
        let (kind, subject) = action.split_once(' ')?;
        Some(match kind {
            "read" => Self::Read(PathBuf::from(subject)),
            "write" => Self::Write(PathBuf::from(subject)),
            "exec" => Self::Exec(subject.into()),
            "tool" => Self::Tool(subject.into()),
            "net" => {
                let (host, port) = subject
                    .rsplit_once(':')
                    .and_then(|(host, port)| port.parse().ok().map(|port| (host, port)))
                    .unwrap_or((subject, 0));
                Self::Net {
                    host: host.into(),
                    port,
                }
            }
            _ => return None,
        })
    }
    pub(super) fn crossing(crossing: &Crossing) -> Self {
        match crossing {
            Crossing::Read(path) => Self::Read(path.clone()),
            Crossing::Write(path) => Self::Write(path.clone()),
            Crossing::Exec(command) => Self::Exec(command.clone()),
            Crossing::Net(host) => Self::Net {
                host: host.clone(),
                port: 0,
            },
        }
    }
}

impl fmt::Display for ActionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(path) => write!(f, "read {}", path.display()),
            Self::Write(path) => write!(f, "write {}", path.display()),
            Self::Exec(command) => write!(f, "exec {command}"),
            Self::Tool(name) => write!(f, "tool {name}"),
            Self::Net { host, port: 0 } => write!(f, "net {host}"),
            Self::Net { host, port } => write!(f, "net {host}:{port}"),
        }
    }
}

pub(crate) struct ApprovalAnswer {
    pub(crate) approved: bool,
    pub(crate) remember: Remember,
    pub(crate) grant: Option<(String, ActionKey)>,
}

pub(crate) struct PendingAsk {
    pub(crate) answer: oneshot::Sender<ApprovalAnswer>,
    pub(crate) taint: bool,
    pub(crate) grant: (String, ActionKey),
    pub(crate) workspace: PathBuf,
    pub(crate) trusted: bool,
    pub(crate) resolving: bool,
}

impl PendingAsk {
    pub(super) fn validate(&self, approved: bool, remember: Remember) -> Result<()> {
        if self.resolving {
            return Err(Error::NotPending);
        }
        if !remember.is_once() && (!approved || self.taint) {
            return Err(Error::InvalidRemember(
                "only an approved, non-taint ask can be remembered".into(),
            ));
        }
        if remember == Remember::Workspace && !self.trusted {
            return Err(Error::InvalidRemember(
                "workspace approvals require a trusted workspace".into(),
            ));
        }
        Ok(())
    }
}
