# Roadmap

xi has a usable agent on Linux and macOS, an extension host with a TypeScript SDK, and a benchmark to measure features with. Features get added one at a time and stay in the default only when a measurement supports them. The oh-my-pi (omp) survey behind the queue is summarised in [What omp teaches](#what-omp-teaches).

One per-user daemon routes and supervises, with one worker per open session. Line mode and the TUI use the daemon; `xi -p`, `xi serve`, the benchmark and tests use isolated private instances. The daemon and worker split, including socket activation and scheduler leases, is implemented. See the [design](design.md#architecture) and completed [build order](design.md#build-order).

## Two tracks

Sandbox and feature work meet at the two interfaces below. The Platform queue tracks the remaining sandbox and infrastructure work.

| Track | Owns | Interface to the other track |
|---|---|---|
| Platform | `xi-sandbox`, `xi-policy`, workspace trust, egress proxy, secrets, daemon and worker lifecycle | `xi_sandbox::Spec` (what a process may touch) and `xi_tools::Crossing` (what a call asks policy for) |
| Features | tools, context, compaction, extensions, subagents, clients | A feature that needs new access declares a crossing or a spec field. It never changes the sandbox itself. |

A feature lands behind a toggle: a tool name for `--tools`, a config key, or an extension package. `xi-bench` then compares it against the baseline arm on the same tasks and model. A feature goes into the default set only when it pays for its tokens and time on the suite, or when safety needs it.

## Baseline

The arm everything is compared against: `read`, `edit` (exact replacement), `write`, `bash`, `search`; a static system prompt under 1k tokens; `AGENTS.md` files and a skills index. Automatic compaction off, no extensions.

## Feature queue

Rows 1 to 5 are done; their results are below.

| # | Feature | Home | Outcome or deciding measurement |
|---|---|---|---|
| 1 | Harder benchmark tasks | `bench/` | Done. Four tasks added; arms no longer pass everything. |
| 2 | Edit format | `xi-tools` | Done. Exact replacement is the default `edit`; hash anchors are the opt-in `hashline`. |
| 3 | Compaction | kernel | Done. Overflow recovery always on; automatic compaction stays opt-in. |
| 4 | Extension host and TypeScript SDK | kernel, `sdk/typescript` | Done. Gate for rows 5 to 9. |
| 5 | Subagents | `extensions/subagents` | Done as an optional package. Not used unprompted by Qwen 3.8. |
| 6 | LSP diagnostics after edits | package | Calls and time to green on the Rust and TypeScript tasks. |
| 7 | Todo or plan tool | package | Pass rate on multi-step tasks (`invoice-rounding`, `revision-store`), prompt size. |
| 8 | `fetch` and web search, with taint wired in | package plus platform egress proxy | Tasks that need docs; taint must turn later egress into `ask`. |
| 9 | Memory | package | Last: needs multi-session tasks to measure. |
| 10 | TUI | client | Daemon client with event replay and deduplication, streaming retry resets and a `/tree` checkout overlay. The surface for daily use is tracked in [Terminal client](#terminal-client). Not benchmarked. |

A response that stops at `max_tokens` without tool calls ends the turn; two `parallel-kernels` baseline failures ended that way, so continuing such a turn is worth measuring.

## Terminal client

The goal is a client good enough for daily work, including developing xi with xi. The target surface is in the [design](design.md#terminal-client). It lands in five slices.

| Slice | Contents | Protocol | Status |
|---|---|---|---|
| 1. Model and reasoning | Model picker, `shift+tab` reasoning cycle, `/reasoning` (also in line mode), reasoning in the footer | `session/models`, `session/set_reasoning`, `reasoning` in `SessionDetails` | Done |
| 2. Sessions and approvals | `/resume` picker, approvals inbox across sessions with `/approvals`, approval scopes for once, session and workspace | `remember` on `approval/resolve`; `taint` and `trusted` on pending asks; `started`, `updated` and `title` in `session/list` | Done |
| 3. Composer | Saved history with `ctrl+r`, `ctrl+g` external editor, `/` and `@` completion, extension commands and `/compact` in the TUI; `/help` built from the key dispatch table | `session/commands` | Done |
| 4. Rendering and keys | `edit` diffs, `ctrl+t` thinking display, `/copy`, `[keys]` and `[display]` in `tui.toml` | None | Done |
| 5. Settings | `/settings` view with each value's layer, `ctrl+s` to save the session's model or reasoning as the default, the `approvals` preset | `config/settings`, `config/set` | Done |

After these, in rough order of daily value: `!cmd` and `!!cmd` run in the session sandbox (needs `session/shell`), session rename and auto-naming, a large-paste menu, image paste, `alt+up` to pull queued prompts back, git branch and session cost in the footer, `/export`, and themes. Plan mode, todo, advisor, an agent hub and dashboards stay packages in the feature queue.

Self-hosting limits outside the client: tools have no network, so adding a crate or pushing happens in the user's shell until the egress proxy exists. Daily use runs the `mise run install` build; workers keep the binary they started with, so rebuilding the repository does not disturb running sessions.

## Platform queue

| Item | Why |
|---|---|
| Egress proxy and in-namespace forwarder | Tools and extensions have no network at all today; `fetch`, package installs and `git push` need it. Extensions that declare `net` refuse to start until it exists. |
| Harder tasks for frontier models | `gpt-6.1-sol` passes every hard task in every arm (36/36; see [Frontier model](#frontier-model)), so the suite can no longer rank harnesses or features on a frontier model. The Anthropic adapter passes mocked transcripts only; there is no API key on these machines. |
| Credentials from the keychain | Only env vars and 0600 files work now. |
| Extension package pinning | Packages load from configured directories; the content-hash lockfile is not built. |
| PID namespace (Linux) | Other processes' command lines are visible in `/proc`. |
| seccomp filter (Linux) | Reduces kernel attack surface (`io_uring`, `bpf`, `userfaultfd`). |
| Hooks in repos created by `git init` inside the sandbox | Their `.git/hooks` stays writable until the next tool call builds a new spec. |
| Git hook policy | Sandboxed git uses a session config with identity only, so global hooks (gitleaks) do not run inside. Decide whether repo hooks should run there. |

## What omp teaches

From a read-only survey of `~/src/oh-my-pi` (31 tools, a loop of about 115 KB in `packages/agent/src/agent-loop.ts`):

- Hard loop invariants, now in the xi kernel: steering never lands between a `tool_use` and its `tool_result`; a user abort keeps the steering queue; a dangling tool call gets a synthetic result before the next request.
- Removed features are the strongest signal. omp dropped its `librarian` and `designer` subagents, Ruby and Julia eval, git and jj wrappers, per-call model overrides and several hashline ops (`DEL`, `COPY`). Small, general tool vocabularies outlast specialised ones.
- Most of omp's surface is optional subsystems: subagents and the job hub, advisor (a second model reviewing turns), LSP and DAP, memory, stream-interrupt rules, eval kernels. In xi each of these is a package and a benchmark ablation, in the order of the feature queue.
- Sol's pi fork already ships subagents, web search and compaction as extensions (`~/src/pi-sol/extensions`). Its `workflows` extension informed the subagent package.

## Benchmark results

All runs on 2026-10-26 with `xi-bench` (see [bench/README.md](../bench/README.md)), three trials per arm and task unless noted. Models are local llama.cpp servers on amarna's RTX PRO 6000: Qwen 3.8 27B (Q8), Gemma 4 12B (Q8) and Ornith 1.5 35B-A3B (Q4), except in [Frontier model](#frontier-model). Counts and tokens are per-run medians. Raw records are in `~/xi-bench-2026-10-26` on amarna.

### Suite baseline

The four harder tasks, `xi` against `omp`, Qwen 3.8 on the single-slot 256k server, run after the edit cutover.

| Task | Arm | Pass | Median wall | Median calls | Median tokens (input / output / cache read) | Edit failures |
|---|---|---:|---:|---:|---:|---:|
| `invoice-rounding` | `xi` | 2/3 | 173 s | 7 | 5,165 / 7,807 / 19,861 | 0% (0/6) |
| `invoice-rounding` | `omp` | 2/3 | 230 s | 9 | 14,621 / 13,595 / 167,798 | 0% (0/3) |
| `long-context-catalog` | `xi` | 2/3 | 106 s | 18 | 33,652 / 9,869 / 271,027 | 0% (0/12) |
| `long-context-catalog` | `omp` | 2/3 | 444 s | 19 | 39,051 / 13,904 / 591,679 | 25% (2/8) |
| `parallel-kernels` | `xi` | 2/3 | 299 s | 33 | 16,100 / 27,058 / 280,727 | 0% (0/14) |
| `parallel-kernels` | `omp` | 2/3 | 826 s | 16 | 25,736 / 33,320 / 421,559 | 0% (0/7) |
| `revision-store` | `xi` | 2/3 | 61 s | 6 | 7,593 / 4,428 / 21,981 | 0% (0/2) |
| `revision-store` | `omp` | 2/3 | 246 s | 9 | 15,180 / 11,928 / 134,189 | 22% (2/9) |

- The harder tasks separate runs: both arms pass 8/12, against 15/15 on the starter suite.
- xi matches omp's pass rate at under half the wall time and a fraction of the cached prompt tokens on every task.
- Three trials per cell cannot rank the two harnesses on pass rate; that needs more trials or a frontier model.

### Frontier model

The four harder tasks on `gpt-6.1-sol` through a ChatGPT plan, at medium reasoning: xi at 9d71600 (`chatgpt/…`) and omp 18.12.6 (`openai-codex/…`), on hyperion. One runner per task ran concurrently on the same account; within a task the arms alternated order. Raw records are in `~/xi-bench-2026-10-26-gpt` on hyperion.

| Arm | Pass | Median wall | Median calls | Median tool calls | Median tokens (input / output / cache read) | Edit failures |
|---|---:|---:|---:|---:|---:|---:|
| `xi` | 12/12 | 76 s | 15.5 | 14.5 | 14,122 / 2,593 / 54,208 | 4% (2/52) |
| `xi-hashline` | 12/12 | 73 s | 17.5 | 16.5 | 16,858 / 2,512 / 58,432 | 0% (0/55) |
| `omp` | 12/12 | 103 s | 13 | 26 | 19,824 / 3,951 / 182,336 | 9% (2/23) |

| Task | `xi` wall | `xi-hashline` wall | `omp` wall |
|---|---:|---:|---:|
| `invoice-rounding` | 112 s | 100 s | 125 s |
| `long-context-catalog` | 98 s | 82 s | 81 s |
| `parallel-kernels` | 57 s | 71 s | 137 s |
| `revision-store` | 68 s | 61 s | 89 s |

- Every run passed. On a frontier model the hard suite measures cost and speed, not success.
- xi was faster than omp on three tasks and used about 70% fewer cache-read tokens and a third less output. omp was faster on `long-context-catalog`.
- omp made fewer model calls but nearly twice the tool calls. The ChatGPT backend's Lite request shape requires `parallel_tool_calls: false`, so xi gets one tool call per response there.
- `hashline` made no edit errors in 55 edits; exact replacement failed 2 of 52, both in one run where the model repeated a stale snippet. Anchors failed 23% to 93% of edits on the local models, and pass rate and time are equal here, so exact replacement stays the default.
- Compaction and subagent arms were not run on this model.

### Edit format

Hash-anchored edits (now `hashline`) against exact replacement (now the default `edit`). The starter suite is the five original tasks; the hard pair is `invoice-rounding` and `revision-store`.

| Model | Tasks | Anchors: pass | Anchors: edit failures | Replace: pass | Replace: edit failures | Median calls (anchors / replace) |
|---|---|---:|---:|---:|---:|---:|
| Qwen 3.8 27B | starter | 15/15 | 25% (8/32) | 15/15 | 3% (1/37) | 6 / 6 |
| Qwen 3.8 27B | hard pair | 5/6 | 23% (6/26) | 6/6 | 0% (0/14) | 15.5 / 10 |
| Gemma 4 12B | starter | 8/15 | 93% (131/141) | 9/15 | 0% (0/16) | 16 / 9 |
| Ornith 1.5 35B | starter | 15/15 | 73% (56/77) | 14/15 | 3% (1/34) | 16 / 9 |
| Ornith 1.5 35B | hard pair | 4/6 | 91% (32/35) | 4/6 | 32% (9/28) | 44.5 / 45.5 |

- Anchors failed far more on every model. The failed calls are format mistakes, not stale views: models pass a bare line number (`"4"`), the line's text, or a whole `LINE:HH|text` row with the wrong hash.
- Replacement matched or beat anchors on pass rate everywhere except one Ornith starter trial, and needed fewer calls on the smaller models.
- Decision: `edit` is now exact replacement, and `read` and `search` print `LINE|text`. `hashline` keeps the anchored editor, and turns `LINE:HH|text` back on, for a frontier-model comparison later.
- The Gemma hard-pair run is not reported: two copies of it ran into the same output directory by mistake.

### Compaction

`long-context-catalog`, Qwen 3.8. The `xi-compact` arm compacts at 24,000 tokens and keeps 8,000 recent tokens.

| Arm | Pass | Median wall | Median calls | Median tokens (input / output / cache read) | Compactions | Peak request per run |
|---|---:|---:|---:|---:|---:|---|
| `xi` | 3/3 | 118 s | 19 | 29,071 / 10,945 / 335,019 | 0 | 55.5k, 29.0k, 25.1k |
| `xi-compact` | 3/3 | 259 s | 55 | 89,363 / 19,283 / 768,609 | 2 | 23.8k, 22.8k, 23.2k |

- Compaction works: it kept every request under the trigger and every trial still passed.
- It cost a lot when it fired early: calls nearly tripled and tokens more than doubled. The extra calls are more reads, edits and shell runs on the model's own extraction script after each summary, consistent with the summary dropping working detail.
- Decision: automatic compaction stays off. A context-overflow error now always triggers one compaction and a retry, enabled or not, since that request has already failed. Revisit the default with a task that exceeds the context window.

### Subagents

`parallel-kernels` (five independent packages), Qwen 3.8 Q8 served with four parallel slots of 64k context each, so children can run at the same time. All three arms used this server.

| Arm | Pass | Median wall | Median calls (subagent calls) | Median tokens (input / output / cache read) |
|---|---:|---:|---:|---:|
| `xi` | 0/3 | 385 s | 5 (0) | 5,883 / 19,180 / 13,445 |
| `xi-subagents` | 1/3 | 470 s | 13 (0) | 10,869 / 23,290 / 82,556 |
| `xi-delegate` | 2/3 | 339 s | 26 (20) | 25,680 / 21,807 / 53,213 |

- With the `task` tool available, Qwen never called it in three trials. The `xi-subagents` differences are trial noise.
- When the prompt asked for delegation, Qwen split the packages across children. Two of three trials passed against none for the baseline, at five times the model calls and four times the uncached input. Two baseline trials ended when a single response used the whole 16,384-token output budget reasoning about all five packages; the third hit the 900-second timeout. Each child only holds one package.
- The same `xi` baseline passed 2/3 on the single-slot 256k server (see the suite baseline). The two servers differ in per-slot context and GGUF build, and three trials are few, so the delegation gain is suggestive, not settled.
- Decision: subagents stay an optional package. The mechanism helps when used, so the next step is the tool description or a prompt hint, measured on the same task.
