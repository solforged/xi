import { defineExtension, text } from "../../../../sdk/typescript/index.ts";
import { readFileSync, writeFileSync } from "node:fs";
import type { Json } from "../../../../sdk/typescript/index.ts";
import { setTimeout as delay } from "node:timers/promises";

let workspace = "";
function field(input: Json, key: string): Json | undefined {
  return input && typeof input === "object" && !Array.isArray(input) ? input[key] : undefined;
}
const peer = defineExtension({
  initialize(params) { workspace = params.workspace; },
  tools: [{
    name: "probe", description: "Test extension", input_schema: { type: "object" },
    async invoke(input, ctx) {
      const mode = field(input, "mode");
      if (mode === "crash") process.exit(9);
      if (mode === "child") {
        const results = await Promise.all(["child one", "child two"].map(prompt => ctx.agentRun({ prompt, tools: ["read"] })));
        await ctx.appendCustom("children", { results });
        return text(results.map(result => result.text).join("\n"));
      }
      if (mode === "ask_child") {
        const result = await ctx.agentRun({ prompt: "ask child", tools: ["bash"] });
        await ctx.appendCustom("ask_child", { path: result.path });
        return text(result.text);
      }
      if (mode === "unknown_child") return text((await ctx.agentRun({ prompt: "child one", tools: ["probe"] })).text);
      if (mode === "slow_child") return text((await ctx.agentRun({ prompt: "slow child" })).text);
      if (mode === "fetch") {
        try {
          const response = await fetch(String(field(input, "url")));
          return text(`${response.status} ${(await response.text()).trim()}`);
        } catch (error) {
          return text(`error ${error}`, true);
        }
      }
      if (mode === "sandbox") {
        let readable = false;
        try { readFileSync(`${workspace}/secret`); readable = true; } catch {}
        let writable = false;
        try { writeFileSync(`${workspace}/escape`, "bad"); writable = true; } catch {}
        if (readable || writable) throw new Error("extension escaped manifest roots");
      }
      await ctx.appendCustom("invoked", { mode: mode ?? null });
      ctx.log("info", "probe invoked");
      return text("extension result", false, true);
    },
  }],
  hooks: {
    pre_tool(event) {
      if (field(event.input, "mode") === "deny") return { outcome: "deny", reason: "hook denied" };
      if (field(event.input, "command") === "rewrite me") return { outcome: "rewrite_input", input: { command: "blocked command" } };
      if (field(event.input, "mode") === "invalid") return { outcome: "replace_summary", summary: "wrong point" };
      return { outcome: "continue" };
    },
    post_tool(event) {
      if (event.tool === "probe" && field(event.input, "mode") === "replace") return { outcome: "replace_output", output: text("replacement") };
      return { outcome: "continue" };
    },
    async turn_end(_event, ctx) { await ctx.appendCustom("turn_end", { pid: process.pid }); return { outcome: "continue" }; },
    async compact(_event, ctx) { await ctx.appendCustom("compact", {}); return { outcome: "replace_summary", summary: "extension summary" }; },
  },
  commands: [
    { name: "echo", description: "Echo test", async run(args, ctx) { await ctx.appendCustom("command", { args }); return { message: args }; } },
    { name: "identity", description: "Process identity", run() { return { message: String(process.pid) }; } },
    { name: "slow", description: "Wait until the caller drops its service future", async run() { await delay(60_000); return {}; } },
    { name: "cross", description: "Try another session", async run(session) {
      const rejected = await Promise.all([
        peer.request("session/append_custom", { session, kind: "crossed", data: {} }),
        peer.request("agent/run", { session, prompt: "child one", tools: ["read"] }),
      ].map(request => request.then(() => false, () => true)));
      return { message: JSON.stringify(rejected) };
    } },
    { name: "fetch", description: "Fetch with overlapping session scopes", async run(url) {
      await delay(50);
      const response = await fetch(url);
      return { message: `${response.status} ${(await response.text()).trim()}` };
    } },
  ],
});
