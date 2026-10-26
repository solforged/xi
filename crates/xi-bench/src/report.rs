use std::{collections::BTreeMap, fmt::Write, fs, path::Path};

use crate::{Result, runner::Record};

pub(crate) fn load(dir: &Path) -> Result<Vec<Record>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path().join("record.json");
        if path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    let records = paths
        .iter()
        .map(|path| Ok(serde_json::from_slice(&fs::read(path)?)?))
        .collect::<Result<Vec<_>>>()?;
    if records.is_empty() {
        return Err("no run records found".into());
    }
    Ok(records)
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable_by(f64::total_cmp);
    let mid = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    })
}

#[derive(Debug)]
struct Summary {
    runs: usize,
    passes: usize,
    wall_ms: f64,
    model_calls: Option<f64>,
    tokens: [Option<f64>; 4],
    tools: Option<f64>,
    edits: u64,
    edit_failures: u64,
    measured: usize,
}

fn summarize(records: &[&Record]) -> Summary {
    let metrics: Vec<_> = records
        .iter()
        .filter_map(|record| record.metrics.as_ref())
        .collect();
    Summary {
        runs: records.len(),
        passes: records.iter().filter(|record| record.pass).count(),
        wall_ms: median(records.iter().map(|record| record.agent.wall_ms).collect()).unwrap_or(0.0),
        model_calls: median(metrics.iter().map(|m| m.model_calls as f64).collect()),
        tokens: [
            median(metrics.iter().map(|m| m.tokens.input as f64).collect()),
            median(metrics.iter().map(|m| m.tokens.output as f64).collect()),
            median(metrics.iter().map(|m| m.tokens.cache_read as f64).collect()),
            median(
                metrics
                    .iter()
                    .map(|m| m.tokens.cache_write as f64)
                    .collect(),
            ),
        ],
        tools: median(
            metrics
                .iter()
                .map(|m| m.tool_calls.values().sum::<u64>() as f64)
                .collect(),
        ),
        edits: metrics.iter().map(|m| m.edit_results).sum(),
        edit_failures: metrics.iter().map(|m| m.edit_failures).sum(),
        measured: metrics.len(),
    }
}

fn number(value: Option<f64>) -> String {
    match value {
        Some(n) if n.fract() == 0.0 => format!("{n:.0}"),
        Some(n) => format!("{n:.1}"),
        None => "n/a".into(),
    }
}

fn cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

fn table(output: &mut String, records: &[&Record]) {
    output.push_str("| Arm | Model | Pass rate | Median wall | Model calls | Tokens (in/out/cache read/cache write) | Tool calls | Edit failure rate | Metrics |\n|---|---|---:|---:|---:|---:|---:|---:|---:|\n");
    let mut groups: BTreeMap<(&str, &str), Vec<&Record>> = BTreeMap::new();
    for record in records {
        groups
            .entry((&record.arm, &record.model))
            .or_default()
            .push(record);
    }
    for ((arm, model), records) in groups {
        let s = summarize(&records);
        let edit_rate = if s.edits == 0 {
            "n/a".into()
        } else {
            format!(
                "{:.1}% ({}/{})",
                100.0 * s.edit_failures as f64 / s.edits as f64,
                s.edit_failures,
                s.edits
            )
        };
        writeln!(
            output,
            "| {} | {} | {}/{} ({:.1}%) | {:.2}s | {} | {} | {} | {} | {}/{} |",
            cell(arm),
            cell(model),
            s.passes,
            s.runs,
            100.0 * s.passes as f64 / s.runs as f64,
            s.wall_ms / 1000.0,
            number(s.model_calls),
            s.tokens.map(number).join(" / "),
            number(s.tools),
            edit_rate,
            s.measured,
            s.runs
        )
        .expect("writing a String cannot fail");
    }
}

pub(crate) fn render(records: &[Record]) -> String {
    let mut output = String::from("# Benchmark report\n\n## By arm\n\n");
    table(&mut output, &records.iter().collect::<Vec<_>>());
    output.push_str("\n## By task\n");
    let mut tasks: BTreeMap<&str, Vec<&Record>> = BTreeMap::new();
    for record in records {
        tasks.entry(&record.task).or_default().push(record);
    }
    for (task, records) in tasks {
        write!(output, "\n### {}\n\n", cell(task)).expect("writing a String cannot fail");
        table(&mut output, &records);
    }
    output.push_str("\nCounts and token columns are per-run medians. Edit failure rates pool observed edit/replace results. Missing event metrics are excluded, not counted as zero. Model time is wall time minus the union of completed tool spans; it also includes harness overhead.\n");
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Kind,
        metrics::{Metrics, Tokens},
        runner::Outcome,
    };

    fn record(wall_ms: f64, calls: u64, pass: bool) -> Record {
        Record {
            schema_version: 1,
            arm: "test".into(),
            kind: Kind::Omp,
            task: "task".into(),
            tags: vec![],
            model: "model".into(),
            trial: 1,
            order: 0,
            argv: vec![],
            env: BTreeMap::new(),
            pass,
            protected_untouched: true,
            protected_status: String::new(),
            agent: Outcome {
                exit_code: Some(0),
                timed_out: false,
                wall_ms,
                error: None,
            },
            check: Outcome {
                exit_code: Some(0),
                timed_out: false,
                wall_ms: 1.0,
                error: None,
            },
            metrics: Some(Metrics {
                model_calls: calls,
                tokens: Tokens {
                    input: calls * 10,
                    ..Tokens::default()
                },
                tool_calls: BTreeMap::from([("read".into(), calls)]),
                edit_results: 2,
                edit_failures: 1,
                ..Metrics::default()
            }),
            metrics_error: None,
            diff_stat: String::new(),
        }
    }

    #[test]
    fn aggregation_uses_even_medians_and_pooled_failure_counts() {
        let a = record(1000.0, 2, true);
        let b = record(3000.0, 5, false);
        let s = summarize(&[&a, &b]);
        assert_eq!((s.runs, s.passes), (2, 1));
        assert_eq!(s.wall_ms, 2000.0);
        assert_eq!(s.model_calls, Some(3.5));
        assert_eq!(s.tokens[0], Some(35.0));
        assert_eq!(s.tools, Some(3.5));
        assert_eq!((s.edit_failures, s.edits), (2, 4));
        let rendered = render(&[a, b]);
        assert!(
            rendered.contains("1/2 (50.0%) | 2.00s | 3.5 | 35 / 0 / 0 / 0 | 3.5 | 50.0% (2/4)")
        );
        assert!(rendered.contains("### task"));
    }

    #[test]
    fn missing_metrics_do_not_bias_medians_to_zero() {
        let a = record(1000.0, 8, true);
        let mut b = record(3000.0, 0, false);
        b.metrics = None;
        let s = summarize(&[&a, &b]);
        assert_eq!(s.model_calls, Some(8.0));
        assert_eq!(s.measured, 1);
        assert_eq!(median(vec![4.0, 1.0, 9.0]), Some(4.0));
        assert_eq!(median(vec![]), None);
    }
}
