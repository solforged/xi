use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{BufRead, BufReader},
    path::Path,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use xi_protocol::{
    Event,
    content::Block,
    event::{EventKind, Usage},
};

use crate::Result;

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub(crate) struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
pub(crate) struct Metrics {
    pub model_calls: u64,
    #[serde(default)]
    pub subagent_model_calls: u64,
    #[serde(default)]
    pub compactions: u64,
    pub tokens: Tokens,
    pub tool_calls: BTreeMap<String, u64>,
    pub tool_busy_ms: f64,
    pub model_ms: f64,
    pub turn_failures: u64,
    pub edit_results: u64,
    pub edit_failures: u64,
    pub incomplete_tool_spans: usize,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TimedLine {
    pub at_ms: f64,
    pub line: String,
}

#[derive(Default)]
struct Accumulator {
    metrics: Metrics,
    names: HashMap<String, String>,
    starts: HashMap<String, f64>,
    spans: Vec<(f64, f64)>,
}

impl Accumulator {
    fn usage(&mut self, usage: Usage) {
        self.metrics.model_calls += 1;
        self.metrics.tokens.input += usage.input;
        self.metrics.tokens.output += usage.output;
        self.metrics.tokens.cache_read += usage.cache_read;
        self.metrics.tokens.cache_write += usage.cache_write;
    }

    fn call(&mut self, id: String, name: String) {
        *self.metrics.tool_calls.entry(name.clone()).or_default() += 1;
        self.names.insert(id, name);
    }

    fn end(&mut self, id: &str, at: f64, error: Option<bool>) {
        if let Some(start) = self.starts.remove(id) {
            if at >= start {
                self.spans.push((start, at));
            } else {
                self.metrics.incomplete_tool_spans += 1;
            }
        } else {
            self.metrics.incomplete_tool_spans += 1;
        }
        if self
            .names
            .get(id)
            .is_some_and(|name| matches!(name.as_str(), "edit" | "hashline"))
            && let Some(error) = error
        {
            self.metrics.edit_results += 1;
            self.metrics.edit_failures += u64::from(error);
        }
    }

    fn finish(mut self, wall_ms: f64) -> Metrics {
        self.metrics.incomplete_tool_spans += self.starts.len();
        self.metrics.tool_busy_ms = span_union(&mut self.spans).min(wall_ms).max(0.0);
        self.metrics.model_ms = (wall_ms - self.metrics.tool_busy_ms).max(0.0);
        self.metrics
    }
}

pub(crate) fn span_union(spans: &mut [(f64, f64)]) -> f64 {
    spans.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = 0.0;
    let mut current: Option<(f64, f64)> = None;
    for &(start, end) in spans.iter() {
        if !start.is_finite() || !end.is_finite() || end < start {
            continue;
        }
        match current {
            Some((left, right)) if start <= right => current = Some((left, right.max(end))),
            Some((left, right)) => {
                total += right - left;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((left, right)) = current {
        total += right - left;
    }
    total
}

pub(crate) fn parse_xi(sessions: &Path, wall_ms: f64) -> Result<Metrics> {
    let mut acc = Accumulator::default();
    let mut roots = 0;
    for entry in fs::read_dir(sessions)? {
        let entry = entry?;
        if !entry.file_type()?.is_file()
            || !entry.path().extension().is_some_and(|ext| ext == "jsonl")
        {
            continue;
        }
        let calls_before = acc.metrics.model_calls;
        let root = read_xi(BufReader::new(File::open(entry.path())?), &mut acc)
            .map_err(|error| format!("{}: {error}", entry.path().display()))?;
        if root {
            roots += 1;
        } else {
            acc.metrics.subagent_model_calls += acc.metrics.model_calls - calls_before;
        }
        // Provider call ids are session-local. Keep spans on the common clock,
        // but never match a child's result against another session's start.
        acc.metrics.incomplete_tool_spans += acc.starts.len();
        acc.starts.clear();
        acc.names.clear();
    }
    if roots != 1 {
        return Err(format!("expected one root xi session, found {roots}").into());
    }
    Ok(acc.finish(wall_ms))
}

fn read_xi(reader: impl BufRead, acc: &mut Accumulator) -> Result<bool> {
    let mut root = None;
    for (index, line) in reader.lines().enumerate() {
        let line = line?;
        let event: Event = serde_json::from_str(&line)
            .map_err(|e| format!("xi session line {}: {e}", index + 1))?;
        let at = event.at.0 as f64;
        match event.kind {
            EventKind::SessionStarted { parent_session, .. } => {
                if index != 0 || root.replace(parent_session.is_none()).is_some() {
                    return Err("session_started must occur exactly once, on line 1".into());
                }
            }
            EventKind::AssistantMessage { content, usage, .. } => {
                acc.usage(usage);
                for block in content {
                    if let Block::ToolUse { id, name, .. } = block {
                        acc.call(id.to_string(), name);
                    }
                }
            }
            EventKind::Compaction { usage, .. } => {
                acc.metrics.compactions += 1;
                if let Some(usage) = usage {
                    acc.usage(usage);
                }
            }
            EventKind::ToolDecision {
                call,
                during,
                extension,
                ..
            } if during.is_none() && extension.is_none() && acc.names.contains_key(&call.0) => {
                acc.starts.insert(call.to_string(), at);
            }
            EventKind::ToolResult { call, output, .. } => {
                acc.end(&call.to_string(), at, Some(output.is_error))
            }
            EventKind::TurnFailed { .. } => acc.metrics.turn_failures += 1,
            _ => {}
        }
    }
    root.ok_or_else(|| "missing session_started event".into())
}

/// Parses timestamped `--mode json` stdout from omp or pi; omp kept pi's
/// event stream.
pub(crate) fn parse_omp(reader: impl BufRead, wall_ms: f64) -> Result<Metrics> {
    let mut acc = Accumulator::default();
    for (index, line) in reader.lines().enumerate() {
        let timed: TimedLine = serde_json::from_str(&line?)
            .map_err(|e| format!("stdout capture line {}: {e}", index + 1))?;
        let Ok(event) = serde_json::from_str::<Value>(&timed.line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("message_end") if event["message"]["role"] == "assistant" => {
                acc.metrics.model_calls += 1;
                let usage = &event["message"]["usage"];
                acc.metrics.tokens.input += usage["input"].as_u64().unwrap_or(0);
                acc.metrics.tokens.output += usage["output"].as_u64().unwrap_or(0);
                acc.metrics.tokens.cache_read += usage["cacheRead"].as_u64().unwrap_or(0);
                acc.metrics.tokens.cache_write += usage["cacheWrite"].as_u64().unwrap_or(0);
                if matches!(
                    event["message"]["stopReason"].as_str(),
                    Some("error" | "aborted")
                ) {
                    acc.metrics.turn_failures += 1;
                }
            }
            Some("tool_execution_start") => {
                let id = event["toolCallId"]
                    .as_str()
                    .ok_or("omp tool start has no call id")?;
                let name = event["toolName"]
                    .as_str()
                    .ok_or("omp tool start has no name")?;
                acc.call(id.to_owned(), name.to_owned());
                acc.starts.insert(id.to_owned(), timed.at_ms);
            }
            Some("tool_execution_end") => {
                let id = event["toolCallId"]
                    .as_str()
                    .ok_or("omp tool end has no call id")?;
                let error = event["isError"]
                    .as_bool()
                    .or_else(|| event["result"]["isError"].as_bool());
                acc.end(id, timed.at_ms, error);
            }
            _ => {}
        }
    }
    Ok(acc.finish(wall_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_merges_overlaps_nested_and_touching_spans() {
        assert_eq!(
            span_union(&mut [
                (8.0, 12.0),
                (1.0, 5.0),
                (2.0, 3.0),
                (5.0, 9.0),
                (15.0, 16.0),
                (20.0, 19.0)
            ]),
            12.0
        );
        assert_eq!(span_union(&mut []), 0.0);
    }

    #[test]
    fn xi_protocol_fixture_accounts_usage_overlap_and_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("root.jsonl"),
            include_bytes!("../tests/fixtures/xi-session.jsonl"),
        )
        .unwrap();
        let metrics = parse_xi(dir.path(), 1000.0).unwrap();
        assert_eq!(metrics.model_calls, 2);
        assert_eq!(metrics.tokens.input, 103);
        assert_eq!(metrics.tokens.output, 24);
        assert_eq!(metrics.tokens.cache_read, 50);
        assert_eq!(metrics.tokens.cache_write, 10);
        assert_eq!(metrics.tool_calls["edit"], 1);
        assert_eq!(metrics.tool_calls["read"], 1);
        assert_eq!(metrics.tool_busy_ms, 300.0);
        assert_eq!(metrics.model_ms, 700.0);
        assert_eq!(metrics.turn_failures, 1);
        assert_eq!((metrics.edit_results, metrics.edit_failures), (1, 1));
        assert_eq!(metrics.incomplete_tool_spans, 0);
    }

    #[test]
    fn egress_decisions_do_not_count_calls_or_open_spans() {
        let fixture = include_str!("../tests/fixtures/xi-session.jsonl");
        let mut acc = Accumulator::default();
        read_xi(fixture.as_bytes(), &mut acc).unwrap();
        for (during, extension) in [(Some("a"), None), (None, Some("fetch")), (None, None)] {
            let event = Event {
                id: xi_protocol::EventId::generate(),
                parent: None,
                at: xi_protocol::Timestamp(1700),
                kind: EventKind::ToolDecision {
                    call: "connection".into(),
                    action: "net example.org:443".into(),
                    decision: xi_protocol::event::Decision::Ask,
                    rule: None,
                    taint: false,
                    during: during.map(Into::into),
                    extension: extension.map(Into::into),
                },
            };
            let line = format!("{fixture}{}\n", serde_json::to_string(&event).unwrap());
            let mut with_egress = Accumulator::default();
            read_xi(line.as_bytes(), &mut with_egress).unwrap();
            let metrics = with_egress.finish(1000.0);
            assert_eq!(metrics.tool_calls, acc.metrics.tool_calls);
            assert_eq!(metrics.incomplete_tool_spans, 0);
            assert_eq!(metrics.tool_busy_ms, 300.0);
        }
    }

    #[test]
    fn corrupted_xi_log_is_not_silent_zero_usage() {
        assert!(
            read_xi(b"{broken}\n".as_slice(), &mut Accumulator::default())
                .unwrap_err()
                .to_string()
                .contains("line 1")
        );
    }

    #[test]
    fn session_tree_aggregates_children_compactions_and_overlapping_spans() {
        let dir = tempfile::tempdir().unwrap();
        for (name, fixture) in [
            ("root", include_str!("../tests/fixtures/xi-tree/root.jsonl")),
            (
                "child-a",
                include_str!("../tests/fixtures/xi-tree/child-a.jsonl"),
            ),
            (
                "child-b",
                include_str!("../tests/fixtures/xi-tree/child-b.jsonl"),
            ),
        ] {
            fs::write(dir.path().join(format!("{name}.jsonl")), fixture).unwrap();
        }
        let metrics = parse_xi(dir.path(), 1000.0).unwrap();
        assert_eq!(metrics.model_calls, 5);
        assert_eq!(metrics.subagent_model_calls, 2);
        assert_eq!(metrics.compactions, 2);
        assert_eq!((metrics.tokens.input, metrics.tokens.output), (166, 37));
        assert_eq!(
            (metrics.tokens.cache_read, metrics.tokens.cache_write),
            (58, 13)
        );
        assert_eq!(metrics.tool_busy_ms, 400.0);
        assert_eq!(metrics.model_ms, 600.0);
        assert_eq!(metrics.incomplete_tool_spans, 0);
        assert_eq!((metrics.edit_results, metrics.edit_failures), (1, 1));
        assert_eq!(metrics.tool_calls["task"], 1);
        assert_eq!(metrics.tool_calls["read"], 1);
        assert_eq!(metrics.tool_calls["hashline"], 1);
    }

    #[test]
    fn missing_or_multiple_root_sessions_fail_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(parse_xi(dir.path(), 100.0).is_err());
        let fixture = include_str!("../tests/fixtures/xi-tree/root.jsonl");
        fs::write(dir.path().join("a.jsonl"), fixture).unwrap();
        fs::write(dir.path().join("b.jsonl"), fixture).unwrap();
        assert!(
            parse_xi(dir.path(), 100.0)
                .unwrap_err()
                .to_string()
                .contains("found 2")
        );
    }

    #[test]
    fn omp_failure_and_missing_span_are_visible() {
        let lines = [
            serde_json::json!({"at_ms": 10, "line": r#"{"type":"tool_execution_start","toolCallId":"a","toolName":"edit"}"#}),
            serde_json::json!({"at_ms": 20, "line": r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","usage":{"input":3,"output":1,"cacheRead":5,"cacheWrite":2}}}"#}),
        ].map(|line| line.to_string()).join("\n");
        let metrics = parse_omp(lines.as_bytes(), 100.0).unwrap();
        assert_eq!(metrics.turn_failures, 1);
        assert_eq!(metrics.incomplete_tool_spans, 1);
        assert_eq!(
            (
                metrics.tokens.input,
                metrics.tokens.output,
                metrics.tokens.cache_read,
                metrics.tokens.cache_write
            ),
            (3, 1, 5, 2)
        );
        assert_eq!(metrics.edit_results, 0);
    }

    #[test]
    fn captured_omp_smoke_accounts_real_usage_and_tool_spans() {
        let metrics = parse_omp(
            include_bytes!("../tests/fixtures/omp-live.jsonl").as_slice(),
            18_933.463_334,
        )
        .unwrap();
        assert_eq!(metrics.model_calls, 5);
        assert_eq!(
            (metrics.tokens.input, metrics.tokens.output),
            (11_531, 1_499)
        );
        assert_eq!(
            (metrics.tokens.cache_read, metrics.tokens.cache_write),
            (42_588, 0)
        );
        assert_eq!(
            metrics.tool_calls,
            BTreeMap::from([("bash".into(), 1), ("read".into(), 4), ("write".into(), 2)])
        );
        assert!((metrics.tool_busy_ms - 134.652_834).abs() < 0.001);
        assert!((metrics.model_ms - 18_798.810_5).abs() < 0.001);
        assert_eq!(metrics.turn_failures, 0);
        assert_eq!(metrics.incomplete_tool_spans, 0);
    }

    #[test]
    fn omp_edit_error_flags_are_not_assumed_when_absent() {
        let events = [
            serde_json::json!({"type":"tool_execution_start", "toolCallId":"a", "toolName":"edit"}),
            serde_json::json!({"type":"tool_execution_end", "toolCallId":"a", "isError":true}),
            serde_json::json!({"type":"tool_execution_start", "toolCallId":"b", "toolName":"hashline"}),
            serde_json::json!({"type":"tool_execution_end", "toolCallId":"b"}),
        ];
        let lines = events
            .iter()
            .enumerate()
            .map(|(at, event)| {
                serde_json::to_string(&TimedLine {
                    at_ms: at as f64,
                    line: event.to_string(),
                })
                .unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let metrics = parse_omp(lines.as_bytes(), 10.0).unwrap();
        assert_eq!((metrics.edit_results, metrics.edit_failures), (1, 1));
        assert_eq!(metrics.tool_busy_ms, 2.0);
    }
}
