# Roadmap

xi now has a usable agent on Linux and macOS and a benchmark to measure it with. From here, features get added one at a time and stay in the default only when a measurement supports them. The oh-my-pi (omp) survey behind this list is summarised in [What omp teaches](#what-omp-teaches).

## Two tracks

Work runs on two tracks that only meet at two interfaces, so neither has to wait for the other.

| Track | Owns | Interface to the other track |
|---|---|---|
| Platform | `xi-sandbox`, `xi-policy`, workspace trust, egress proxy, secrets | `xi_sandbox::Spec` (what a process may touch) and `xi_tools::Crossing` (what a call asks policy for) |
| Features | tools, context, compaction, extensions, subagents, clients | A feature that needs new access declares a crossing or a spec field. It never changes the sandbox itself. |

A feature lands behind a toggle: a tool name for `--tools`, a config key, or an extension package. `xi-bench` then compares it against the baseline arm on the same tasks and model. A feature goes into the default set only when it pays for its tokens and time on the suite, or when safety needs it.

## Baseline

The arm everything is compared against: `read`, `edit`, `write`, `bash`, `search`; a static system prompt under 1k tokens; `AGENTS.md` files and a skills index. No compaction, subagents or extensions.

## Feature queue

In order. Each row names the measurement that decides it.

| # | Feature | Home | Decided by |
|---|---|---|---|
| 1 | Harder benchmark tasks: multi-step, 5 to 15 files, a long-context task, a task that parallelises | `bench/` | Arms stop passing everything; the first suite is at ceiling. |
| 2 | Edit format: hash anchors (`edit`) or exact replacement (`replace`) | `xi-tools` | Edit failure rate and calls on several models. On Qwen 3.8 the anchor format fails much more often. |
| 3 | Compaction: a structured summary appended as a `compaction` event, keeping recent tool results | kernel, `on_compact` hook | Pass rate and tokens on the long-context task. |
| 4 | Extension host and TypeScript SDK | kernel, `sdk/typescript` | Gate for rows 5 to 9: each ships as a package. |
| 5 | Subagents: `task` runs a child session with a reduced toolset and returns its final message as tool output | package | Calls, tokens and wall time on the parallel task, against the baseline doing it alone. |
| 6 | LSP diagnostics after edits | package | Calls and time to green on the Rust and TypeScript tasks. |
| 7 | Todo or plan tool | package | Pass rate on multi-step tasks, prompt size. |
| 8 | `fetch` and web search, with taint wired in | package plus platform egress proxy | Tasks that need docs; taint must turn later egress into `ask`. |
| 9 | Memory | package | Last: needs multi-session tasks to measure. |
| 10 | TUI | client | Not benchmarked; replaces line mode when the protocol is stable. |

Interactive steering in line mode is missing (the RPC path queues prompts already) and belongs with the TUI.

## Platform queue

| Item | Why |
|---|---|
| Egress proxy and in-namespace forwarder | Tools have no network at all today; `fetch`, package installs and `git push` need it. |
| Anthropic live test | The adapter passes mocked transcripts only; there is no key on this machine. |
| Credentials from the keychain | Only env vars and 0600 files work now. |
| PID namespace (Linux) | Other processes' command lines are visible in `/proc`. |
| seccomp filter (Linux) | Reduces kernel attack surface (`io_uring`, `bpf`, `userfaultfd`). |
| Hooks in repos created by `git init` inside the sandbox | Their `.git/hooks` stays writable until the next tool call builds a new spec. |
| Git hook policy | Sandboxed git uses a session config with identity only, so global hooks (gitleaks) do not run inside. Decide whether repo hooks should run there. |

## What omp teaches

From a read-only survey of `~/src/oh-my-pi` (31 tools, a loop of about 115 KB in `packages/agent/src/agent-loop.ts`):

- Hard loop invariants, now in the xi kernel: steering never lands between a `tool_use` and its `tool_result`; a user abort keeps the steering queue; a dangling tool call gets a synthetic result before the next request.
- Removed features are the strongest signal. omp dropped its `librarian` and `designer` subagents, Ruby and Julia eval, git and jj wrappers, per-call model overrides and several hashline ops (`DEL`, `COPY`). Small, general tool vocabularies outlast specialised ones.
- Most of omp's surface is optional subsystems: subagents and the job hub, advisor (a second model reviewing turns), LSP and DAP, memory, stream-interrupt rules, eval kernels. In xi each of these is a package and a benchmark ablation, in the order of the feature queue.
- Sol's pi fork already ships subagents, web search and compaction as extensions (`~/src/pi-sol/extensions`). Read its `workflows` extension before designing the subagent package.

## Benchmark results

Run with `xi-bench` (see [bench/README.md](../bench/README.md)) on the five starter tasks, three trials each, model `amarna/qwen3.8-27b-256k` (Qwen 3.8 27B on the local RTX PRO 6000).

Run on 2026-10-26 after the cargo fix, 15 runs per arm:

| Arm | Pass rate | Median wall | Median model calls | Median tokens per run (input / output / cache read) | Edit failures |
|---|---:|---:|---:|---:|---:|
| `xi` (hash-anchored `edit`) | 15/15 | 12.9 s | 6 | 3,360 / 1,111 / 13,436 | 8 of 32 (25%) |
| `xi-replace` (`replace`) | 15/15 | 13.1 s | 6 | 2,836 / 1,136 / 10,892 | 1 of 37 (3%) |
| `omp` | 15/15 | 18.8 s | 6 | 1,756 / 1,600 / 62,675 | 3 of 23 (13%) |

What it shows:

- Every arm passes every task, so the suite cannot rank harnesses yet. Harder tasks come first in the feature queue.
- xi sends about a fifth of omp's prompt tokens per run (cache reads), and finishes faster at the same number of model calls. Most of the difference is the size of the system prompt and tool catalog. The omp arm keeps Sol's omp configuration apart from the flags in `bench/arms.toml`, so it is not a minimal omp.
- Qwen 3.8 gets hash anchors wrong often: it writes `2a:` or a bare line number, or pastes whole rows. `replace` almost never fails. The anchor format needs testing on frontier models before it stays the default.
- The first run, before the fix, found two real bugs: cargo was unreachable in the macOS sandbox (its homes and PATH directories were not readable), and anchors pasted with their row text were rejected. Both are fixed.

The `pi` arm passed a one-trial smoke run on `openai/gpt-6-luna`. Pi has no amarna provider configured, so it is not in this table. xi cannot run `gpt-6-luna` either, without an `OPENAI_API_KEY`.
