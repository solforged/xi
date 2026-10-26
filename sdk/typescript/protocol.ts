// Hand-written extension subset of xi-protocol. Field names match the wire.
export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type SessionId = string;
export type EventId = string;
export type ToolCallId = string;
export interface ModelRef { provider: string; model: string }
export interface Usage { input: number; output: number; cache_read: number; cache_write: number }
export type OutputBlock = { type: "text"; text: string } | { type: "image"; media_type: string; data: string };
export interface ToolOutput { content: OutputBlock[]; is_error?: boolean; untrusted?: boolean }
export interface ToolSpec { name: string; description: string; input_schema: Json }
export interface CommandSpec { name: string; description: string }
export type HookPoint = "pre_tool" | "post_tool" | "turn_end" | "compact";
export interface Registration { tools: ToolSpec[]; hooks: HookPoint[]; commands: CommandSpec[] }
export interface InitializeParams { protocol_version: number; xi_version: string; workspace: string }
export interface InvokeParams { session: SessionId; call: ToolCallId; tool: string; input: Json }
export type HookEvent =
  | { point: "pre_tool"; call: ToolCallId; tool: string; input: Json }
  | { point: "post_tool"; call: ToolCallId; tool: string; input: Json; output: ToolOutput }
  | { point: "turn_end"; head: EventId }
  | { point: "compact"; tokens_before: number; previous_summary?: string };
export type HookParams = { session: SessionId } & HookEvent;
export type HookOutcome =
  | { outcome: "continue" }
  | { outcome: "deny"; reason: string }
  | { outcome: "rewrite_input"; input: Json }
  | { outcome: "replace_output"; output: ToolOutput }
  | { outcome: "replace_summary"; summary: string };
export interface CommandParams { session: SessionId; name: string; args: string }
export interface CommandResult { message?: string }
export interface AppendCustomParams { session: SessionId; kind: string; data: Json }
export interface AgentRunParams { session: SessionId; prompt: string; tools?: string[]; model?: ModelRef }
export interface AgentRunResult { session: SessionId; path: string; text: string; failed: boolean; usage: Usage }
export type LogLevel = "debug" | "info" | "warn" | "error";
export interface LogParams { level: LogLevel; message: string }
export interface Methods {
  initialize: [InitializeParams, Registration];
  "tool/invoke": [InvokeParams, ToolOutput];
  "hook/run": [HookParams, HookOutcome];
  "command/run": [CommandParams, CommandResult];
  shutdown: [null, null];
  "session/append_custom": [AppendCustomParams, EventId];
  "agent/run": [AgentRunParams, AgentRunResult];
}
