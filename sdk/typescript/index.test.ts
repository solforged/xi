import { test } from "node:test";
import assert from "node:assert/strict";
import { PassThrough } from "node:stream";
import { defineExtension, RpcPeer, text } from "./index.ts";

test("full duplex handlers, ordered concurrent children and hooks", async () => {
  const toExtension = new PassThrough();
  const toKernel = new PassThrough();
  const kernel = new RpcPeer(toKernel, toExtension);
  const extension = new RpcPeer(toExtension, toKernel);
  let custom = false;
  kernel.handle("session/append_custom", params => { custom = params.kind === "test"; return "event"; });
  kernel.handle("agent/run", async params => ({ session: params.prompt, path: "/child.jsonl", text: params.prompt, failed: false, usage: { input: 1, output: 2, cache_read: 0, cache_write: 0 } }));
  defineExtension({
    tools: [{ name: "test", description: "Test", input_schema: {}, async invoke(_input, ctx) {
      await ctx.appendCustom("test", {});
      const children = await Promise.all(["first", "second"].map(prompt => ctx.agentRun({ prompt })));
      return text(children.map(child => child.text).join(","));
    } }],
    hooks: { compact: () => ({ outcome: "replace_summary", summary: "summary" }) },
  }, extension);
  const registration = await kernel.request("initialize", { protocol_version: 0, xi_version: "0", workspace: "/tmp" });
  assert.equal(registration.tools[0].name, "test");
  const result = await kernel.request("tool/invoke", { session: "parent", call: "call", tool: "test", input: {} });
  assert.deepEqual(result, text("first,second"));
  assert.equal(custom, true);
  assert.deepEqual(await kernel.request("hook/run", { session: "parent", point: "compact", tokens_before: 10 }), { outcome: "replace_summary", summary: "summary" });
  assert.equal(await kernel.request("shutdown", null), null);
  kernel.close(); extension.close();
  toExtension.destroy(); toKernel.destroy();
});

test("cancellation reaches the pending handler", async () => {
  const toExtension = new PassThrough();
  const toKernel = new PassThrough();
  const kernel = new RpcPeer(toKernel, toExtension);
  const extension = new RpcPeer(toExtension, toKernel);
  const entered = Promise.withResolvers<void>();
  const aborted = Promise.withResolvers<void>();
  extension.handle("tool/invoke", async (_params, signal) => {
    signal.addEventListener("abort", () => aborted.resolve(), { once: true });
    entered.resolve();
    await aborted.promise;
    return text("cancelled", true);
  });
  const controller = new AbortController();
  const pending = kernel.request("tool/invoke", { session: "parent", call: "call", tool: "test", input: {} }, controller.signal);
  const rejected = assert.rejects(pending, /cancelled/);
  await entered.promise;
  controller.abort();
  await rejected;
  await aborted.promise;
  kernel.close(); extension.close();
  toExtension.destroy(); toKernel.destroy();
});
