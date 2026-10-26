import { defineExtension, text } from "../../sdk/typescript/index.ts";
import type { Json } from "../../sdk/typescript/index.ts";

interface Task { prompt: string; tools?: string[] }
function tasksFrom(input: Json): Task[] {
  if (!input || typeof input !== "object" || Array.isArray(input) || !Array.isArray(input.tasks) || input.tasks.length < 1 || input.tasks.length > 8) {
    throw new Error("Expected tasks with 1 to 8 items");
  }
  return input.tasks.map(task => {
    if (!task || typeof task !== "object" || Array.isArray(task) || typeof task.prompt !== "string" || !task.prompt.trim()) throw new Error("Each task needs a nonempty prompt");
    if (task.tools !== undefined && (!Array.isArray(task.tools) || !task.tools.every(tool => typeof tool === "string"))) throw new Error("Task tools must be a string array");
    return { prompt: task.prompt, tools: task.tools };
  });
}

defineExtension({
  tools: [{
    name: "task",
    description: "Run 1 to 8 child agents concurrently for independent sub-problems. Give each child a self-contained prompt with its files, goal and acceptance criteria. Children share the workspace: assign disjoint files when editing. Do not use for dependent steps, small direct questions, or work you can finish with a few tool calls. Children cannot delegate. Results return in task order after all children finish.",
    input_schema: {
      type: "object", required: ["tasks"], additionalProperties: false,
      properties: { tasks: { type: "array", minItems: 1, maxItems: 8, items: {
        type: "object", required: ["prompt"], additionalProperties: false,
        properties: { prompt: { type: "string", minLength: 1 }, tools: { type: "array", items: { type: "string" } } },
      } } },
    },
    async invoke(input, context) {
      const tasks = tasksFrom(input);
      const results = await Promise.all(tasks.map(async (task, index) => {
        try {
          // Without `tools`, the kernel gives the child the parent's built-ins.
          const result = await context.agentRun({ prompt: task.prompt, tools: task.tools });
          return { failed: result.failed, text: `Task ${index + 1} [${result.failed ? "FAILED" : "done"}]\n${result.text}` };
        } catch (error) {
          return { failed: true, text: `Task ${index + 1} [FAILED]\n${String(error)}` };
        }
      }));
      return text(results.map(result => result.text).join("\n\n"), results.some(result => result.failed));
    },
  }],
});
