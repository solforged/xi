use std::fmt::Write as _;

use globset::{Glob, GlobMatcher};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder, sinks::UTF8};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::Value;
use xi_protocol::ToolCallId;

use crate::{Context, Ran, files, output, parse, paths};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pattern: Option<String>,
    #[serde(default = "dot")]
    pub(crate) path: String,
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default = "limit")]
    limit: usize,
}
fn dot() -> String {
    ".".into()
}
fn limit() -> usize {
    200
}
impl Input {
    fn matcher(&self) -> Result<Option<RegexMatcher>, String> {
        self.pattern
            .as_ref()
            .map(|pattern| {
                RegexMatcherBuilder::new()
                    .case_insensitive(self.ignore_case)
                    .build(pattern)
                    .map_err(|error| format!("Invalid regex: {error}"))
            })
            .transpose()
    }
    fn glob(&self) -> Result<Option<GlobMatcher>, String> {
        self.glob
            .as_ref()
            .map(|glob| {
                Glob::new(glob)
                    .map(|g| g.compile_matcher())
                    .map_err(|error| format!("Invalid glob: {error}"))
            })
            .transpose()
    }
}
pub(crate) fn input(input: &Value) -> Result<Input, String> {
    let input: Input = parse(input)?;
    if input.pattern.is_none() && input.glob.is_none() {
        return Err("glob is required when pattern is absent".into());
    }
    if input.limit == 0 {
        return Err("limit must be positive".into());
    }
    input.matcher()?;
    input.glob()?;
    Ok(input)
}

pub(crate) fn run(
    call: &ToolCallId,
    value: &Value,
    ctx: &Context,
    anchors: bool,
) -> Result<Ran, String> {
    // Tool::run has already validated the arguments before dispatch.
    let input: Input = parse(value)?;
    let root = paths::resolve(&input.path, ctx)?;
    let matcher = input.matcher()?;
    let glob = input.glob()?;
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .bom_sniffing(false)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .build();
    let mut builder = WalkBuilder::new(&root);
    builder
        .follow_links(false)
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    let mut out = String::new();
    let mut count = 0;
    let mut truncated = false;
    for entry in builder.build() {
        if ctx.cancel.is_cancelled() {
            return Err("Search cancelled".into());
        }
        let entry = entry.map_err(|error| error.to_string())?;
        if !entry.file_type().is_some_and(|ty| ty.is_file()) {
            continue;
        }
        let path = entry.path();
        let relative = paths::relative(path, &ctx.workspace);
        let within_root = path.strip_prefix(&root).unwrap_or(path);
        if glob
            .as_ref()
            .is_some_and(|glob| !glob.is_match(&relative) && !glob.is_match(within_root))
        {
            continue;
        }
        if let Some(matcher) = &matcher {
            let mut heading = false;
            searcher
                .search_path(
                    matcher,
                    path,
                    UTF8(|number, line| {
                        if count == input.limit {
                            truncated = true;
                            return Ok(false);
                        }
                        if !heading {
                            writeln!(out, "{}", relative.display()).expect("writing String");
                            heading = true;
                        }
                        let line = if let Some(line) = line.strip_suffix('\n') {
                            line.strip_suffix('\r').unwrap_or(line)
                        } else {
                            line
                        };
                        files::numbered(&mut out, number as usize, line, anchors);
                        count += 1;
                        Ok(true)
                    }),
                )
                .map_err(|error| format!("Cannot search {}: {error}", relative.display()))?;
        } else {
            if count == input.limit {
                truncated = true;
                break;
            }
            writeln!(out, "{}", relative.display()).expect("writing String");
            count += 1;
        }
        if truncated {
            break;
        }
    }
    if truncated {
        writeln!(
            out,
            "[Truncated after {} {}; narrow path, glob, or pattern, or raise limit.]",
            input.limit,
            if matcher.is_some() {
                "matching lines"
            } else {
                "files"
            }
        )
        .expect("writing String");
    }
    Ok(output::text(call, out, false, ctx))
}
