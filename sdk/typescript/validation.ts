import type { Json, Methods } from "./protocol.ts";

export function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function strings(value: unknown): value is string[] {
  return Array.isArray(value) && value.every(item => typeof item === "string");
}
function json(value: unknown): value is Json {
  return value === null || typeof value === "string" || typeof value === "boolean" || (typeof value === "number" && Number.isFinite(value)) || (Array.isArray(value) ? value.every(json) : object(value) && Object.values(value).every(json));
}
function output(value: unknown): boolean {
  return object(value) && (value.is_error === undefined || typeof value.is_error === "boolean") && (value.untrusted === undefined || typeof value.untrusted === "boolean") && Array.isArray(value.content) && value.content.every(block => object(block) && ((block.type === "text" && typeof block.text === "string") || (block.type === "image" && typeof block.media_type === "string" && typeof block.data === "string")));
}
function hook(value: unknown): boolean {
  if (!object(value)) return false;
  switch (value.outcome) {
    case "continue": return true;
    case "deny": return typeof value.reason === "string";
    case "rewrite_input": return json(value.input);
    case "replace_output": return output(value.output);
    case "replace_summary": return typeof value.summary === "string";
    default: return false;
  }
}
function valid<M extends keyof Methods, I extends 0 | 1>(method: M, part: I, value: unknown): value is Methods[M][I] {
  if (method === "shutdown") return value === null;
  if (part === 1 && method === "session/append_custom") return typeof value === "string";
  if (!object(value)) return false;
  if (part === 0) {
    if (method === "initialize") return typeof value.protocol_version === "number" && typeof value.xi_version === "string" && typeof value.workspace === "string";
    if (typeof value.session !== "string") return false;
    switch (method) {
      case "tool/invoke": return typeof value.call === "string" && typeof value.tool === "string" && json(value.input);
      case "command/run": return typeof value.name === "string" && typeof value.args === "string";
      case "session/append_custom": return typeof value.kind === "string" && json(value.data);
      case "agent/run": return typeof value.prompt === "string" && (value.tools === undefined || strings(value.tools)) && (value.model === undefined || (object(value.model) && typeof value.model.provider === "string" && typeof value.model.model === "string"));
      case "hook/run":
        switch (value.point) {
          case "pre_tool": return typeof value.call === "string" && typeof value.tool === "string" && json(value.input);
          case "post_tool": return typeof value.call === "string" && typeof value.tool === "string" && json(value.input) && output(value.output);
          case "turn_end": return typeof value.head === "string";
          case "compact": return typeof value.tokens_before === "number" && (value.previous_summary === undefined || typeof value.previous_summary === "string");
          default: return false;
        }
      default: return false;
    }
  }
  switch (method) {
    case "initialize": return Array.isArray(value.tools) && value.tools.every(tool => object(tool) && typeof tool.name === "string" && typeof tool.description === "string" && json(tool.input_schema)) && Array.isArray(value.commands) && value.commands.every(command => object(command) && typeof command.name === "string" && typeof command.description === "string") && strings(value.hooks) && value.hooks.every(point => ["pre_tool", "post_tool", "turn_end", "compact"].includes(point));
    case "tool/invoke": return output(value);
    case "hook/run": return hook(value);
    case "command/run": return value.message === undefined || typeof value.message === "string";
    case "agent/run": return typeof value.session === "string" && typeof value.path === "string" && typeof value.text === "string" && typeof value.failed === "boolean" && object(value.usage) && ["input", "output", "cache_read", "cache_write"].every(key => object(value.usage) && typeof value.usage[key] === "number");
    default: return false;
  }
}
export function decode<M extends keyof Methods, I extends 0 | 1>(method: M, part: I, value: unknown): Methods[M][I] {
  if (!valid(method, part, value)) throw new Error(`Invalid ${method} ${part === 0 ? "parameters" : "result"}`);
  return value;
}
