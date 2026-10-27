//! The opt-in `task` tool: concurrent child sessions for independent work.
//!
//! Naming `task` in `tools` enables it. Each item runs as a linked child
//! session through the same path as an extension's `agent/run`: the child
//! gets the parent's built-in tools or a subset, its asks are denied
//! headlessly, and it cannot delegate, because `task` is never one of a
//! child's tools. The kernel runs the call itself; [`Task`] only puts the
//! tool in the catalog, so policy, hooks and name checks treat it like any
//! other tool.

use serde::Deserialize;
use serde_json::{Value, json};
use xi_protocol::BoxFuture;
use xi_protocol::manifest::ToolSpec;
use xi_protocol::methods::extension::AgentRunResult;
use xi_protocol::model::ModelSelector;
use xi_protocol::{OutputBlock, ToolCallId, ToolOutput};
use xi_tools::{Context, Crossing, Ran, Tool};

/// The tool name in `tools` and in the model's catalog.
pub(crate) const NAME: &str = "task";
const MAX_TASKS: usize = 8;

/// The catalog entry for `task`.
pub(crate) struct Task;

impl Tool for Task {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "Run 1 to 8 child agents concurrently for independent sub-problems. Give each child a short name and a self-contained prompt with its files, goal and acceptance criteria. Children inherit your enabled tools, including web_search and fetch, or an explicit subset; they cannot delegate and approval asks are denied headlessly. Children share the workspace: assign disjoint files when editing. Do not use for dependent steps, small direct questions, or work you can finish with a few tool calls. Results return in task order after all children finish.".into(),
            input_schema: json!({
                "type": "object",
                "required": ["tasks"],
                "additionalProperties": false,
                "properties": {"tasks": {
                    "type": "array", "minItems": 1, "maxItems": MAX_TASKS,
                    "items": {
                        "type": "object",
                        "required": ["prompt"],
                        "additionalProperties": false,
                        "properties": {
                            "prompt": {"type": "string", "minLength": 1},
                            "name": {
                                "type": "string", "minLength": 1, "maxLength": super::title::MAX_CHARS,
                                "description": "Short task name shown in the UI and results, such as History model or UX review; one line.",
                            },
                            "tools": {"type": "array", "items": {"type": "string"}},
                            "model": {
                                "type": "string",
                                "description": "Model for this child; your own when omitted. @smol or @tiny for routine, well-specified work; @slow for the hardest problems.",
                            },
                        },
                    },
                }},
            }),
        }
    }

    fn crossings(&self, input: &Value, _: &Context) -> Result<Vec<Crossing>, String> {
        parse(input.clone()).map(|_| Vec::new())
    }

    fn run<'a>(&'a self, _: &'a ToolCallId, _: Value, _: &'a Context) -> BoxFuture<'a, Ran> {
        // The session runs `task` calls; this is never reached through it.
        Box::pin(async {
            Ran {
                output: ToolOutput {
                    content: vec![OutputBlock::Text {
                        text: "task runs only inside a session".into(),
                    }],
                    is_error: true,
                    untrusted: false,
                },
                spill: None,
            }
        })
    }
}

/// One child to run.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Child {
    pub prompt: String,
    pub name: Option<String>,
    pub tools: Option<Vec<String>>,
    pub model: Option<ModelSelector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    tasks: Vec<Item>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    prompt: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    model: Option<String>,
}

/// Validates a call's input before any child starts.
///
/// # Errors
/// Returns a message for the model when the input does not match the schema
/// or names an invalid model.
pub(crate) fn parse(input: Value) -> Result<Vec<Child>, String> {
    let input: Input = serde_json::from_value(input).map_err(|error| error.to_string())?;
    if !(1..=MAX_TASKS).contains(&input.tasks.len()) {
        return Err(format!("Expected tasks with 1 to {MAX_TASKS} items"));
    }
    input
        .tasks
        .into_iter()
        .map(|item| {
            if item.prompt.trim().is_empty() {
                return Err("Each task needs a nonempty prompt".into());
            }
            let model = item
                .model
                .map(|model| model.parse::<ModelSelector>())
                .transpose()
                .map_err(|error| error.to_string())?;
            Ok(Child {
                prompt: item.prompt,
                name: item.name.map(|name| child_name(&name)).transpose()?,
                tools: item.tools,
                model,
            })
        })
        .collect()
}

/// Uses the same limits as session titles for parent-assigned child names.
pub(crate) fn child_name(name: &str) -> Result<String, String> {
    super::title::validate(name)
        .map(str::to_owned)
        .map_err(|error| format!("child name: {error}"))
}

/// A child's answer, with whether its context ended tainted.
pub(crate) type ChildResult = crate::Result<(AgentRunResult, bool)>;

/// Joins child results in task order. The output is an error when any child
/// failed, and untrusted when any child's context was tainted, so the
/// parent cannot launder a child's untrusted reads.
pub(crate) fn output(results: Vec<(Option<String>, ChildResult)>) -> ToolOutput {
    let mut failed = false;
    let mut untrusted = false;
    let text: Vec<_> = results
        .into_iter()
        .enumerate()
        .map(|(index, (name, result))| {
            let mut label = format!("Task {}", index + 1);
            if let Some(name) = name {
                label.push_str(" · ");
                label.push_str(&name);
            }
            match result {
                Ok((result, tainted)) => {
                    failed |= result.failed;
                    untrusted |= tainted;
                    let status = if result.failed { "FAILED" } else { "done" };
                    format!("{label} [{status}]\n{}", result.text)
                }
                Err(error) => {
                    failed = true;
                    format!("{label} [FAILED]\n{error}")
                }
            }
        })
        .collect();
    ToolOutput {
        content: vec![OutputBlock::Text {
            text: text.join("\n\n"),
        }],
        is_error: failed,
        untrusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xi_protocol::SessionId;
    use xi_protocol::event::Usage;
    use xi_protocol::model::ModelRole;

    #[test]
    fn parse_accepts_roles_and_models_and_rejects_bad_input() {
        let children = parse(json!({"tasks": [
            {"prompt": "a", "name": " UX review ", "model": "@smol", "tools": ["read"]},
            {"prompt": "b", "model": "claude/claude-haiku-5-5"},
            {"prompt": "c"},
        ]}))
        .unwrap();
        assert_eq!(children[0].model, Some(ModelRole::Smol.into()));
        assert_eq!(children[0].tools.as_deref(), Some(&["read".to_owned()][..]));
        assert_eq!(children[0].name.as_deref(), Some("UX review"));
        assert_eq!(children[2].name, None);
        assert_eq!(
            children[1].model.as_ref().unwrap().to_string(),
            "claude/claude-haiku-5-5"
        );
        assert_eq!(children[2].model, None);
        for (input, message) in [
            (json!({"tasks": []}), "1 to 8"),
            (json!({"tasks": vec![json!({"prompt": "x"}); 9]}), "1 to 8"),
            (json!({"tasks": [{"prompt": " "}]}), "nonempty prompt"),
            (
                json!({"tasks": [{"prompt": "x", "model": "@huge"}]}),
                "unknown model role",
            ),
            (
                json!({"tasks": [{"prompt": "x", "model": "gpt"}]}),
                "invalid model",
            ),
            (
                json!({"tasks": [{"prompt": "x", "extra": 1}]}),
                "unknown field",
            ),
            (json!({}), "missing field"),
        ] {
            let error = parse(input.clone()).unwrap_err();
            assert!(error.contains(message), "{input}: {error}");
        }
    }

    #[test]
    fn names_follow_title_limits_before_any_child_is_started() {
        let name = "界".repeat(super::super::title::MAX_CHARS);
        let children = parse(json!({"tasks": [{"prompt": "inspect", "name": name}]})).unwrap();
        assert_eq!(children[0].name.as_deref(), Some(name.as_str()));
        for name in [
            "".into(),
            " \t ".into(),
            "a\nb".into(),
            "a\rb".into(),
            "界".repeat(81),
        ] {
            let error = parse(json!({"tasks": [
                {"prompt": "inspect", "name": "Valid"},
                {"prompt": "review", "name": name},
            ]}))
            .unwrap_err();
            assert!(error.contains("child name"), "{name:?}: {error}");
        }
        assert!(parse(json!({"tasks": [{"prompt": "inspect", "name": 1}]})).is_err());
    }

    #[test]
    fn output_keeps_task_order_and_carries_failure_and_taint() {
        let result = |text: &str, failed| AgentRunResult {
            session: SessionId::generate(),
            path: "/child.jsonl".into(),
            text: text.into(),
            failed,
            usage: Usage::default(),
        };
        let output = output(vec![
            (None, Ok((result("first", false), false))),
            (Some("Review".into()), Ok((result("second", false), true))),
        ]);
        assert_eq!(
            output.content,
            [OutputBlock::Text {
                text: "Task 1 [done]\nfirst\n\nTask 2 · Review [done]\nsecond".into()
            }]
        );
        assert!(!output.is_error);
        assert!(output.untrusted);
        let output = super::output(vec![
            (Some("Review".into()), Ok((result("partial", true), false))),
            (
                Some("Fix".into()),
                Err(crate::Error::Config(
                    "child tool x is not one of the parent's built-in tools".into(),
                )),
            ),
        ]);
        assert!(output.is_error);
        assert!(!output.untrusted);
        let OutputBlock::Text { text } = &output.content[0] else {
            panic!("text output")
        };
        assert!(
            text.starts_with("Task 1 · Review [FAILED]\npartial\n\nTask 2 · Fix [FAILED]\n"),
            "{text}"
        );
    }
}
