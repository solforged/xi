# TypeScript extensions

A zero-dependency extension SDK for Node 26 type stripping and Bun. No install or build step is needed. Import `index.ts` from a package with an `xi-extension.toml` manifest and launch it with `xi --extension /path/to/package`.

## Minimal example

`xi-extension.toml`:

```toml
name = "hello"
version = "0.1.0"
run = ["node", "index.ts"]

[capabilities]
fs_read = ["../xi/sdk/typescript"]
```

`index.ts`:

```typescript
import { defineExtension, text } from "../xi/sdk/typescript/index.ts";

defineExtension({
  tools: [{
    name: "hello",
    description: "Return a greeting.",
    input_schema: { type: "object", properties: {}, additionalProperties: false },
    async invoke(_input, ctx) {
      await ctx.appendCustom("greeted", {});
      ctx.log("info", "Greeting returned");
      return text("Hello");
    },
  }],
  hooks: {
    turn_end: () => ({ outcome: "continue" }),
  },
  commands: [{
    name: "hello",
    description: "Show a greeting.",
    run: args => ({ message: `Hello ${args}` }),
  }],
});
```

Paths in the manifest resolve against the package directory. They are literal existing filesystem roots, not globs. The package is readable but not writable by default. Add `fs_write` only for required roots. The SDK import needs explicit `fs_read` access when it is outside the package. `env` names opt in to passthrough beyond the sandbox's baseline runtime environment. `net` lists the hosts the extension may reach, as case-insensitive globs such as `api.example.org` or `*.github.com`; every connection goes through the session's egress proxy, is refused with `403` when the host is outside the list, and otherwise follows the user's `net` policy rules and approvals. The process gets `HTTP_PROXY` and friends plus `NODE_USE_ENV_PROXY=1`, so `fetch` works unchanged in Node 24+ and Bun. Without `net` the extension has no network. Subprocesses inherit the OS sandbox; there is no separate `exec` capability.

## API

`defineExtension({ tools, hooks, commands, initialize?, shutdown? })` starts an NDJSON JSON-RPC peer on stdio. Do not write application output to stdout. Use `ctx.log(level, message)` or stderr instead. Stderr and logs go to a per-extension file beside the session log.

Tool and command handlers receive `{ session, signal, agentRun, appendCustom, log }`. Tools also receive `call`. `text(value, is_error?, untrusted?)` builds a tool output. Set `untrusted` when output includes third-party content. Post-tool hooks cannot clear an existing taint flag.

`ctx.agentRun({ prompt, name?, tools?, model? })` requires `subagents = true`. It returns `{ session, path, text, failed, usage }`. The kernel creates a linked child log, uses only requested built-in tools from the parent's catalog, and denies approval asks with a recorded headless decision. Omitting `tools` selects every parent built-in except `task`, including enabled `web_search` and `fetch`; an explicit list can only narrow that set. `name` is a short task label, persisted as the child's title and shown in the TUI. Its trimmed value must be one line of 1 to 80 characters; unnamed children show as `Agent N`. `model` is a `ModelRef` such as `{ provider: "claude", model: "claude-haiku-5-5" }` or a role such as `{ role: "smol" }`, resolved against the parent's config; omitting it keeps the parent's model. Children share the workspace, inherit config and sandbox settings, and cannot load extensions or delegate. Network policy still applies, and untrusted child results taint the parent. Use `Promise.all` for independent children; assign disjoint files. Cancelling the parent cancels the children.

Hooks use the Rust protocol's snake-case tagged unions:

- `pre_tool`: `continue`, `deny { reason }`, or `rewrite_input { input }`. Rewritten input goes through policy.
- `post_tool`: `continue` or `replace_output { output }`.
- `turn_end`: `continue` only.
- `compact`: `continue` or `replace_summary { summary }`.

The peer dispatches requests concurrently, so `agentRun` and `appendCustom` work while a tool invocation is pending. Handlers should honor `ctx.signal`. The optional `$/cancelRequest` notification aborts it. The kernel also retires a cancelled extension process and its descendants so ignored cancellation cannot continue writing; its tools then report errors until a new kernel process starts.

`RpcPeer` is exported for custom transports and tests. `protocol.ts` mirrors the small extension subset of `xi-protocol`; `validation.ts` checks the wire boundary without dependencies.

Run the SDK self-test with `node --test sdk/typescript/index.test.ts` from the repository root, or `bun test sdk/typescript/index.test.ts`.
