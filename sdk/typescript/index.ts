import { createInterface, type Interface } from "node:readline";
import type { Readable, Writable } from "node:stream";
import type {
  AgentRunParams, AgentRunResult, CommandParams, CommandResult, CommandSpec,
  EventId, HookOutcome, HookParams, HookPoint, InitializeParams, InvokeParams,
  Json, LogLevel, Methods, Registration, SessionId, ToolOutput, ToolSpec,
} from "./protocol.ts";
import { decode, object } from "./validation.ts";
export type * from "./protocol.ts";

type Id = string | number;
type Handler = (params: unknown, signal: AbortSignal) => unknown | Promise<unknown>;

/** Full-duplex NDJSON peer. Handlers never block delivery of other messages. */
export class RpcPeer {
  private next = 1;
  private pending = new Map<Id, { resolve: (value: unknown) => void; reject: (error: Error) => void }>();
  private active = new Map<Id, AbortController>();
  private handlers = new Map<string, Handler>();
  private lines: Interface;
  private output: Writable;
  private closed = false;

  constructor(input: Readable = process.stdin, output: Writable = process.stdout) {
    this.output = output;
    this.lines = createInterface({ input, crlfDelay: Infinity });
    this.lines.on("line", line => { void this.receive(line); });
    this.lines.on("close", () => this.close());
    input.on("error", error => this.close(error));
    output.on("error", error => this.close(error));
  }

  handle<M extends keyof Methods>(method: M, handler: (params: Methods[M][0], signal: AbortSignal) => Methods[M][1] | Promise<Methods[M][1]>): void {
    if (this.handlers.has(method)) throw new Error(`Duplicate handler: ${method}`);
    this.handlers.set(method, (params, signal) => handler(decode(method, 0, params), signal));
  }

  request<M extends keyof Methods>(method: M, params: Methods[M][0], signal?: AbortSignal): Promise<Methods[M][1]> {
    if (this.closed) return Promise.reject(new Error("RPC peer closed"));
    if (signal?.aborted) return Promise.reject(new Error("RPC request cancelled"));
    const id = this.next++;
    const { promise, resolve, reject } = Promise.withResolvers<Methods[M][1]>();
    const abort = () => {
      this.pending.delete(id);
      this.notify("$/cancelRequest", { id });
      reject(new Error("RPC request cancelled"));
    };
    const cleanup = () => signal?.removeEventListener("abort", abort);
    this.pending.set(id, {
      resolve: value => {
        cleanup();
        try { resolve(decode(method, 1, value)); } catch (error) { reject(error); }
      },
      reject: error => { cleanup(); reject(error); },
    });
    signal?.addEventListener("abort", abort, { once: true });
    this.send({ jsonrpc: "2.0", id, method, params });
    return promise;
  }

  notify(method: string, params: unknown): void {
    this.send({ jsonrpc: "2.0", method, params });
  }

  close(error = new Error("RPC peer closed")): void {
    if (this.closed) return;
    this.closed = true;
    this.lines.close();
    for (const controller of this.active.values()) controller.abort(error);
    this.active.clear();
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
  }

  private send(message: unknown): void {
    if (this.closed) return;
    this.output.write(`${JSON.stringify(message)}\n`);
  }

  private async receive(line: string): Promise<void> {
    let message: unknown;
    try { message = JSON.parse(line); }
    catch { this.send({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Invalid JSON" } }); return; }
    if (!object(message) || message.jsonrpc !== "2.0") {
      this.send({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "Invalid JSON-RPC message" } });
      return;
    }
    const id = typeof message.id === "string" || typeof message.id === "number" ? message.id : undefined;
    if (typeof message.method !== "string") {
      const pending = id === undefined ? undefined : this.pending.get(id);
      if (!pending) return;
      this.pending.delete(id!);
      if (object(message.error) && typeof message.error.message === "string") pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
      return;
    }
    if (message.method === "$/cancelRequest") {
      if (object(message.params) && (typeof message.params.id === "number" || typeof message.params.id === "string")) {
        this.active.get(message.params.id)?.abort(new Error("RPC request cancelled"));
      }
      return;
    }
    const handler = this.handlers.get(message.method);
    if (!handler) {
      if (id !== undefined) this.send({ jsonrpc: "2.0", id, error: { code: -32601, message: `Unknown method: ${message.method}` } });
      return;
    }
    const controller = new AbortController();
    if (id !== undefined) this.active.set(id, controller);
    try {
      const result = await handler(message.params ?? null, controller.signal);
      if (id !== undefined) this.send({ jsonrpc: "2.0", id, result: result ?? null });
    } catch (error) {
      if (id !== undefined) this.send({ jsonrpc: "2.0", id, error: { code: -32000, message: String(error) } });
    } finally {
      if (id !== undefined) this.active.delete(id);
    }
  }
}

export interface ExtensionContext {
  session: SessionId;
  signal: AbortSignal;
  agentRun(params: Omit<AgentRunParams, "session">): Promise<AgentRunResult>;
  appendCustom(kind: string, data: Json): Promise<EventId>;
  log(level: LogLevel, message: string): void;
}
export interface ToolDefinition extends ToolSpec {
  invoke(input: Json, context: ExtensionContext & { call: string }): ToolOutput | Promise<ToolOutput>;
}
export interface CommandDefinition extends CommandSpec {
  run(args: string, context: ExtensionContext): CommandResult | Promise<CommandResult>;
}
export type Hooks = {
  [P in HookPoint]?: (event: Extract<HookParams, { point: P }>, context: ExtensionContext) => HookOutcome | Promise<HookOutcome>;
};
export interface ExtensionDefinition {
  tools?: ToolDefinition[];
  hooks?: Hooks;
  commands?: CommandDefinition[];
  initialize?(params: InitializeParams): void | Promise<void>;
  shutdown?(): void | Promise<void>;
}

/** Starts a zero-build extension on stdio; stdout is reserved for JSON-RPC. */
export function defineExtension(definition: ExtensionDefinition, peer = new RpcPeer()): RpcPeer {
  const tools = new Map((definition.tools ?? []).map(tool => [tool.name, tool]));
  const commands = new Map((definition.commands ?? []).map(command => [command.name, command]));
  if (tools.size !== (definition.tools?.length ?? 0) || commands.size !== (definition.commands?.length ?? 0)) throw new Error("Duplicate extension registration");
  const context = (session: SessionId, signal: AbortSignal): ExtensionContext => ({
    session, signal,
    agentRun: params => peer.request("agent/run", { ...params, session }, signal),
    appendCustom: (kind, data) => peer.request("session/append_custom", { session, kind, data }, signal),
    log: (level, message) => peer.notify("log", { level, message }),
  });
  peer.handle("initialize", async params => {
    if (params.protocol_version !== 0) throw new Error(`Unsupported protocol version: ${params.protocol_version}`);
    await definition.initialize?.(params);
    return {
      tools: [...tools.values()].map(({ name, description, input_schema }) => ({ name, description, input_schema })),
      hooks: Object.keys(definition.hooks ?? {}) as HookPoint[],
      commands: [...commands.values()].map(({ name, description }) => ({ name, description })),
    } satisfies Registration;
  });
  peer.handle("tool/invoke", async (params: InvokeParams, signal) => {
    const tool = tools.get(params.tool);
    if (!tool) throw new Error(`Unknown tool: ${params.tool}`);
    return tool.invoke(params.input, { ...context(params.session, signal), call: params.call });
  });
  peer.handle("hook/run", async (params: HookParams, signal) => {
    const hook = definition.hooks?.[params.point] as ((event: HookParams, context: ExtensionContext) => HookOutcome | Promise<HookOutcome>) | undefined;
    return hook ? hook(params, context(params.session, signal)) : { outcome: "continue" };
  });
  peer.handle("command/run", async (params: CommandParams, signal) => {
    const command = commands.get(params.name);
    if (!command) throw new Error(`Unknown command: ${params.name}`);
    return command.run(params.args, context(params.session, signal));
  });
  // The host closes stdin after the response; EOF aborts any remaining work.
  peer.handle("shutdown", async () => { await definition.shutdown?.(); return null; });
  return peer;
}

export function text(text: string, is_error = false, untrusted = false): ToolOutput {
  return { content: [{ type: "text", text }], is_error, untrusted };
}
