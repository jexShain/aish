//! Model-maintained todo state machine (oh-my-pi style).
//!
//! A process-global map of phase lists (`LazyLock` + `parking_lot::Mutex`)
//! keyed by *session key* holds the todo state for the lifetime of the aish
//! process. The main shell session owns [`MAIN_SESSION_KEY`]; every sub-agent
//! gets a private key through `Tool::for_sub_session`, so a child agent's
//! `init` can never wipe the parent's checklist. The model mutates state via
//! the `todo` tool (`tools.rs`); the shell renders it with
//! [`render_checklist`]. Tasks are identified by verbatim content, never IDs.

pub mod tools;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Snapshot of the checklist *as the op left it*. Callers must render
    /// this instead of re-reading the global state, otherwise a concurrent
    /// agent can change the list between the mutation and the render.
    pub phases: Vec<TodoPhase>,
    pub errors: Vec<String>,
    /// `(phase_name, task_content)` for every task that became completed.
    pub completed: Vec<(String, String)>,
}

/// Default phase name for flat `init` and `append` to an empty list.
const DEFAULT_PHASE: &str = "Tasks";

/// Session key of the user-facing shell session.
pub const MAIN_SESSION_KEY: &str = "main";

/// Prefix for sub-agent session keys.
const SUB_SESSION_PREFIX: &str = "sub-";

/// Upper bound on retained session keys, so a long-lived process that spawns
/// many sub-agents cannot grow the map without bound.
const MAX_SESSION_KEYS: usize = 64;

/// Serializes test bodies that touch the process-global map (the key-pruning
/// test evicts entries, which would otherwise race sibling tests).
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Take [`TEST_LOCK`].
#[cfg(test)]
pub(crate) fn serialize_test() -> parking_lot::MutexGuard<'static, ()> {
    TEST_LOCK.lock()
}

/// Bumped for every sub-agent todo instance; only used to build unique keys.
static SUB_SESSION_SEQ: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh, process-unique session key for a sub-agent.
fn new_sub_session_key() -> String {
    format!(
        "{SUB_SESSION_PREFIX}{}",
        SUB_SESSION_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Default)]
struct State {
    /// Phase list per session key.
    sessions: HashMap<String, Vec<TodoPhase>>,
    /// Insertion order of `sessions` keys, used to prune the oldest.
    order: VecDeque<String>,
}

impl State {
    /// Phase list of `key` (empty slice when the session has no todos).
    fn phases(&self, key: &str) -> &[TodoPhase] {
        self.sessions.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Mutable phase list of `key`, creating it on first use.
    fn phases_mut(&mut self, key: &str) -> &mut Vec<TodoPhase> {
        if !self.sessions.contains_key(key) {
            self.sessions.insert(key.to_string(), Vec::new());
            self.order.push_back(key.to_string());
            self.prune_keys();
        }
        self.sessions
            .get_mut(key)
            .expect("session key inserted above")
    }

    /// Drop the oldest non-main session keys beyond [`MAX_SESSION_KEYS`].
    fn prune_keys(&mut self) {
        while self.order.len() > MAX_SESSION_KEYS {
            let Some(oldest) = self.order.pop_front() else {
                return;
            };
            if oldest == MAIN_SESSION_KEY {
                // Never evict the user-facing checklist.
                self.order.push_back(oldest);
                continue;
            }
            self.sessions.remove(&oldest);
        }
    }
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

fn state() -> parking_lot::MutexGuard<'static, State> {
    STATE.lock()
}

/// Read-only snapshot of the main session's phases.
pub fn current_phases() -> Vec<TodoPhase> {
    current_phases_for(MAIN_SESSION_KEY)
}

/// Read-only snapshot of one session's phases.
pub fn current_phases_for(session_key: &str) -> Vec<TodoPhase> {
    state().phases(session_key).to_vec()
}

/// Clear the main session's todo state.
pub fn reset() {
    reset_session(MAIN_SESSION_KEY);
}

/// Clear one session's todo state.
pub fn reset_session(session_key: &str) {
    let mut state = state();
    if let Some(phases) = state.sessions.get_mut(session_key) {
        phases.clear();
    }
}

/// Render the main session's checklist.
pub fn render_checklist() -> String {
    render_phases(&current_phases())
}

/// Render a checklist snapshot; one `[name] done/total` header plus one
/// checkbox line per task. Returns `"(no todos)"` when the snapshot has no
/// tasks at all.
///
/// Content is sanitized here as well as at op time: the shell writes these
/// lines straight to the terminal, so embedded ESC/OSC sequences or newlines
/// from model-authored text must never reach it.
pub fn render_phases(phases: &[TodoPhase]) -> String {
    let mut out = String::new();
    for phase in phases {
        if phase.tasks.is_empty() {
            // A phase with no tasks renders as noise ("[Empty] 0/0").
            continue;
        }
        let total = phase.tasks.len();
        let done = phase
            .tasks
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        out.push_str(&format!(
            "[{}] {}/{}\n",
            sanitize_content(&phase.name),
            done,
            total
        ));
        for task in &phase.tasks {
            out.push_str(&format!(
                "  {} {}\n",
                task.status.marker(),
                sanitize_content(&task.content)
            ));
        }
    }
    if out.is_empty() {
        return "(no todos)".to_string();
    }
    // Trim the trailing newline of the last line.
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Replace characters that would break the single-line checklist invariant or
/// let untrusted content drive the terminal.
///
/// Newlines/CR/TAB become spaces (the card renderer prefixes one rail per
/// line, so an embedded newline would escape the box); other control
/// characters (including ESC, which starts SGR/OSC sequences) are dropped.
fn sanitize_content(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\n' | '\r' | '\t' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
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

/// Build the phase list an `init` op would install.
///
/// Returns `None` when the arguments carry no usable list: the caller must
/// then leave the existing checklist untouched (P0-1 — a malformed `init`
/// used to wipe the user-visible plan).
fn build_init_phases(args: &serde_json::Value, errors: &mut Vec<String>) -> Option<Vec<TodoPhase>> {
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
        return None;
    }
    let mut next: Vec<TodoPhase> = Vec::new();
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
                let Some(raw) = item.as_str() else {
                    continue;
                };
                let content = sanitize_content(raw);
                if content.is_empty() {
                    continue;
                }
                if !seen_tasks.insert(content.clone()) {
                    // Skipping (not merely warning) keeps `done <content>`
                    // resolvable: a duplicate would shadow the second copy.
                    errors.push(format!("duplicate task \"{content}\" in init list"));
                    continue;
                }
                tasks.push(TodoItem {
                    content,
                    status: TodoStatus::Pending,
                });
            }
        }
        next.push(TodoPhase { name, tasks });
    }
    if next.iter().all(|p| p.tasks.is_empty()) {
        errors.push("init list contains no tasks".to_string());
        return None;
    }
    Some(next)
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
    let mut contents = named_task_contents(args);
    if contents.is_empty() {
        // `done`/`drop` with only a `phase` targets every task in it.
        if let Some(name) = args.get("phase").and_then(|v| v.as_str()) {
            let Some(phase) = phases.iter().find(|p| p.name == name) else {
                errors.push(format!("phase not found: {name}"));
                return;
            };
            contents = phase.tasks.iter().map(|t| t.content.clone()).collect();
        }
    }
    if contents.is_empty() {
        errors.push(format!(
            "{} requires a task, items or a phase with tasks",
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
            let entry = (
                phases[pi].name.clone(),
                phases[pi].tasks[ti].content.clone(),
            );
            // `done ["a", "a"]` (or a repeated call) must not double-report.
            if !completed.contains(&entry) {
                completed.push(entry);
            }
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
    // task's phase, else the last phase, else a new "Tasks" phase. An empty
    // `phase` string falls through to the same chain instead of creating an
    // unnamed phase.
    let name = args
        .get("phase")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
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
    let phase_idx = match phases.iter().position(|p| p.name == name) {
        Some(idx) => idx,
        None => {
            phases.push(TodoPhase {
                name,
                tasks: Vec::new(),
            });
            phases.len() - 1
        }
    };
    for raw in items {
        let content = sanitize_content(&raw);
        if content.is_empty() {
            continue;
        }
        // `init` rejects duplicate content; `append` must too, otherwise the
        // shadowed copy can never be completed (`done` always hits the first).
        if find_task(phases, &content).is_some() {
            errors.push(format!("duplicate task \"{content}\""));
            continue;
        }
        phases[phase_idx].tasks.push(TodoItem {
            content,
            status: TodoStatus::Pending,
        });
    }
}

/// Apply one todo op to the main session's state.
pub fn todo_op(args: &serde_json::Value) -> Result<TodoOpResult, String> {
    todo_op_for(MAIN_SESSION_KEY, args)
}

/// Apply one todo op to `session_key`'s state. Operational errors (unknown
/// task, missing parameters) are collected in `TodoOpResult::errors`; only
/// unparseable arguments (`op` not a string) are a hard `Err`.
pub fn todo_op_for(session_key: &str, args: &serde_json::Value) -> Result<TodoOpResult, String> {
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
        return todo_op_for(session_key, &owned);
    };

    let mut state = state();
    let phases = state.phases_mut(session_key);
    let mut result = TodoOpResult {
        phases: Vec::new(),
        errors: Vec::new(),
        completed: Vec::new(),
    };

    match op {
        "init" => {
            // Validate first, install second: a malformed init must not
            // destroy the checklist the user is looking at.
            if let Some(next) = build_init_phases(args, &mut result.errors) {
                *phases = next;
            }
        }
        "start" => op_start(phases, args, &mut result.errors),
        "done" => op_set_status(
            phases,
            args,
            TodoStatus::Completed,
            &mut result.errors,
            &mut result.completed,
        ),
        "drop" => op_set_status(
            phases,
            args,
            TodoStatus::Abandoned,
            &mut result.errors,
            &mut result.completed,
        ),
        "rm" => op_rm(phases, args, &mut result.errors),
        "append" => op_append(phases, args, &mut result.errors),
        "view" => {}
        other => return Err(format!("unknown op: {other}")),
    }

    // A rejected op must not mutate state the model did not ask to change.
    if op != "view" && result.errors.is_empty() {
        auto_promote(phases);
    }
    result.phases = phases.clone();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(args: serde_json::Value) -> TodoOpResult {
        todo_op(&args).expect("op should parse")
    }

    fn op_for(key: &str, args: serde_json::Value) -> TodoOpResult {
        todo_op_for(key, &args).expect("op should parse")
    }

    /// Tests share the process-global todo state and run in parallel threads;
    /// a global lock serializes each test body end to end. It is shared with
    /// `todo::tools` tests, which mutate the same map.
    fn with_lock() -> parking_lot::MutexGuard<'static, ()> {
        serialize_test()
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

    // -----------------------------------------------------------------------
    // P0-1: a malformed init must not destroy the existing checklist.
    // -----------------------------------------------------------------------

    #[test]
    fn init_malformed_preserves_previous_phases() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["keep-me", "and-me"]}));
        let before = current_phases();

        let result = op(json!({"op": "init"}));
        assert_eq!(
            result.errors,
            vec!["missing list for init operation".to_string()]
        );
        assert_eq!(current_phases(), before, "malformed init wiped the list");
        assert!(render_checklist().contains("keep-me"));

        // An empty list is just as malformed as a missing one.
        let result = op(json!({"op": "init", "list": []}));
        assert!(!result.errors.is_empty());
        assert_eq!(current_phases(), before);
    }

    // -----------------------------------------------------------------------
    // P0-2: state is keyed by session so sub-agents cannot clobber the parent.
    // -----------------------------------------------------------------------

    #[test]
    fn sub_session_state_is_isolated_from_main() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["parent-task"]}));

        op_for("sub-test-1", json!({"op": "init", "items": ["child-task"]}));

        let main = current_phases();
        assert_eq!(main.len(), 1);
        assert_eq!(main[0].tasks[0].content, "parent-task");
        let child = current_phases_for("sub-test-1");
        assert_eq!(child[0].tasks[0].content, "child-task");

        // A child reset only clears its own list.
        reset_session("sub-test-1");
        assert!(current_phases_for("sub-test-1").is_empty());
        assert_eq!(current_phases()[0].tasks[0].content, "parent-task");
    }

    #[test]
    fn sub_session_keys_are_unique_and_bounded() {
        let a = new_sub_session_key();
        let b = new_sub_session_key();
        assert_ne!(a, b);
        assert!(a.starts_with(SUB_SESSION_PREFIX));

        // A private State so the assertion does not race sibling tests that
        // share the process-global map. Allocating far more keys than the cap
        // must not grow it without bound, and must never evict "main".
        let mut state = State::default();
        state.phases_mut(MAIN_SESSION_KEY).push(TodoPhase {
            name: DEFAULT_PHASE.to_string(),
            tasks: vec![TodoItem {
                content: "survivor".to_string(),
                status: TodoStatus::Pending,
            }],
        });
        for _ in 0..(MAX_SESSION_KEYS * 3) {
            let key = new_sub_session_key();
            state.phases_mut(&key).push(TodoPhase {
                name: DEFAULT_PHASE.to_string(),
                tasks: Vec::new(),
            });
        }
        assert!(state.sessions.len() <= MAX_SESSION_KEYS);
        assert_eq!(
            state.phases(MAIN_SESSION_KEY)[0].tasks[0].content,
            "survivor"
        );
    }

    // -----------------------------------------------------------------------
    // P0-3: rendering must use the op's snapshot, not a second global read.
    // -----------------------------------------------------------------------

    #[test]
    fn render_phases_renders_only_the_given_snapshot() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["global-task"]}));

        // The snapshot argument wins over the live state.
        assert_eq!(render_phases(&[]), "(no todos)");
        let snapshot = vec![TodoPhase {
            name: "Snapshot".to_string(),
            tasks: vec![TodoItem {
                content: "own-task".to_string(),
                status: TodoStatus::InProgress,
            }],
        }];
        let rendered = render_phases(&snapshot);
        assert!(rendered.contains("own-task"));
        assert!(!rendered.contains("global-task"));
        // ...while the global state is untouched.
        assert!(render_checklist().contains("global-task"));
    }

    // -----------------------------------------------------------------------
    // P1-1/P1-3: model-authored content must never reach the terminal raw.
    // -----------------------------------------------------------------------

    #[test]
    fn render_strips_terminal_control_sequences() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": [
            "\u{1b}[31mRED\u{1b}[0m",
            "\u{1b}]8;;https://evil\u{7}click\u{1b}]8;;\u{7}",
            "first\nsecond",
            "with\ttab",
        ]}));
        let out = render_checklist();
        assert!(!out.contains('\u{1b}'), "escape byte leaked: {out:?}");
        assert!(!out.contains('\u{7}'), "BEL leaked: {out:?}");
        assert!(out.contains("RED"));
        assert!(out.contains("click"));
        // Every checklist line stays a single physical line.
        assert_eq!(out.lines().count(), 1 + 4);
        assert!(out.lines().all(|l| !l.contains('\t')));
    }

    // -----------------------------------------------------------------------
    // P1-5: a rejected op must not auto-promote.
    // -----------------------------------------------------------------------

    #[test]
    fn auto_promote_skipped_when_op_reports_errors() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a", "b"]}));
        // `a` is auto-promoted by init.
        assert_eq!(current_phases()[0].tasks[0].status, TodoStatus::InProgress);

        let result = op(json!({"op": "done", "task": "ghost"}));
        assert_eq!(result.errors, vec!["task not found: ghost".to_string()]);
        // Nothing moved: `a` is still the single in-progress task.
        let phases = current_phases();
        assert_eq!(phases[0].tasks[0].status, TodoStatus::InProgress);
        assert_eq!(phases[0].tasks[1].status, TodoStatus::Pending);
    }

    // -----------------------------------------------------------------------
    // P2: schema/behaviour consistency.
    // -----------------------------------------------------------------------

    #[test]
    fn append_rejects_duplicate_task_content() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a"]}));
        let result = op(json!({"op": "append", "items": ["a", "b"]}));
        assert_eq!(result.errors, vec!["duplicate task \"a\"".to_string()]);
        let tasks = &current_phases()[0].tasks;
        assert_eq!(tasks.len(), 2, "duplicate must not be stored: {tasks:?}");
        assert_eq!(tasks[1].content, "b");
    }

    #[test]
    fn append_empty_phase_falls_back_to_default_phase() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a"]}));
        op(json!({"op": "append", "items": ["b"], "phase": ""}));
        let phases = current_phases();
        assert_eq!(phases.len(), 1, "empty phase name created a phase");
        assert_eq!(phases[0].name, "Tasks");
    }

    #[test]
    fn done_with_phase_marks_every_task_in_phase() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "list": [
            {"phase": "Setup", "items": ["a", "b"]},
            {"phase": "Verify", "items": ["c"]}
        ]}));
        let result = op(json!({"op": "done", "phase": "Setup"}));
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.completed.len(), 2);
        let phases = current_phases();
        assert_eq!(phases[0].tasks[0].status, TodoStatus::Completed);
        assert_eq!(phases[0].tasks[1].status, TodoStatus::Completed);
        assert_ne!(phases[1].tasks[0].status, TodoStatus::Completed);

        // Unknown phase is still an error.
        let result = op(json!({"op": "done", "phase": "Nope"}));
        assert_eq!(result.errors, vec!["phase not found: Nope".to_string()]);
    }

    #[test]
    fn render_skips_phases_without_tasks() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "list": [
            {"phase": "Empty", "items": []},
            {"phase": "Real", "items": ["a"]}
        ]}));
        let out = render_checklist();
        assert!(
            !out.contains("[Empty]"),
            "empty phase header rendered: {out}"
        );
        assert!(out.contains("[Real] 0/1"));
    }

    #[test]
    fn completed_list_deduplicates() {
        let _guard = with_lock();
        reset();
        op(json!({"op": "init", "items": ["a"]}));
        let result = op(json!({"op": "done", "task": ["a", "a"]}));
        assert_eq!(result.completed.len(), 1);
        let result = op(json!({"op": "done", "task": "a"}));
        assert_eq!(result.completed.len(), 1, "re-completion re-reported");
    }
}
