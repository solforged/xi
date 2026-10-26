# Harness benchmarks

`xi-bench` compares harnesses and tool/model ablations on fresh copies of the same tasks. Commands below run from the repository root:

```sh
cargo run -p xi-bench -- verify
cargo run -p xi-bench -- run --arms bench/arms.toml --arm omp --task python-batches --model amarna/qwen3.8-27b-256k --trials 1 --out bench/results/smoke
cargo run -p xi-bench -- report bench/results/smoke
```

Install `xi` on `PATH` before selecting the xi arms. For a comparison, use `--arm xi xi-replace omp --trials 3` and omit `--task` to select the entire suite. Repeat `--arm` and `--task` or supply multiple names after each. `verify` accepts positional task names. Both `verify` and `run` accept `--tasks-dir`; its default is `bench/tasks`.

## Tasks

Each directory under `bench/tasks` contains:

- `task.toml`: `prompt`, `check` (an argv array, not a shell string), positive `timeout_seconds` for the agent, `protected_paths` (literal relative paths), and optional `tags`.
- `repo/`: the initial repository, without `.git`, symlinks, or external dependencies.
- `reference.patch`: a patch applied by `git apply` from the copied repository root.

`verify` requires a normal nonzero check exit on the initial repository and a zero exit after the reference patch, with protected paths unchanged. Checks have a separate ten-second limit, including compilation. A timeout or spawn error is not accepted as proof of an initially failing test. The five starter tasks cover a multi-bug Python library, Rust interval union, a public-function rename, searching a policy registry, and case-sensitive URL normalization regressions. Reference fixes change one or two production files. Every task protects its tests and tells the agent not to edit them.

## Arms

`bench/arms.toml` has one `[[arm]]` entry per variant: unique `name`, `kind` (`xi` or `omp`), `argv`, and optional `[arm.env]`. Argument and environment-value templates support `{prompt}`, `{workspace}`, `{session_dir}`, and `{model}`. Expansion happens once, without a shell; placeholders inside the prompt remain literal. The environment otherwise inherits the invoking shell. Each command receives `PYTHONDONTWRITEBYTECODE=1` and a workspace-local `CARGO_TARGET_DIR`.

The runner writes `{session_dir}/omp.yml` with `memory.backend: off` for omp arms. The shipped omp arm disables extensions, skills, rules, title generation, and approval prompts, but retains the user's configured model/provider registry. `omp --help` v18.12.6 names the approval option `--approval-mode yolo`; this is the current equivalent of older `--yolo` invocations.

An arm may execute arbitrary commands with the invoking user's authority. The temporary workspace is an isolation mechanism for task state, not a security sandbox. Do not use untrusted tasks or arms. The agent's stdin is closed, and on timeout or exit its process group is terminated to stop background children. A deliberately detached process can escape the process group.

## Output and metrics

Each task/arm/trial gets a new temporary repository with an unsigned, hook-skipping baseline commit. Odd trials use the selected arm order; even trials reverse it. The runner refuses to overwrite an existing run directory. Choose a fresh `--out` directory for a new experiment.

Each run directory contains `record.json`, raw agent/check stdout and stderr, timestamped stdout JSONL (`at_ms` is monotonic elapsed milliseconds), `prompt.txt`, the harness `sessions/`, `changes.patch`, and `git-status.txt`. Temporary task workspaces are removed when the run ends. New untracked files appear in the status log but are not included in Git's diff/stat or the patch. Keep output private if prompts, provider responses, or per-arm environment values contain secrets.

A pass requires the check to pass, all protected paths to remain unchanged, a successful agent exit without timeout, readable event metrics, and no recorded turn failures. Both `git status --porcelain -- <paths>` and a diff against the original commit inspect protected paths, so an agent commit cannot hide a test edit. This is intended to catch accidental benchmark contamination, not adversarial Git manipulation. A task failure still produces a record and does not prevent the remaining arms from running. Setup/filesystem/Git errors stop the run.

For xi, metrics are read from the single session JSONL, deserialized as `xi_protocol::Event`: assistant messages supply model-call and usage counts; tool-use blocks supply tool names; each `tool_decision.at` starts a span closed by the matching `tool_result.at`. For omp, timestamped stdout supplies assistant `message_end` usage, `tool_execution_start`/`tool_execution_end` spans, and error/aborted assistant stop reasons. Unfinished or unmatched spans are counted separately. Only completed spans contribute busy time.

Tool busy time is the union of spans, not their sum. Model time is agent wall time minus busy time, and therefore also includes startup, scheduling, and other harness overhead. Input, output, cache-read, and cache-write token counts are retained separately; input excludes cached input according to each harness's usage schema. Edit-tool failures count observable `edit` and `replace` results, including policy denials. Missing error flags are not assumed successful.

Reports group by arm and model, then show each task separately. Wall time, model calls, each token category, and total tool calls are per-run medians. Pass rates include all recorded runs. Edit failure rates pool observable edit results across runs; no observed results produce `n/a`. Failed metric parses remain explicit in each record and are excluded from metric medians, never replaced with zeros. The Metrics column shows the usable record count. A one-trial smoke proves plumbing, not comparative performance.

## Parser fixtures

The xi fixture in `crates/xi-bench/tests/fixtures/xi-session.jsonl` is hand-written against the protocol and includes overlapping tools and an edit failure. The omp fixture is a filtered capture of the live starter-task smoke: assistant message ends and tool start/end records retain their original payloads and measured timestamps. Stream deltas and unrelated lifecycle records are omitted. Tests also cover union accounting, aggregation, missing spans, subprocess timeout cleanup, and template expansion.
