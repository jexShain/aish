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

/// Current wall-clock time in milliseconds, on the same scale as [`event_ms`].
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn is_sub_agent_event(event: &LlmEvent) -> bool {
    event.data.get("source").and_then(|v| v.as_str()) == Some("sub_agent")
}

/// A terminal event that retires a run: either explicitly labelled
/// `source=sub_agent`, or carrying a `spawn_id` without a `source` at all.
///
/// The second form is the fallback for terminal events emitted by paths that
/// never installed the sub-agent event proxy. A `source` that names something
/// else (e.g. a parent-session event) is never treated as sub-agent terminal.
fn is_sub_agent_terminal_event(event: &LlmEvent) -> bool {
    if event.event_type != LlmEventType::OpEnd {
        return false;
    }
    if event
        .data
        .get("spawn_id")
        .and_then(|v| v.as_str())
        .is_none()
    {
        return false;
    }
    match event.data.get("source") {
        None => true,
        Some(source) => source.as_str() == Some("sub_agent"),
    }
}

/// Map one event onto an existing run, returning whether it changed anything.
fn apply_event(run: &mut AgentRun, event: &LlmEvent, at_ms: u64) -> bool {
    match event.event_type {
        LlmEventType::GenerationStart => false,
        LlmEventType::ContentDelta => match content_delta_text(event) {
            Some(text) => run.push_text(text),
            None => false,
        },
        LlmEventType::ToolExecutionStart => {
            run.tool_calls += 1;
            true
        }
        LlmEventType::OpEnd => {
            let state = if event.data.get("reason").and_then(|v| v.as_str()) == Some("cancelled") {
                AgentState::Cancelled
            } else {
                AgentState::Completed
            };
            run.finish(state, at_ms)
        }
        LlmEventType::Cancelled => run.finish(AgentState::Cancelled, at_ms),
        LlmEventType::Error => run.finish(AgentState::Failed, at_ms),
        _ => false,
    }
}

/// Tracks sub-agent runs from LlmEvents.
#[derive(Debug, Default)]
pub struct AgentTracker {
    runs: HashMap<String, AgentRun>,
    /// Wall-clock ms of the last event seen per run, used by [`finish_stale`].
    last_activity_ms: HashMap<String, u64>,
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
            return if is_sub_agent_terminal_event(event) {
                self.observe_terminal_without_source(event)
            } else {
                self.observe_parent_event(event)
            };
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
        self.last_activity_ms.insert(spawn_id.clone(), at_ms);

        let Some(run) = self.runs.get_mut(&spawn_id) else {
            return changed;
        };
        changed |= run.merge_metadata(event);
        changed |= apply_event(run, event, at_ms);
        changed
    }

    /// Terminal `OpEnd` that carries a `spawn_id` but no `source`: some spawn
    /// paths emit their run identity without installing the sub-agent event
    /// proxy. Such an event may only retire a known run — it never creates one,
    /// because without `source` we cannot tell an agent event from a
    /// parent-session event that happens to carry a `spawn_id`.
    fn observe_terminal_without_source(&mut self, event: &LlmEvent) -> bool {
        let Some(spawn_id) = event.data.get("spawn_id").and_then(|v| v.as_str()) else {
            return false;
        };
        let at_ms = event_ms(event);
        let Some(run) = self.runs.get_mut(spawn_id) else {
            return false;
        };
        self.last_activity_ms
            .insert(spawn_id.to_string(), event_ms(event));
        let mut changed = run.merge_metadata(event);
        changed |= apply_event(run, event, at_ms);
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

    /// Drop runs that finished more than `retention_ms` ago.
    ///
    /// A finished run stays visible long enough to be read ("completed · 3
    /// tools") and then leaves the live view, so a shell with history does not
    /// accumulate rows forever. Runs that never finished are always kept.
    pub fn prune_finished(&mut self, retention_ms: u64) -> usize {
        let now_ms = wall_clock_ms();
        let before = self.runs.len();
        self.runs.retain(|_, run| match run.finished_at_ms {
            Some(finished_at) => now_ms.saturating_sub(finished_at) < retention_ms,
            None => true,
        });
        let removed = before - self.runs.len();
        if removed > 0 {
            self.last_activity_ms
                .retain(|spawn_id, _| self.runs.contains_key(spawn_id));
        }
        removed
    }

    /// Fail-safe for a run that never reported a terminal event: finish every
    /// `Running` run whose last event is older than `idle_ms`.
    ///
    /// Returns the number of runs retired. `idle_ms == 0` retires nothing, so a
    /// caller that has not decided on a threshold cannot flush live agents by
    /// accident. A run with no recorded activity is retired immediately, since
    /// no event ever reached the tracker for it.
    pub fn finish_stale(&mut self, idle_ms: u64) -> usize {
        if idle_ms == 0 {
            return 0;
        }
        let now_ms = wall_clock_ms();
        let mut retired = 0;
        for (spawn_id, run) in self.runs.iter_mut() {
            if run.state != AgentState::Running {
                continue;
            }
            let last = self
                .last_activity_ms
                .get(spawn_id)
                .copied()
                .unwrap_or(run.started_at_ms);
            if now_ms.saturating_sub(last) < idle_ms {
                continue;
            }
            if run.finish(AgentState::Completed, now_ms) {
                retired += 1;
            }
        }
        retired
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

    #[test]
    fn running_count_returns_zero_after_terminal_event() {
        // The HUD derives busy/idle from this count alone: a run that reports
        // its terminal `OpEnd` must never linger as `running`.
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationEnd,
            json!({}),
            1.4,
        ));
        assert_eq!(
            tracker.running_count(),
            1,
            "quiet between turns is not done"
        );

        tracker.observe(&sub_agent_event(
            LlmEventType::OpEnd,
            json!({"reason": "completed"}),
            1.5,
        ));
        assert_eq!(tracker.running_count(), 0);
        assert_eq!(tracker.runs()[0].state, AgentState::Completed);
    }

    #[test]
    fn terminal_event_without_source_still_finishes() {
        // A spawn path that never installs the sub-agent event proxy emits the
        // terminal `OpEnd` with a bare `spawn_id`. It must still retire the run.
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        let terminal = LlmEvent {
            event_type: LlmEventType::OpEnd,
            data: json!({"spawn_id": "spawn-1", "reason": "completed"}),
            timestamp: 2.0,
            metadata: None,
        };
        assert!(tracker.observe(&terminal));
        assert_eq!(tracker.runs()[0].state, AgentState::Completed);
        assert_eq!(tracker.running_count(), 0);
    }

    #[test]
    fn sourceless_terminal_event_does_not_create_a_run() {
        // Without `source` we cannot tell an agent event from a parent-session
        // event that happens to carry a `spawn_id`; only known runs may finish.
        let mut tracker = AgentTracker::new();
        let terminal = LlmEvent {
            event_type: LlmEventType::OpEnd,
            data: json!({"spawn_id": "unknown", "reason": "completed"}),
            timestamp: 2.0,
            metadata: None,
        };
        assert!(!tracker.observe(&terminal));
        assert!(tracker.runs().is_empty());
    }

    #[test]
    fn foreign_source_terminal_event_is_ignored() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        let mut event = sub_agent_event(LlmEventType::OpEnd, json!({}), 2.0);
        event.data["source"] = json!("observer");
        assert!(!tracker.observe(&event));
        assert_eq!(tracker.running_count(), 1);
    }

    #[test]
    fn finish_stale_retires_idle_runs_only() {
        let mut tracker = AgentTracker::new();
        // Activity stamped in the far past: stale relative to wall clock.
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        // A second run that just reported activity.
        let now = wall_clock_ms() as f64 / 1000.0;
        let mut live = sub_agent_event(LlmEventType::GenerationStart, json!({}), now);
        live.data["spawn_id"] = json!("spawn-live");
        tracker.observe(&live);

        assert_eq!(tracker.finish_stale(60_000), 1);
        let states: Vec<_> = tracker
            .runs()
            .into_iter()
            .map(|run| (run.spawn_id, run.state))
            .collect();
        assert_eq!(
            states,
            vec![
                ("spawn-1".to_string(), AgentState::Completed),
                ("spawn-live".to_string(), AgentState::Running),
            ]
        );
        assert_eq!(tracker.running_count(), 1);
        // Idempotent: nothing left to retire.
        assert_eq!(tracker.finish_stale(60_000), 0);
    }

    #[test]
    fn finish_stale_with_zero_idle_retires_nothing() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        assert_eq!(tracker.finish_stale(0), 0);
        assert_eq!(tracker.running_count(), 1);
    }

    #[test]
    fn prune_finished_drops_old_terminal_runs() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        tracker.observe(&sub_agent_event(LlmEventType::OpEnd, json!({}), 1.5));

        // The synthetic 1970 timestamp is ancient relative to the wall clock,
        // so it is past any retention window.
        assert_eq!(tracker.prune_finished(3_000), 1);
        assert!(tracker.runs().is_empty());
        assert_eq!(tracker.running_count(), 0);
    }

    #[test]
    fn prune_finished_keeps_running_runs() {
        let mut tracker = AgentTracker::new();
        tracker.observe(&sub_agent_event(
            LlmEventType::GenerationStart,
            json!({}),
            1.0,
        ));
        assert_eq!(tracker.prune_finished(0), 0);
        assert_eq!(tracker.runs().len(), 1);
    }
}
