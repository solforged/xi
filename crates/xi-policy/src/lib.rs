//! Layered, taint-aware policy for boundary crossings.
//!
//! Policy decides what happens when an action would leave the sandbox:
//! network egress, reads or writes outside it, specific commands, and tool
//! use. Evaluation:
//!
//! 1. With no matching rule, the decision is [`Decision::Ask`].
//! 2. [`Authority::Full`] layers apply in the order they were added; the
//!    last matching rule wins.
//! 3. [`Authority::TightenOnly`] layers apply afterwards. A matching rule
//!    replaces the decision only when it is stricter, so their `allow`
//!    rules never take effect.
//! 4. In a tainted session, an `allow` for egress becomes `ask` unless the
//!    deciding rule was written with `when = "tainted"`.
//!
//! Patterns are globs over the action's subject. For paths, `*` does not
//! cross `/`; use `**`. Hosts match case-insensitively.

use std::fmt;
use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};
pub use xi_protocol::Decision;

/// Errors from loading policy.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The policy file is not valid TOML or has unknown fields.
    #[error("invalid policy file: {0}")]
    Parse(#[from] toml::de::Error),
    /// A rule's pattern is not a valid glob.
    #[error("{layer}#{index}: invalid pattern: {source}")]
    Pattern {
        /// Layer name.
        layer: String,
        /// Rule index within the layer.
        index: usize,
        /// The glob error.
        #[source]
        source: globset::Error,
    },
}

/// The kind of boundary crossing a rule applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Running a command line.
    Exec,
    /// Reading a path outside the sandbox.
    Read,
    /// Writing a path outside the sandbox.
    Write,
    /// Connecting to a host through the egress proxy.
    Net,
    /// Calling a tool.
    Tool,
}

/// A concrete boundary crossing to decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action<'a> {
    /// Running a command line.
    Exec {
        /// The full command line.
        command: &'a str,
    },
    /// Reading a path.
    Read {
        /// Absolute path.
        path: &'a Path,
    },
    /// Writing a path.
    Write {
        /// Absolute path.
        path: &'a Path,
    },
    /// Connecting to a host.
    Net {
        /// Host name without port.
        host: &'a str,
    },
    /// Calling a tool.
    Tool {
        /// Registered tool name.
        name: &'a str,
    },
}

impl<'a> Action<'a> {
    /// The action's kind.
    #[must_use]
    pub fn kind(&self) -> ActionKind {
        match self {
            Self::Exec { .. } => ActionKind::Exec,
            Self::Read { .. } => ActionKind::Read,
            Self::Write { .. } => ActionKind::Write,
            Self::Net { .. } => ActionKind::Net,
            Self::Tool { .. } => ActionKind::Tool,
        }
    }

    /// Whether the action can carry data out of the machine.
    #[must_use]
    pub fn is_egress(&self) -> bool {
        matches!(self, Self::Net { .. })
    }

    fn subject(&self) -> &'a Path {
        match *self {
            Self::Exec { command: s } | Self::Net { host: s } | Self::Tool { name: s } => {
                Path::new(s)
            }
            Self::Read { path } | Self::Write { path } => path,
        }
    }
}

/// Whether the session has ingested untrusted content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taint {
    /// Only trusted content so far.
    Clean,
    /// Untrusted content is in context.
    Tainted,
}

/// When a rule applies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// Regardless of taint. An `always` allow for egress still becomes
    /// `ask` in a tainted session.
    #[default]
    Always,
    /// Only in a tainted session. An allow written this way holds there.
    Tainted,
    /// Only in a clean session.
    Clean,
}

/// A rule as written in a policy file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    /// The decision when the rule matches.
    pub effect: Decision,
    /// The kind of action the rule applies to.
    pub action: ActionKind,
    /// Glob over the action's subject.
    pub pattern: String,
    /// Taint condition.
    #[serde(default)]
    pub when: Condition,
}

/// A policy file: a list of `[[rule]]` tables.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyFile {
    /// Rules in file order.
    #[serde(default, rename = "rule")]
    pub rules: Vec<RuleSpec>,
}

impl PolicyFile {
    /// Parses a policy file.
    ///
    /// # Errors
    /// Fails on invalid TOML or unknown fields.
    pub fn parse(text: &str) -> Result<Self, Error> {
        Ok(toml::from_str(text)?)
    }
}

/// What a layer may do to the decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Authority {
    /// May allow, ask or deny. Built-in, user, and the workspace-local file
    /// in a trusted workspace.
    Full,
    /// May only make the decision stricter. The committed project file, and
    /// the workspace-local file in an untrusted workspace.
    TightenOnly,
}

/// Identifies the rule behind a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleRef<'p> {
    /// Layer name.
    pub layer: &'p str,
    /// Rule index within the layer.
    pub index: usize,
}

impl fmt::Display for RuleRef<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.layer, self.index)
    }
}

/// The outcome of evaluating an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict<'p> {
    /// What to do.
    pub decision: Decision,
    /// The deciding rule; `None` when no rule matched.
    pub rule: Option<RuleRef<'p>>,
    /// Whether taint turned the rule's `allow` into `ask`.
    pub taint_downgraded: bool,
}

/// An ordered set of policy layers.
#[derive(Debug, Default)]
pub struct Policy {
    layers: Vec<Layer>,
}

#[derive(Debug)]
struct Layer {
    name: String,
    authority: Authority,
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    effect: Decision,
    action: ActionKind,
    when: Condition,
    matcher: GlobMatcher,
}

impl Rule {
    fn matches(&self, action: &Action<'_>, taint: Taint) -> bool {
        let condition = match self.when {
            Condition::Always => true,
            Condition::Tainted => taint == Taint::Tainted,
            Condition::Clean => taint == Taint::Clean,
        };
        condition && self.action == action.kind() && self.matcher.is_match(action.subject())
    }
}

impl Policy {
    /// Creates an empty policy, which asks for everything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a layer after the existing ones.
    ///
    /// # Errors
    /// Fails if a pattern is not a valid glob; the policy is unchanged.
    pub fn add_layer(
        &mut self,
        name: impl Into<String>,
        authority: Authority,
        rules: &[RuleSpec],
    ) -> Result<(), Error> {
        let name = name.into();
        let rules = rules
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                let mut glob = GlobBuilder::new(&spec.pattern);
                match spec.action {
                    ActionKind::Read | ActionKind::Write => glob.literal_separator(true),
                    ActionKind::Net => glob.case_insensitive(true),
                    ActionKind::Exec | ActionKind::Tool => &mut glob,
                };
                let matcher = glob
                    .build()
                    .map_err(|source| Error::Pattern {
                        layer: name.clone(),
                        index,
                        source,
                    })?
                    .compile_matcher();
                Ok(Rule {
                    effect: spec.effect,
                    action: spec.action,
                    when: spec.when,
                    matcher,
                })
            })
            .collect::<Result<_, Error>>()?;
        self.layers.push(Layer {
            name,
            authority,
            rules,
        });
        Ok(())
    }

    /// Decides an action.
    #[must_use]
    pub fn evaluate(&self, action: Action<'_>, taint: Taint) -> Verdict<'_> {
        let mut decision = Decision::Ask;
        let mut decided_by: Option<(RuleRef<'_>, &Rule)> = None;

        for authority in [Authority::Full, Authority::TightenOnly] {
            for layer in self.layers.iter().filter(|l| l.authority == authority) {
                for (index, rule) in layer.rules.iter().enumerate() {
                    if !rule.matches(&action, taint) {
                        continue;
                    }
                    if authority == Authority::Full || rule.effect > decision {
                        decision = rule.effect;
                        let rule_ref = RuleRef {
                            layer: &layer.name,
                            index,
                        };
                        decided_by = Some((rule_ref, rule));
                    }
                }
            }
        }

        let taint_downgraded = taint == Taint::Tainted
            && decision == Decision::Allow
            && action.is_egress()
            && decided_by.is_none_or(|(_, rule)| rule.when != Condition::Tainted);
        if taint_downgraded {
            decision = Decision::Ask;
        }
        Verdict {
            decision,
            rule: decided_by.map(|(rule_ref, _)| rule_ref),
            taint_downgraded,
        }
    }
}
