//! LLM tool wrapper over the todo state machine: the `todo` tool.

use aish_llm::{PreflightResult, Tool, ToolResult};

use super::{new_sub_session_key, render_phases, todo_op_for, MAIN_SESSION_KEY};

fn t(key: &str) -> String {
    aish_i18n::t(key)
}

// Cached description (translated once).
static DESC_TODO: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| t("tools.todo.description"));

static USAGE_PROMPT: &str = "Use the todo tool to track multi-step work. Ops: init (3+ steps or requested task sets; list EVERY user item), start (one task), done (mark completed), rm (remove), drop (mark abandoned), append (add items), view.\nTasks are identified by verbatim content, NEVER generated IDs. NEVER take a turn with only todo: emit it in the same response as your next real tool call. Mark done immediately after success; after any mutation the earliest pending task auto-starts, so never start a task manually unless it must jump the queue.";

/// The `todo` tool bound to one session's checklist.
///
/// Each sub-agent receives its own instance (see [`Tool::for_sub_session`]) so
/// a child's `init` cannot wipe the parent's plan.
pub struct TodoTool {
    session_key: String,
}

impl Default for TodoTool {
    fn default() -> Self {
        Self::new()
    }
}

impl TodoTool {
    /// Tool for the user-facing shell session.
    pub fn new() -> Self {
        Self {
            session_key: MAIN_SESSION_KEY.to_string(),
        }
    }

    /// Tool bound to an explicit session key (tests and sub-agents).
    pub fn with_session_key(session_key: impl Into<String>) -> Self {
        Self {
            session_key: session_key.into(),
        }
    }

    /// Session key this instance mutates.
    pub fn session_key(&self) -> &str {
        &self.session_key
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
                "phase": {"type": "string", "description": "target phase name (append) or phase to clear/mark (rm, done, drop)"},
                "items": {"type": "array", "items": {"type": "string"}, "description": "tasks for flat init or append"},
                "list": {"type": "array", "items": {"type": "object", "properties": {"phase": {"type":"string"}, "items": {"type":"array","items":{"type":"string"}}}, "required":["phase","items"]}, "description": "phases for init"}
            },
            "required": ["op"],
            // `start` targets exactly one task; an array there is rejected by
            // the op, so fail it at the schema boundary instead.
            "if": {"properties": {"op": {"const": "start"}}},
            "then": {"properties": {"task": {"type": "string"}}}
        })
    }

    fn preflight(&self, _args: &serde_json::Value) -> PreflightResult {
        PreflightResult::Allow
    }

    fn execute(&self, args: serde_json::Value) -> ToolResult {
        match todo_op_for(&self.session_key, &args) {
            Ok(result) => {
                // Render the op's own snapshot: re-reading the global state
                // would let a concurrent agent's tasks leak into this result.
                let checklist = render_phases(&result.phases);
                let mut output = checklist.clone();
                if !result.errors.is_empty() {
                    output.push_str(&format!("\nerrors: {}", result.errors.join("; ")));
                }
                ToolResult {
                    // A rejected op must not present as success; the shell
                    // card keys its ✔/✗ marker off this flag.
                    ok: result.errors.is_empty(),
                    output,
                    meta: Some(serde_json::json!({
                        "op": args.get("op"),
                        "completed": result.completed,
                        "errors": result.errors,
                        // The card renderer draws this snapshot rather than
                        // re-reading global state, so a sub-agent's checklist
                        // is attributed to the agent that produced it.
                        "checklist": checklist,
                    })),
                }
            }
            Err(e) => ToolResult::error(e),
        }
    }

    fn for_sub_session(&self, _sub: &aish_llm::LlmSession) -> Option<std::sync::Arc<dyn Tool>> {
        // Sub-agents get a private checklist: a child `init` must never wipe
        // the parent's plan.
        Some(std::sync::Arc::new(Self {
            session_key: new_sub_session_key(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::{self, serialize_test};

    fn unique_tool() -> TodoTool {
        // A unique key per test keeps the process-global map from leaking
        // between parallel test bodies.
        TodoTool::with_session_key(todo::new_sub_session_key())
    }

    #[test]
    fn execute_renders_the_ops_own_snapshot() {
        let _guard = serialize_test();
        let tool = unique_tool();
        tool.execute(serde_json::json!({"op": "init", "items": ["mine"]}));
        let result = tool.execute(serde_json::json!({"op": "view"}));
        assert!(result.ok);
        assert!(result.output.contains("mine"), "got: {}", result.output);
    }

    #[test]
    fn execute_exposes_the_snapshot_for_the_card_renderer() {
        let _guard = serialize_test();
        let tool = unique_tool();
        tool.execute(serde_json::json!({"op": "init", "items": ["card-task"]}));
        let result = tool.execute(serde_json::json!({"op": "view"}));
        let meta = result.meta.expect("meta");
        let checklist = meta["checklist"].as_str().expect("checklist string");
        assert!(checklist.contains("card-task"), "got: {checklist}");
        assert_eq!(
            checklist, result.output,
            "card snapshot diverged from output"
        );
    }

    #[test]
    fn rejected_op_exposes_errors_in_meta() {
        let _guard = serialize_test();
        let tool = unique_tool();
        tool.execute(serde_json::json!({"op": "init", "items": ["a"]}));
        let result = tool.execute(serde_json::json!({"op": "done", "task": "ghost"}));
        let meta = result.meta.expect("meta");
        assert_eq!(
            meta["errors"],
            serde_json::json!(["task not found: ghost"]),
            "card needs the error text to render it"
        );
    }

    #[test]
    fn failed_op_reports_not_ok() {
        let _guard = serialize_test();
        let tool = unique_tool();
        tool.execute(serde_json::json!({"op": "init", "items": ["a"]}));
        let result = tool.execute(serde_json::json!({"op": "done", "task": "ghost"}));
        assert!(!result.ok, "rejected op must not report success");
        assert!(
            result.output.contains("errors: task not found: ghost"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn missing_op_still_errors() {
        let _guard = serialize_test();
        let tool = unique_tool();
        let result = tool.execute(serde_json::json!({}));
        assert!(!result.ok);
        assert_eq!(result.output, "missing op");
    }

    #[test]
    fn concurrent_execute_never_returns_foreign_tasks() {
        let _guard = serialize_test();
        // P0-3: `execute` used to re-read the global state, so under
        // concurrency an op's output could contain another thread's tasks.
        let tool = std::sync::Arc::new(unique_tool());
        let noise_key = new_sub_session_key();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        let noise_stop = stop.clone();
        let noise = std::thread::spawn(move || {
            let mut i = 0u64;
            while !noise_stop.load(std::sync::atomic::Ordering::Relaxed) {
                i += 1;
                todo::todo_op_for(
                    &noise_key,
                    &serde_json::json!({"op": "append", "items": [format!("NOISE-{i}")]}),
                )
                .expect("noise op");
            }
        });

        let mut violations = 0;
        for _ in 0..300 {
            let result = tool.execute(serde_json::json!({"op": "done", "task": "task-0"}));
            if result.output.contains("NOISE") {
                violations += 1;
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        noise.join().expect("noise thread");

        assert_eq!(
            violations, 0,
            "{violations}/300 outputs contained foreign tasks"
        );
    }

    #[test]
    fn sub_session_instances_are_isolated() {
        let _guard = serialize_test();
        let parent = unique_tool();
        parent.execute(serde_json::json!({"op": "init", "items": ["parent-task"]}));

        // Emulate `for_sub_session`: a fresh key with its own state.
        let child = TodoTool::with_session_key(new_sub_session_key());
        child.execute(serde_json::json!({"op": "init", "items": ["child-task"]}));

        let parent_view = parent.execute(serde_json::json!({"op": "view"}));
        assert!(parent_view.output.contains("parent-task"));
        assert!(
            !parent_view.output.contains("child-task"),
            "child clobbered parent: {}",
            parent_view.output
        );
    }

    #[test]
    fn escape_sequences_do_not_reach_the_tool_output() {
        let _guard = serialize_test();
        let tool = unique_tool();
        tool.execute(serde_json::json!({
            "op": "init",
            "items": [
                "\u{1b}[31mRED\u{1b}[0m",
                "\u{1b}]8;;https://evil\u{7}link\u{1b}]8;;\u{7}",
                "line\nbreak",
            ],
        }));
        let result = tool.execute(serde_json::json!({"op": "view"}));
        assert!(
            !result.output.contains('\u{1b}'),
            "SGR/OSC leaked: {:?}",
            result.output
        );
        assert!(
            !result.output.contains('\u{7}'),
            "BEL leaked: {:?}",
            result.output
        );
        assert!(result.output.contains("RED"));
        assert!(result.output.contains("link"));
        assert!(result.output.contains("line break"));
    }
}
