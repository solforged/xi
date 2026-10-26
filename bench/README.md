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

`verify` requires a normal nonzero check exit on the initial repository and a zero exit after the reference patch, with protected paths unchanged. Checks have a separate ten-second limit, including compilation. A timeout or spawn error is not accepted as proof of an initially failing test. The five starter tasks cover a multi-bug Python library, Rust interval union, a public-function rename, searching a policy registry, and case-sensitive URL normalization regressions. Reference fixes change one or two production files.

The harder tasks add these workloads:

| Task | Workload | Repository size | Material |
|---|---|---:|---:|
| `invoice-rounding` | Python feature through config, model, CSV parser, exact aggregation, JSON and CLI. Covers override precedence, grouping, negative ties and hostile Decimal contexts. | 10 files, 6 changed by reference | About 2k tokens |
| `revision-store` | Dependency-free Rust store with one revision invariant across transactions, snapshots, prefix scans, watches and compaction. Includes atomic failures and a deterministic historical-state oracle. | 11 files, 6 changed by reference | About 2.8k tokens |
| `parallel-kernels` | Five independent Python packages: incremental CSV, interval overlays, dependency waves, JSON pointers and bounded retry scheduling. Each has separate failing tests. | 13 files, 5 changed by reference | About 2.3k tokens |
| `long-context-catalog` | Reconcile 256 policies from 16 original charters and 16 matching two-revision memoranda. Every policy is checked, including field rescissions and dependent arithmetic. | 38 files, 2 changed by reference | 101,875 Qwen tokens of normative prose |

The long-context count comes from the configured Qwen server's tokenizer, without special tokens. The prose is 547,203 characters; every charter and memorandum supplies tested facts. It fits a 262k-token window before the conversation and generated catalog. Test digests do not repeat the source facts, and the checker loads the resulting module and catalog in a standalone directory. There are no network, package-install, clock, or random-seed dependencies in the checks. Rust checks use `cargo test --offline`; generated histories use fixed seeds. Every task protects its tests, specifications and source material where applicable. An all-task `verify` run checks old and new tasks together on both macOS and Linux.

## Arms

`bench/arms.toml` has one `[[arm]]` entry per variant: unique `name`, `kind` (`xi`, `omp` or `pi`), `argv`, and optional `[arm.env]`. Argument and environment-value templates support `{prompt}`, `{workspace}`, `{session_dir}`, `{model}`, and `{arms_dir}`. The last is the absolute directory containing the arms file, not the temporary workspace. Expansion happens once, without a shell; placeholders inside the prompt remain literal. The environment otherwise inherits the invoking shell. Each command receives `PYTHONDONTWRITEBYTECODE=1` and a workspace-local `CARGO_TARGET_DIR`.

The original `xi`, `xi-replace`, `omp`, and `pi` arms remain unchanged. Two additional arms opt into features:

- `xi-compact` adds `--config {arms_dir}/configs/compact.toml`. That file enables automatic compaction with `trigger_tokens = 24000` and `keep_recent_tokens = 8000`. Automatic compaction is off by default. The kernel's default recent retention is 20,000 tokens, clamped to at most half the effective trigger; this arm explicitly uses 8,000. Compare pass rate, total usage, and compaction count on the long-context workload.
- `xi-subagents` adds `--extension {arms_dir}/../extensions/subagents`. Its `task` tool accepts `{"tasks":[{"prompt":"...","tools":["read","edit","write","bash","search"]}]}` with 1 to 8 items. Tools are optional; the listed built-ins are the default set. Use self-contained prompts and disjoint file scopes. Children run concurrently, cannot delegate again, and log `parent_session` in the parent's session directory. Compare against `xi` on `parallel-kernels`.

The runner writes `{session_dir}/omp.yml` with `memory.backend: off` for omp arms. The shipped omp arm disables extensions, skills, rules, title generation, and approval prompts, but retains the user's configured model/provider registry. `omp --help` v18.12.6 names the approval option `--approval-mode yolo`; this is the current equivalent of older `--yolo` invocations.

The `pi` arm runs upstream pi (`pi` 1.4.0 here) with extensions, skills, prompt templates, context files, MCP and startup network activity disabled. Pi has no approval prompts. Its model must exist in pi's own registry (`pi --list-models`); the amarna llama.cpp server is not configured there, so a smoke used `openai/gpt-6-luna`. omp and pi share the `--mode json` event stream, so both use the same parser.

An arm may execute arbitrary commands with the invoking user's authority. The temporary workspace is an isolation mechanism for task state, not a security sandbox. Do not use untrusted tasks or arms. The agent's stdin is closed, and on timeout or exit its process group is terminated to stop background children. A deliberately detached process can escape the process group.

## Output and metrics

Each task/arm/trial gets a new temporary repository with an unsigned, hook-skipping baseline commit. Odd trials use the selected arm order; even trials reverse it. The runner refuses to overwrite an existing run directory. Choose a fresh `--out` directory for a new experiment.

Each run directory contains `record.json`, raw agent/check stdout and stderr, timestamped stdout JSONL (`at_ms` is monotonic elapsed milliseconds), `prompt.txt`, the harness `sessions/`, `changes.patch`, and `git-status.txt`. Temporary task workspaces are removed when the run ends. New untracked files appear in the status log but are not included in Git's diff/stat or the patch. Keep output private if prompts, provider responses, or per-arm environment values contain secrets.

A pass requires the check to pass, all protected paths to remain unchanged, a successful agent exit without timeout, readable event metrics, and no recorded turn failures. Both `git status --porcelain -- <paths>` and a diff against the original commit inspect protected paths, so an agent commit cannot hide a test edit. This is intended to catch accidental benchmark contamination, not adversarial Git manipulation. A task failure still produces a record and does not prevent the remaining arms from running. Setup/filesystem/Git errors stop the run.

For xi, metrics read every `*.jsonl` file in the run's session directory, deserialized as `xi_protocol::Event`. Exactly one session must have no `parent_session`; it is the root. Root and child assistant messages all contribute model calls and usage. A `compaction` event increments the compaction count; when it has `usage`, it also contributes one model call and those tokens. Hook-supplied summaries without usage count as compactions but not model calls. Calls made in child sessions are also reported in the subagent model calls column. Tool ids are scoped to their session, while completed spans from all sessions share one clock and one union. Tool-use blocks supply names; each `tool_decision.at` starts a span closed by the matching `tool_result.at`. For omp, timestamped stdout supplies assistant `message_end` usage, `tool_execution_start`/`tool_execution_end` spans, and error/aborted assistant stop reasons. Unfinished or unmatched spans are counted separately.

Tool busy time is the union of spans, not their sum. Model time is agent wall time minus busy time, and therefore also includes startup, scheduling, and other harness overhead. Input, output, cache-read, and cache-write token counts are retained separately; input excludes cached input according to each harness's usage schema. Edit-tool failures count observable `edit` and `replace` results, including policy denials. Missing error flags are not assumed successful.

Reports group by arm and model, then show each task separately. Wall time, model calls, subagent model calls, compaction count, each token category, and total tool calls are per-run medians. Pass rates include all recorded runs. Edit failure rates pool observable edit results across runs; no observed results produce `n/a`. Failed metric parses remain explicit in each record and are excluded from metric medians, never replaced with zeros. The Metrics column shows the usable record count. A one-trial smoke proves plumbing, not comparative performance.

## Parser fixtures

The xi fixture in `crates/xi-bench/tests/fixtures/xi-session.jsonl` is hand-written against the protocol and includes overlapping tools and an edit failure. The hand-written `xi-tree/` fixture adds a parent, two children reusing the same tool-call id, overlapping parent/child spans, a paid compaction and a hook-supplied compaction. It proves aggregate usage without double-counting busy time or matching tool ids across sessions. The omp fixture is a filtered capture of the live starter-task smoke: assistant message ends and tool start/end records retain their original payloads and measured timestamps. Stream deltas and unrelated lifecycle records are omitted. Tests also cover union accounting, aggregation, missing spans, subprocess timeout cleanup, and template expansion.

## Hard-task calibration

One trial per task and arm, model `amarna/qwen3.8-27b-256k`, on macOS. The workspace was built with `cargo build --workspace`, and `target/debug` was prepended to `PATH`. Raw records are in `bench/results/queue-calibration/` on the calibration worktree. The command was:

```sh
PATH="$PWD/target/debug:$PATH" cargo run -p xi-bench -- run \
  --arms bench/arms.toml --arm xi omp \
  --task invoice-rounding revision-store parallel-kernels long-context-catalog \
  --model amarna/qwen3.8-27b-256k --trials 1 \
  --out bench/results/queue-calibration
```

| Task | xi | xi calls | xi wall | omp | omp calls | omp wall |
|---|---|---:|---:|---|---:|---:|
| `invoice-rounding` | Pass | 7 | 264.63s | Pass | 15 | 390.02s |
| `revision-store` | Pass | 6 | 134.44s | Pass | 6 | 253.04s |
| `parallel-kernels` | Fail | 4 | 473.23s | Pass | 11 | 537.59s |
| `long-context-catalog` | Pass | 52 | 473.18s | Transport failure | 17 | 333.44s |

The xi parallel run exhausted its 16,384-token response limit while reasoning, made no edits, and left the tests failing. The omp catalog run ended with `ECONNRESET`, made no tracked changes, and left the tests failing. That transport failure is not evidence of task difficulty. Both failures preserved protected paths. The successful tasks required substantive code or catalog generation rather than the starter suite's short fixes; one trial is not enough to rank harnesses.

The xi catalog run used scripted prose extraction rather than reading all 101,875 source tokens into model context. Its largest request still contained 39,732 input tokens including cache reads; 30 of 52 requests exceeded the 24,000-token compaction trigger, and the mean was 26,399. Neither calibration arm enables xi compaction, so these runs do not prove the number of compactions or post-compaction success. Use `xi-compact` to measure those after integration. Source volume alone does not force a particular reading strategy.

All nine tasks passed `xi-bench verify` on macOS and amarna Linux: each initial check failed normally, each reference patch passed, and protected paths stayed unchanged.
