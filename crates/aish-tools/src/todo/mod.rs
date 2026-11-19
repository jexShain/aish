//! Model-maintained todo state machine (oh-my-pi style).
//!
//! A process-global phase list (`LazyLock` + `parking_lot::Mutex`) holds the
//! todo state for the lifetime of the aish process. The model mutates it via
//! the `todo` tool (`tools.rs`); the shell renders it with
//! [`render_checklist`]. Tasks are identified by verbatim content, never IDs.

pub mod tools;

use std::sync::LazyLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a todo task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Abandoned,
}

impl TodoStatus {
    fn label(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
            TodoStatus::Abandoned => "abandoned",
        }
    }

    fn marker(self) -> &'static str {
        match self {
            TodoStatus::Pending => "☐",
            TodoStatus::InProgress => "◐",
            TodoStatus::Completed => "☑",
            TodoStatus::Abandoned => "✗",
        }
    }
}

/// One task inside a phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// A named group of tasks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoPhase {
    pub name: String,
    pub tasks: Vec<TodoItem>,
}

/// Result of one `todo_op` call.
#[derive(Debug, Clone, Default)]
pub struct TodoOpResult {
    pub phases: Vec<TodoPhase>,
    pub errors: Vec<String>,
    /// `(phase_name, task_content)` for every task that became completed.
    pub completed: Vec<(String, String)>,
}

/// Default phase name for flat `init` and `append` to an empty list.
const DEFAULT_PHASE: &str = "Tasks";

#[derive(Default)]
struct State {
    phases: Vec<TodoPhase>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

fn state() -> parking_lot::MutexGuard<'static, State> {
    STATE.lock()
}

/// Read-only snapshot of the current phases.
pub fn current_phases() -> Vec<TodoPhase> {
    state().phases.clone()
}

/// Clear all todo state.
pub fn reset() {
    state().phases.clear();
}

/// Render the checklist; one `[name] done/total` header plus one checkbox
/// line per task. Returns `"(no todos)"` when the list is empty.
pub fn render_checklist() -> String {
    let phases = current_phases();
    if phases.iter().all(|p| p.tasks.is_empty()) {
        return "(no todos)".to_string();
    }
    let mut out = String::new();
    for phase in &phases {
        let total = phase.tasks.len();
        let done = phase
            .tasks
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        out.push_str(&format!("[{}] {}/{}\n", phase.name, done, total));
        for task in &phase.tasks {
            out.push_str(&format!("  {} {}\n", task.status.marker(), task.content));
        }
    }
    // Trim the trailing newline of the last line.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

/// Find a task by verbatim (trimmed) content; returns `(phase index, task index)`.
fn find_task(phases: &[TodoPhase], content: &str) -> Option<(usize, usize)> {
    for (pi, phase) in phases.iter().enumerate() {
        for (ti, task) in phase.tasks.iter().enumerate() {
            if task.content.trim() == content.trim() {
                return Some((pi, ti));
            }
        }
    }
    None
}

/// If more than one task is in progress, demote all but the earliest back to
/// pending; if none is in progress, promote the earliest pending task.
fn auto_promote(phases: &mut [TodoPhase]) {
    let mut first_in_progress: Option<(usize, usize)> = None;
    let mut in_progress_count = 0usize;
    let mut first_pending: Option<(usize, usize)> = None;
    for (pi, phase) in phases.iter().enumerate() {
        for (ti, task) in phase.tasks.iter().enumerate() {
            match task.status {
                TodoStatus::InProgress => {
                    in_progress_count += 1;
                    if first_in_progress.is_none() {
                        first_in_progress = Some((pi, ti));
                    }
                }
                TodoStatus::Pending if first_pending.is_none() => {
                    first_pending = Some((pi, ti));
                }
                _ => {}
            }
        }
    }
    if in_progress_count > 1 {
        let keep = first_in_progress;
        let mut seen_keep = false;
        for (pi, phase) in phases.iter_mut().enumerate() {
            for (ti, task) in phase.tasks.iter_mut().enumerate() {
                if task.status != TodoStatus::InProgress {
                    continue;
                }
                if !seen_keep && keep == Some((pi, ti)) {
                    seen_keep = true;
                    continue;
                }
                task.status = TodoStatus::Pending;
            }
        }
    } else if in_progress_count == 0 {
        if let Some((pi, ti)) = first_pending {
            phases[pi].tasks[ti].status = TodoStatus::InProgress;
        }
    }
}

/// Extract a string or array-of-strings parameter as a list of contents.
fn task_list_from(args: &serde_json::Value, key: &str) -> Option<Vec<String>> {
    match args.get(key) {
        Some(serde_json::Value::String(s)) => Some(vec![s.clone()]),
        Some(serde_json::Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        ),
        _ => None,
    }
}

/// `done`/`drop`/`rm` shared target resolution: `task` (string or array),
/// falling back to `items`.
fn named_task_contents(args: &serde_json::Value) -> Vec<String> {
    task_list_from(args, "task").unwrap_or_else(|| {
        args.get("items")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    })
}

fn op_init(phases: &mut Vec<TodoPhase>, args: &serde_json::Value, errors: &mut Vec<String>) {
    // `list` wins; a flat `items` (optionally with a `phase`) is synthesized
    // into a single-phase list so the common model mistake is not an error.
    let flat = task_list_from(args, "items")
        .filter(|items| !items.is_empty())
        .map(|items| {
            vec![serde_json::json!({
                "phase": args.get("phase").and_then(|v| v.as_str()).unwrap_or(DEFAULT_PHASE),
                "items": items,
            })]
        });
    let list = match args.get("list").and_then(|v| v.as_array()) {
        Some(list) if !list.is_empty() => list.clone(),
        _ => flat.unwrap_or_default(),
    };
    if list.is_empty() {
        errors.push("missing list for init operation".to_string());
        return;
    }
    let mut seen_phases = std::collections::HashSet::new();
    let mut seen_tasks = std::collections::HashSet::new();
    for entry in list {
        let name = entry
            .get("phase")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_PHASE)
            .to_string();
        if !seen_phases.insert(name.clone()) {
            errors.push(format!("duplicate phase \"{name}\" in init list"));
        }
        let mut tasks = Vec::new();
        if let Some(items) = entry.get("items").and_then(|v| v.as_array()) {
            for item in items {
                let Some(content) = item.as_str() else {
                    continue;
                };
                if !seen_tasks.insert(content.to_string()) {
                    errors.push(format!("duplicate task \"{content}\" in init list"));
                }
                tasks.push(TodoItem {
                    content: content.to_string(),
                    status: TodoStatus::Pending,
                });
            }
        }
        phases.push(TodoPhase { name, tasks });
    }
}

fn op_start(phases: &mut [TodoPhase], args: &serde_json::Value, errors: &mut Vec<String>) {
    let Some(content) = args.get("task").and_then(|v| v.as_str()) else {
        errors.push("start requires a task".to_string());
        return;
    };
    let Some((pi, ti)) = find_task(phases, content) else {
        errors.push(format!("task not found: {content}"));
        return;
    };
    // Demote every other in-progress task to pending.
    for (i, phase) in phases.iter_mut().enumerate() {
        for (j, task) in phase.tasks.iter_mut().enumerate() {
            if task.status == TodoStatus::InProgress && (i, j) != (pi, ti) {
                task.status = TodoStatus::Pending;
            }
        }
    }
    phases[pi].tasks[ti].status = TodoStatus::InProgress;
}

fn op_set_status(
    phases: &mut [TodoPhase],
    args: &serde_json::Value,
    status: TodoStatus,
    errors: &mut Vec<String>,
    completed: &mut Vec<(String, String)>,
) {
    let contents = named_task_contents(args);
    if contents.is_empty() {
        errors.push(format!(
            "{} requires a task, items or a phase",
            status.label()
        ));
        return;
    }
    for content in contents {
        let Some((pi, ti)) = find_task(phases, &content) else {
            errors.push(format!("task not found: {content}"));
            continue;
        };
        phases[pi].tasks[ti].status = status;
        if status == TodoStatus::Completed {
            completed.push((
                phases[pi].name.clone(),
                phases[pi].tasks[ti].content.clone(),
            ));
        }
    }
}

fn op_rm(phases: &mut [TodoPhase], args: &serde_json::Value, errors: &mut Vec<String>) {
    let contents = named_task_contents(args);
    if !contents.is_empty() {
        for content in contents {
            let Some((pi, ti)) = find_task(phases, &content) else {
                errors.push(format!("task not found: {content}"));
                continue;
            };
            phases[pi].tasks.remove(ti);
        }
        return;
    }
    if let Some(name) = args.get("phase").and_then(|v| v.as_str()) {
        let Some(phase) = phases.iter_mut().find(|p| p.name == name) else {
            errors.push(format!("phase not found: {name}"));
            return;
        };
        phase.tasks.clear();
        return;
    }
    errors.push("rm requires a task, items or a phase".to_string());
}

fn op_append(phases: &mut Vec<TodoPhase>, args: &serde_json::Value, errors: &mut Vec<String>) {
    let Some(items) = task_list_from(args, "items").filter(|v| !v.is_empty()) else {
        errors.push("append requires items".to_string());
        return;
    };
    // Default phase: the in-progress task's phase, else the first pending
    // task's phase, else the last phase, else a new "Tasks" phase.
    let name = args
        .get("phase")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            phases.iter().find_map(|phase| {
                phase
                    .tasks
                    .iter()
                    .find(|t| t.status == TodoStatus::InProgress)
                    .map(|_| phase.name.clone())
            })
        })
        .or_else(|| {
            phases.iter().find_map(|phase| {
                phase
                    .tasks
                    .iter()
                    .find(|t| t.status == TodoStatus::Pending)
                    .map(|_| phase.name.clone())
            })
        })
        .or_else(|| phases.last().map(|p| p.name.clone()))
        .unwrap_or_else(|| DEFAULT_PHASE.to_string());
    let phase = match phases.iter_mut().find(|p| p.name == name) {
        Some(phase) => phase,
        None => {
            phases.push(TodoPhase {
                name: name.clone(),
                tasks: Vec::new(),
            });
            phases.last_mut().expect("just pushed")
        }
    };
    for content in items {
        phase.tasks.push(TodoItem {
            content,
            status: TodoStatus::Pending,
        });
    }
}

/// Apply one todo op to the global state. Operational errors (unknown task,
/// missing parameters) are collected in `TodoOpResult::errors`; only
/// unparseable arguments (`op` not a string) are a hard `Err`.
pub fn todo_op(args: &serde_json::Value) -> Result<TodoOpResult, String> {
    let Some(op) = args.get("op").and_then(|v| v.as_str()) else {
        // Infer a missing op from the argument shape.
        let inferred = if args
            .get("list")
            .and_then(|v| v.as_array())
            .is_some_and(|l| !l.is_empty())
        {
            "init"
        } else if args
            .get("items")
            .and_then(|v| v.as_array())
            .is_some_and(|l| !l.is_empty())
        {
            "append"
        } else if args.get("task").is_some() {
            "start"
        } else {
            return Err("missing op".to_string());
        };
        let mut owned = args.clone();
        owned["op"] = serde_json::Value::String(inferred.to_string());
        return todo_op(&owned);
    };

    let mut state = state();
    let mut result = TodoOpResult {
        phases: state.phases.clone(),
        errors: Vec::new(),
        completed: Vec::new(),
    };

    match op {
        "init" => {
            state.phases.clear();
            op_init(&mut state.phases, args, &mut result.errors);
        }
        "start" => op_start(&mut state.phases, args, &mut result.errors),
        "done" => op_set_status(
            &mut state.phases,
            args,
            TodoStatus::Completed,
            &mut result.errors,
            &mut result.completed,
        ),
        "drop" => op_set_status(
            &mut state.phases,
            args,
            TodoStatus::Abandoned,
            &mut result.errors,
            &mut result.completed,
        ),
        "rm" => op_rm(&mut state.phases, args, &mut result.errors),
        "append" => op_append(&mut state.phases, args, &mut result.errors),
        "view" => {}
        other => return Err(format!("unknown op: {other}")),
    }

    if op != "view" {
        auto_promote(&mut state.phases);
    }
    result.phases = state.phases.clone();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(args: serde_json::Value) -> TodoOpResult {
        todo_op(&args).expect("op should parse")
    }

    /// Tests share the process-global todo state and run in parallel threads;
    /// a global lock serializes each test body end to end.
    static TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn with_lock() -> parking_lot::MutexGuard<'static, ()> {
        TEST_LOCK.lock()
    }

    #[test]
    fn init_flat_creates_tasks_phase() {
        let _guard = with_lock();
        reset();
        let result = op(json!({"op": "init", "items": ["a", "b"]}));
        assert!(result.errors.is_empty());
        let phases = current_phases();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].name, "Tasks");
        assert_eq!(phases[0].tasks.len(), 2);
        // Auto-promote fires after init: the earliest task starts.
        assert_eq!(phases[0].tasks[0].status, TodoStatus::InProgress);
        assert_eq!(phases[0].tasks[1].status, TodoStatus::Pending);
    }

    #[test]
    fn init_with_list_creates_phases() {
        let _guard = with_lock();
        reset();
        let result = op(json!({"op": "init", "list": [
            {"phase": "Setup", "items": ["clone", "build"]},
            {"phase": "Verify", "items": ["test"]}
        ]}));
        assert!(result.errors.is_empty());
        let phases = current_phases();
        assert_eq!(phases.len(), 2);
        assert_eq!(phases[0].name, "Setup");
        assert_eq!(phases[1].tasks.len(), 1);
    }

    #[test]
    fn start_promotes_and_demotes_others() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a", "b", "c"]}));
        op(json!({"op": "start", "task": "a"}));
        // auto-promote also runs; b is now in progress via auto-promote? No:
        // start promotes a, so b stays pending (a is in progress).
        let phases = current_phases();
        assert_eq!(phases[0].tasks[0].status, TodoStatus::InProgress);
        assert_eq!(phases[0].tasks[1].status, TodoStatus::Pending);
        // Starting b demotes a back to pending.
        op(json!({"op": "start", "task": "b"}));
        let phases = current_phases();
        assert_eq!(phases[0].tasks[0].status, TodoStatus::Pending);
        assert_eq!(phases[0].tasks[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn done_completes_and_autopromotes_next_pending() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a", "b", "c"]}));
        op(json!({"op": "start", "task": "a"}));
        let result = op(json!({"op": "done", "task": "a"}));
        assert_eq!(
            result.completed,
            vec![("Tasks".to_string(), "a".to_string())]
        );
        let phases = current_phases();
        assert_eq!(phases[0].tasks[0].status, TodoStatus::Completed);
        // auto-promote promotes the earliest pending task (b).
        assert_eq!(phases[0].tasks[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn drop_marks_abandoned() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a", "b"]}));
        let result = op(json!({"op": "drop", "task": "a"}));
        assert!(result.errors.is_empty());
        assert_eq!(current_phases()[0].tasks[0].status, TodoStatus::Abandoned);
    }

    #[test]
    fn rm_removes_task() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a", "b"]}));
        let result = op(json!({"op": "rm", "task": "a"}));
        assert!(result.errors.is_empty());
        let phases = current_phases();
        assert_eq!(phases[0].tasks.len(), 1);
        assert_eq!(phases[0].tasks[0].content, "b");
    }

    #[test]
    fn append_adds_to_active_phase() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a"]}));
        op(json!({"op": "start", "task": "a"}));
        let result = op(json!({"op": "append", "items": ["b"]}));
        assert!(result.errors.is_empty());
        let phases = current_phases();
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].tasks.len(), 2);
        assert_eq!(phases[0].tasks[1].content, "b");
    }

    #[test]
    fn view_returns_unchanged() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a"]}));
        op(json!({"op": "start", "task": "a"}));
        let before = current_phases();
        let result = op(json!({"op": "view"}));
        assert!(result.errors.is_empty());
        assert_eq!(result.phases, before);
    }

    #[test]
    fn missing_op_error() {
        let _guard = with_lock();
        reset();
        assert_eq!(todo_op(&json!({})).unwrap_err(), "missing op");
    }

    #[test]
    fn unknown_op_error() {
        let _guard = with_lock();
        reset();
        assert_eq!(
            todo_op(&json!({"op": "frobnicate"})).unwrap_err(),
            "unknown op: frobnicate"
        );
    }

    #[test]
    fn task_not_found_error() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a"]}));
        let result = op(json!({"op": "start", "task": "nope"}));
        assert_eq!(result.errors, vec!["task not found: nope".to_string()]);
    }

    #[test]
    fn render_checklist_contains_checkboxes() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a", "b"]}));
        op(json!({"op": "start", "task": "a"}));
        let out = render_checklist();
        assert!(out.contains("[Tasks] 0/2"));
        assert!(out.contains("◐ a"));
        assert!(out.contains("☐ b"));
        op(json!({"op": "done", "task": "a"}));
        assert!(render_checklist().contains("☑ a"));
    }

    #[test]
    fn reset_clears_state() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a"]}));
        assert!(!current_phases().is_empty());
        reset();
        assert!(current_phases().is_empty());
        assert_eq!(render_checklist(), "(no todos)");
    }
}
