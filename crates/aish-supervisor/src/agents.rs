//! Sub-agent run tracking derived from the [`aish_core::LlmEvent`] stream.

use std::collections::HashMap;

use aish_core::{LlmEvent, LlmEventType};

/// Maximum number of trailing characters kept from the streamed assistant text.
const MAX_LAST_TEXT: usize = 2000;

/// Lifecycle of one sub-agent run, derived from LlmEvent stream.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum AgentState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl AgentState {
    /// True when the run has reached a terminal state.
    pub fn is_finished(&self) -> bool {
        !matches!(self, AgentState::Running)
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            AgentState::Running => "running",
            AgentState::Completed => "completed",
            AgentState::Failed => "failed",
            AgentState::Cancelled => "cancelled",
        };
        f.write_str(name)
    }
}

/// One sub-agent run, keyed by spawn_id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentRun {
    pub spawn_id: String,
    pub agent_type: String,
    pub depth: u32,
    pub skill_name: Option<String>,
    /// 3-5 word task summary when the parent supplied one.
    pub description: Option<String>,
    pub state: AgentState,
    /// Tool calls completed by this agent.
    pub tool_calls: u64,
    /// Streamed assistant text (bounded; last N chars).
    pub last_text: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

impl AgentRun {
    fn new(spawn_id: String, started_at_ms: u64) -> Self {
        Self {
            spawn_id,
            agent_type: String::new(),
            depth: 0,
            skill_name: None,
            description: None,
            state: AgentState::Running,
            tool_calls: 0,
            last_text: String::new(),
            started_at_ms,
            finished_at_ms: None,
        }
    }

    fn merge_metadata(&mut self, event: &LlmEvent) -> bool {
        let mut changed = false;
        if let Some(agent_type) = event.data.get("agent_type").and_then(|v| v.as_str()) {
            if self.agent_type != agent_type {
                self.agent_type = agent_type.to_string();
                changed = true;
            }
        }
        if let Some(depth) = event.data.get("depth").and_then(|v| v.as_u64()) {
            let depth = depth as u32;
            if self.depth != depth {
                self.depth = depth;
                changed = true;
            }
        }
        if let Some(skill_name) = event.data.get("skill_name").and_then(|v| v.as_str()) {
            if self.skill_name.as_deref() != Some(skill_name) {
                self.skill_name = Some(skill_name.to_string());
                changed = true;
            }
        }
        changed
    }

    /// Move the run to a terminal state. Terminal states are sticky: a
    /// failure reported by `Error` is not overwritten by the `OpEnd` that
    /// follows it.
    fn finish(&mut self, state: AgentState, at_ms: u64) -> bool {
        if self.state != AgentState::Running {
            return false;
        }
        self.state = state;
        self.finished_at_ms = Some(at_ms);
        true
    }

    /// Append streamed text, keeping only the trailing [`MAX_LAST_TEXT`] chars.
    fn push_text(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        self.last_text.push_str(text);
        if self.last_text.chars().count() > MAX_LAST_TEXT {
            let skip = self.last_text.chars().count() - MAX_LAST_TEXT;
            let truncated: String = self.last_text.chars().skip(skip).collect();
            self.last_text = truncated;
        }
        true
    }
}

/// Extract the streamed assistant text from a `ContentDelta` payload.
fn content_delta_text(event: &LlmEvent) -> Option<&str> {
    ["text", "delta", "content"]
        .iter()
        .find_map(|key| event.data.get(*key).and_then(|v| v.as_str()))
}

/// Convert an event's float-seconds timestamp to milliseconds.
fn event_ms(event: &LlmEvent) -> u64 {
    if event.timestamp <= 0.0 {
        return 0;
    }
    (event.timestamp * 1000.0) as u64
}

fn is_sub_agent_event(event: &LlmEvent) -> bool {
    event.data.get("source").and_then(|v| v.as_str()) == Some("sub_agent")
}

/// Tracks sub-agent runs from LlmEvents.
#[derive(Debug, Default)]
pub struct AgentTracker {
    runs: HashMap<String, AgentRun>,
    /// Descriptions harvested from parent `Agent` tool calls, waiting for the
    /// sub-agent run they spawned to appear.
    pending_descriptions: Vec<String>,
}

impl AgentTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one event. Returns true when the snapshot changed.
    pub fn observe(&mut self, event: &LlmEvent) -> bool {
        if !is_sub_agent_event(event) {
            return self.observe_parent_event(event);
        }
        let Some(spawn_id) = event
            .data
            .get("spawn_id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
        else {
            return false;
        };
        let at_ms = event_ms(event);
        let mut changed = false;

        // `GenerationStart` is the canonical run creator; any other sub-agent
        // event also materializes the run so a dropped start event cannot
        // hide a live agent from the panel.
        if !self.runs.contains_key(&spawn_id) {
            let mut run = AgentRun::new(spawn_id.clone(), at_ms);
            run.merge_metadata(event);
            self.runs.insert(spawn_id.clone(), run);
            self.attach_pending_description(&spawn_id);
            changed = true;
        }

        let Some(run) = self.runs.get_mut(&spawn_id) else {
            return changed;
        };
        changed |= run.merge_metadata(event);

        match event.event_type {
            LlmEventType::GenerationStart => {}
            LlmEventType::ContentDelta => {
                if let Some(text) = content_delta_text(event) {
                    changed |= run.push_text(text);
                }
            }
            LlmEventType::ToolExecutionStart => {
                run.tool_calls += 1;
                changed = true;
            }
            LlmEventType::OpEnd => {
                let state =
                    if event.data.get("reason").and_then(|v| v.as_str()) == Some("cancelled") {
                        AgentState::Cancelled
                    } else {
                        AgentState::Completed
                    };
                changed |= run.finish(state, at_ms);
            }
            LlmEventType::Cancelled => {
                changed |= run.finish(AgentState::Cancelled, at_ms);
            }
            LlmEventType::Error => {
                changed |= run.finish(AgentState::Failed, at_ms);
            }
            _ => {}
        }
        changed
    }

    /// Parent-session `Agent` tool calls carry the task description the
    /// sub-agent run itself never repeats; remember it for the next spawn.
    fn observe_parent_event(&mut self, event: &LlmEvent) -> bool {
        if event.event_type != LlmEventType::ToolExecutionStart {
            return false;
        }
        if event.data.get("tool_name").and_then(|v| v.as_str()) != Some("Agent") {
            return false;
        }
        let Some(description) = event
            .data
            .get("tool_args")
            .and_then(|args| args.get("description"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|d| !d.is_empty())
        else {
            return false;
        };
        // Attach to a run that has no description yet, newest first.
        let mut best: Option<(&str, u64)> = None;
        for run in self.runs.values() {
            if run.description.is_some() {
                continue;
            }
            let is_newer = best
                .map(|(_, started)| run.started_at_ms > started)
                .unwrap_or(true);
            if is_newer {
                best = Some((run.spawn_id.as_str(), run.started_at_ms));
            }
        }
        if let Some((spawn_id, _)) = best {
            let spawn_id = spawn_id.to_string();
            if let Some(run) = self.runs.get_mut(&spawn_id) {
                run.description = Some(description.to_string());
                return true;
            }
        }
        if self.pending_descriptions.len() >= 32 {
            self.pending_descriptions.remove(0);
        }
        self.pending_descriptions.push(description.to_string());
        false
    }

    /// Give a freshly created run the oldest description harvested from the
    /// parent's `Agent` tool call, when one is pending.
    fn attach_pending_description(&mut self, spawn_id: &str) {
        if self.pending_descriptions.is_empty() {
            return;
        }
        let description = self.pending_descriptions.remove(0);
        if let Some(run) = self.runs.get_mut(spawn_id) {
            run.description = Some(description);
        }
    }

    /// All tracked runs, ordered by start time (oldest first).
    pub fn runs(&self) -> Vec<AgentRun> {
        let mut runs: Vec<AgentRun> = self.runs.values().cloned().collect();
        runs.sort_by(|a, b| {
            a.started_at_ms
                .cmp(&b.started_at_ms)
                .then_with(|| a.spawn_id.cmp(&b.spawn_id))
        });
        runs
    }

    /// Number of runs still in [`AgentState::Running`].
    pub fn running_count(&self) -> usize {
        self.runs
            .values()
            .filter(|run| run.state == AgentState::Running)
            .count()
    }

    /// Drop finished runs, keeping only live ones.
    pub fn clear_finished(&mut self) {
        self.runs.retain(|_, run| !run.state.is_finished());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sub_agent_event(event_type: LlmEventType, extra: serde_json::Value, ts: f64) -> LlmEvent {
        let mut data = json!({
            "source": "sub_agent",
            "agent_type": "explore",
            "depth": 1,
            "spawn_id": "spawn-1",
        });
        if let (Some(map), serde_json::Value::Object(extra)) = (data.as_object_mut(), extra) {
            for (key, value) in extra {
                map.insert(key, value);
            }
        }
        LlmEvent {
            event_type,
            data,
            timestamp: ts,
            metadata: None,
        }
    }

    #[test]
    fn create_on_generation_start() {
        let mut tracker = AgentTracker::new();
        let event = sub_agent_event(LlmEventType::GenerationStart, json!({}), 1000.5);
        assert!(tracker.observe(&event));

        let runs = tracker.runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].spawn_id, "spawn-1");
        assert_eq!(runs[0].agent_type, "explore");
        assert_eq!(runs[0].depth, 1);
        assert_eq!(runs[0].state, AgentState::Running);
        assert_eq!(runs[0].started_at_ms, 1_000_500);
        assert_eq!(tracker.running_count(), 1);
    }

    #[test]
    fn content_accumulates_bounded() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));

        let chunk = "x".repeat(1500);
        tracker.observe(&sub_agent_event(
            LlmEventType::ContentDelta,
            json!({"delta": chunk}),
            1.5,
        ));
        assert_eq!(tracker.runs()[0].last_text.chars().count(), 1500);

        tracker.observe(&sub_agent_event(
            LlmEventType::ContentDelta,
            json!({"text": chunk}),
            1.6,
        ));
        let text = &tracker.runs()[0].last_text;
        assert_eq!(text.chars().count(), MAX_LAST_TEXT);

        // Content in a third field name is also accepted.
        tracker.observe(&sub_agent_event(
            LlmEventType::ContentDelta,
            json!({"content": "tail"}),
            1.7,
        ));
        let text = &tracker.runs()[0].last_text;
        assert!(text.ends_with("tail"));
        assert_eq!(text.chars().count(), MAX_LAST_TEXT);
    }

    #[test]
    fn tool_calls_count() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        for _ in 0..3 {
            tracker.observe(&sub_agent_event(
                LlmEventType::ToolExecutionStart,
                json!({"tool_name": "grep"}),
                1.1,
            ));
        }
        assert_eq!(tracker.runs()[0].tool_calls, 3);
    }

    #[test]
    fn op_end_completes() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        assert!(tracker.observe(&sub_agent_event(
            LlmEventType::OpEnd,
            json!({"reason": "complete"}),
            2.0
        )));
        let run = &tracker.runs()[0];
        assert_eq!(run.state, AgentState::Completed);
        assert_eq!(run.finished_at_ms, Some(2000));
        assert_eq!(tracker.running_count(), 0);
    }

    #[test]
    fn error_marks_failed() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        tracker.observe(&sub_agent_event(
            LlmEventType::Error,
            json!({"error_type": "api_error", "error_message": "boom"}),
            2.0,
        ));
        let run = &tracker.runs()[0];
        assert_eq!(run.state, AgentState::Failed);
        // A later `OpEnd` must not erase the failure.
        assert!(!tracker.observe(&sub_agent_event(
            LlmEventType::OpEnd,
            json!({"reason": "inconclusive"}),
            2.1,
        )));
        assert_eq!(tracker.runs()[0].state, AgentState::Failed);
    }

    #[test]
    fn cancelled_reason() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        tracker.observe(&sub_agent_event(
            LlmEventType::OpEnd,
            json!({"reason": "cancelled"}),
            2.0,
        ));
        assert_eq!(tracker.runs()[0].state, AgentState::Cancelled);
        assert_eq!(tracker.running_count(), 0);
    }

    #[test]
    fn non_subagent_events_ignored() {
        let mut tracker = AgentTracker::new();
        let event = LlmEvent {
            event_type: LlmEventType::ToolExecutionStart,
            data: json!({"tool_name": "grep", "tool_args": {"description": "not a spawn"}}),
            timestamp: 1.0,
            metadata: None,
        };
        assert!(!tracker.observe(&event));
        assert!(tracker.runs().is_empty());
    }

    #[test]
    fn runs_sorted_by_start() {
        let mut tracker = AgentTracker::new();
        for (spawn_id, ts) in [("b", 3.0), ("a", 1.0), ("c", 2.0)] {
            let mut event = sub_agent_event(LlmEventType::GenerationStart, json!({}), ts);
            event.data["spawn_id"] = json!(spawn_id);
            tracker.observe(&event);
        }
        let ids: Vec<String> = tracker.runs().into_iter().map(|r| r.spawn_id).collect();
        assert_eq!(ids, vec!["a", "c", "b"]);
    }

    #[test]
    fn parent_agent_call_supplies_description() {
        let mut tracker = AgentTracker::new();
        let parent_call = LlmEvent {
            event_type: LlmEventType::ToolExecutionStart,
            data: json!({
                "tool_name": "Agent",
                "tool_args": {"description": "audit retry paths", "subagent_type": "explore"},
            }),
            timestamp: 0.9,
            metadata: None,
        };
        tracker.observe(&parent_call);
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));

        let run = &tracker.runs()[0];
        assert_eq!(run.description.as_deref(), Some("audit retry paths"));
    }

    #[test]
    fn clear_finished_drops_terminal_runs() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        tracker.observe(&sub_agent_event(LlmEventType::OpEnd, json!({}), 1.5));

        let mut second = sub_agent_event(LlmEventType::GenerationStart, json!({}), 2.0);
        second.data["spawn_id"] = json!("spawn-2");
        tracker.observe(&second);

        assert_eq!(tracker.runs().len(), 2);
        tracker.clear_finished();
        let runs = tracker.runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].spawn_id, "spawn-2");
    }

    #[test]
    fn repeated_op_end_does_not_reemit_change() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        assert!(tracker.observe(&sub_agent_event(LlmEventType::OpEnd, json!({}), 2.0)));
        assert!(!tracker.observe(&sub_agent_event(LlmEventType::OpEnd, json!({}), 3.0)));
    }
}
