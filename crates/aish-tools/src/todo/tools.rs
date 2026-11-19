//! LLM tool wrapper over the todo state machine: the `todo` tool.

use aish_llm::{PreflightResult, Tool, ToolResult};

use super::{render_checklist, todo_op};

fn t(key: &str) -> String {
    aish_i18n::t(key)
}

// Cached description (translated once).
static DESC_TODO: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| t("tools.todo.description"));

static USAGE_PROMPT: &str = "Use the todo tool to track multi-step work. Ops: init (3+ steps or requested task sets; list EVERY user item), start (one task), done (mark completed), rm (remove), drop (mark abandoned), append (add items), view.\nTasks are identified by verbatim content, NEVER generated IDs. NEVER take a turn with only todo: emit it in the same response as your next real tool call. Mark done immediately after success; after any mutation the earliest pending task auto-starts, so never start a task manually unless it must jump the queue.";

pub struct TodoTool;

impl Default for TodoTool {
    fn default() -> Self {
        Self
    }
}

impl TodoTool {
    pub fn new() -> Self {
        Self
    }
}

impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        &DESC_TODO
    }

    fn prompt(&self) -> &str {
        USAGE_PROMPT
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": {"type": "string", "enum": ["init","start","done","rm","drop","append","view"]},
                "task": {"description": "verbatim task content; array targets several (not start)", "oneOf": [{"type":"string"},{"type":"array","items":{"type":"string"}}]},
                "phase": {"type": "string", "description": "target phase name (append) or phase to clear (rm)"},
                "items": {"type": "array", "items": {"type": "string"}, "description": "tasks for flat init or append"},
                "list": {"type": "array", "items": {"type": "object", "properties": {"phase": {"type":"string"}, "items": {"type":"array","items":{"type":"string"}}}, "required":["phase","items"]}, "description": "phases for init"}
            },
            "required": ["op"]
        })
    }

    fn preflight(&self, _args: &serde_json::Value) -> PreflightResult {
        PreflightResult::Allow
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        match todo_op(&args) {
            Ok(result) => {
                let mut output = render_checklist();
                if !result.errors.is_empty() {
                    output.push_str(&format!("\nerrors: {}", result.errors.join("; ")));
                }
                ToolResult {
                    ok: true,
                    output,
                    meta: Some(serde_json::json!({
                        "op": args.get("op"),
                        "completed": result.completed,
                    })),
                }
            }
            Err(e) => ToolResult::error(e),
        }
    }
}
