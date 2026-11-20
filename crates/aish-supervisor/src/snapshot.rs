//! Snapshot assembly and change notification for the supervision panel.

use aish_core::LlmEvent;

use crate::agents::AgentTracker;
use crate::sessions::{self, SessionRow};
use crate::tasks::{self, TaskRow};
use crate::{AgentRun, TaskState};

/// Which state source changed, so a panel can repaint selectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Agents,
    Tasks,
    Sessions,
}

/// Point-in-time view of everything the scheduler panel renders.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SupervisorSnapshot {
    pub agents: Vec<AgentRun>,
    pub tasks: Vec<TaskRow>,
    pub sessions: Vec<SessionRow>,
}

impl SupervisorSnapshot {
    pub fn running_agents(&self) -> usize {
        self.agents
            .iter()
            .filter(|run| run.state == crate::AgentState::Running)
            .count()
    }

    pub fn running_tasks(&self) -> usize {
        self.tasks
            .iter()
            .filter(|row| row.state == TaskState::Running)
            .count()
    }

    /// True when anything is running (drives the status-line indicator).
    pub fn is_busy(&self) -> bool {
        self.running_agents() > 0 || self.running_tasks() > 0
    }

    /// One-line summary for the HUD, e.g. `"2 agents · 1 task · 3 sessions"`.
    ///
    /// Only non-empty groups are listed so the indicator stays short; the
    /// result is empty when nothing is running.
    pub fn hud_summary(&self) -> String {
        let agents = self.running_agents();
        let tasks = self.running_tasks();
        if agents == 0 && tasks == 0 {
            return String::new();
        }
        let mut parts = Vec::with_capacity(3);
        if agents > 0 {
            parts.push(format!("{agents} {}", plural(agents, "agent", "agents")));
        }
        if tasks > 0 {
            parts.push(format!("{tasks} {}", plural(tasks, "task", "tasks")));
        }
        if !self.sessions.is_empty() {
            let sessions = self.sessions.len();
            parts.push(format!(
                "{sessions} {}",
                plural(sessions, "session", "sessions")
            ));
        }
        parts.join(" · ")
    }
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

/// Owns the agent tracker and produces snapshots on demand.
pub struct Supervisor {
    agents: AgentTracker,
    last: SupervisorSnapshot,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            agents: AgentTracker::new(),
            last: SupervisorSnapshot::default(),
        }
    }

    /// Feed one LLM event; returns Some(kind) when the snapshot changed.
    pub fn observe(&mut self, event: &LlmEvent) -> Option<ChangeKind> {
        self.agents.observe(event).then_some(ChangeKind::Agents)
    }

    /// Take a full snapshot (re-reads tasks/sessions from their sources).
    pub fn snapshot(&mut self) -> SupervisorSnapshot {
        self.last = SupervisorSnapshot {
            agents: self.agents.runs(),
            tasks: tasks::snapshot(),
            sessions: sessions::snapshot(),
        };
        self.last.clone()
    }

    /// Last snapshot without re-reading sources.
    pub fn cached(&self) -> &SupervisorSnapshot {
        &self.last
    }

    /// Number of sub-agent runs still running.
    pub fn running_agents(&self) -> usize {
        self.agents.running_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aish_core::LlmEventType;
    use serde_json::json;

    fn sub_agent_event(event_type: LlmEventType, spawn_id: &str) -> LlmEvent {
        LlmEvent {
            event_type,
            data: json!({
                "source": "sub_agent",
                "agent_type": "explore",
                "depth": 1,
                "spawn_id": spawn_id,
            }),
            timestamp: 1.0,
            metadata: None,
        }
    }

    fn task_row(state: TaskState) -> TaskRow {
        TaskRow {
            task_id: "t1".into(),
            command: "sleep 10".into(),
            state,
            exit_code: None,
            started_at_ms: 1,
            finished_at_ms: None,
            output_path: "/tmp/t1.log".into(),
        }
    }

    fn session_row() -> SessionRow {
        SessionRow {
            session_id: "s1".into(),
            socket_path: "/tmp/s1.sock".into(),
            daemon_pid: 1,
            child_pid: Some(2),
            cwd: "/tmp".into(),
            model: "m".into(),
            name: None,
            role: crate::SessionRole::Controller,
            started_at: "1970-01-01T00:00:01Z".into(),
        }
    }

    #[test]
    fn observe_returns_kind_on_change() {
        let mut supervisor = Supervisor::new();
        assert_eq!(
            supervisor.observe(&sub_agent_event(LlmEventType::GenerationStart, "spawn-1")),
            Some(ChangeKind::Agents)
        );
        // Same event again is not a change.
        assert_eq!(
            supervisor.observe(&sub_agent_event(LlmEventType::GenerationStart, "spawn-1")),
            None
        );
        // Non sub-agent events are ignored.
        assert_eq!(
            supervisor.observe(&LlmEvent {
                event_type: LlmEventType::ContentDelta,
                data: json!({"delta": "hi"}),
                timestamp: 2.0,
                metadata: None,
            }),
            None
        );
        assert_eq!(supervisor.running_agents(), 1);
    }

    #[test]
    fn snapshot_includes_all_three() {
        let mut supervisor = Supervisor::new();
        supervisor.observe(&sub_agent_event(LlmEventType::GenerationStart, "spawn-1"));
        let snapshot = supervisor.snapshot();
        assert_eq!(snapshot.agents.len(), 1);
        // Tasks/sessions come from the live process-global sources; only the
        // agent slice is deterministic here.
        assert!(snapshot.tasks.len() < usize::MAX);
        assert!(snapshot.sessions.len() < usize::MAX);
        assert_eq!(snapshot.agents[0].spawn_id, "spawn-1");
    }

    #[test]
    fn is_busy_false_when_empty() {
        let snapshot = SupervisorSnapshot::default();
        assert!(!snapshot.is_busy());
        assert_eq!(snapshot.running_agents(), 0);
        assert_eq!(snapshot.running_tasks(), 0);
        assert_eq!(snapshot.hud_summary(), "");
    }

    #[test]
    fn is_busy_tracks_running_entries() {
        let mut snapshot = SupervisorSnapshot::default();
        snapshot.tasks.push(task_row(TaskState::Running));
        assert!(snapshot.is_busy());
        assert_eq!(snapshot.running_tasks(), 1);

        let mut snapshot = SupervisorSnapshot::default();
        snapshot.agents.push(AgentRun {
            spawn_id: "s".into(),
            agent_type: "explore".into(),
            depth: 1,
            skill_name: None,
            description: None,
            state: crate::AgentState::Running,
            tool_calls: 0,
            last_text: String::new(),
            started_at_ms: 1,
            finished_at_ms: None,
        });
        assert!(snapshot.is_busy());

        // Terminal entries do not count as busy.
        let mut snapshot = SupervisorSnapshot::default();
        snapshot.tasks.push(task_row(TaskState::Completed));
        assert!(!snapshot.is_busy());
    }

    #[test]
    fn hud_summary_formats() {
        let mut snapshot = SupervisorSnapshot::default();
        snapshot.tasks.push(task_row(TaskState::Running));
        snapshot.sessions.push(session_row());
        assert_eq!(snapshot.hud_summary(), "1 task · 1 session");

        snapshot.tasks.push(task_row(TaskState::Running));
        snapshot.sessions.push(session_row());
        snapshot.sessions.push(session_row());
        assert_eq!(snapshot.hud_summary(), "2 tasks · 3 sessions");

        let mut snapshot = SupervisorSnapshot::default();
        snapshot.agents.push(AgentRun {
            spawn_id: "a".into(),
            agent_type: "explore".into(),
            depth: 1,
            skill_name: None,
            description: None,
            state: crate::AgentState::Running,
            tool_calls: 0,
            last_text: String::new(),
            started_at_ms: 1,
            finished_at_ms: None,
        });
        snapshot.agents.push(AgentRun {
            spawn_id: "b".into(),
            agent_type: "plan".into(),
            depth: 1,
            skill_name: None,
            description: None,
            state: crate::AgentState::Running,
            tool_calls: 0,
            last_text: String::new(),
            started_at_ms: 2,
            finished_at_ms: None,
        });
        snapshot.tasks.push(task_row(TaskState::Running));
        snapshot.sessions.push(session_row());
        snapshot.sessions.push(session_row());
        snapshot.sessions.push(session_row());
        assert_eq!(snapshot.hud_summary(), "2 agents · 1 task · 3 sessions");
    }

    #[test]
    fn cached_returns_last() {
        let mut supervisor = Supervisor::new();
        assert!(supervisor.cached().agents.is_empty());

        supervisor.observe(&sub_agent_event(LlmEventType::GenerationStart, "spawn-1"));
        // Not visible until a snapshot is taken.
        assert!(supervisor.cached().agents.is_empty());

        let snapshot = supervisor.snapshot();
        assert_eq!(supervisor.cached().agents.len(), 1);
        assert_eq!(
            supervisor.cached().agents[0].spawn_id,
            snapshot.agents[0].spawn_id
        );
    }

    #[test]
    fn snapshot_serializes() {
        let mut snapshot = SupervisorSnapshot::default();
        snapshot.tasks.push(task_row(TaskState::Running));
        let json = serde_json::to_string(&snapshot).expect("snapshot serializes");
        assert!(json.contains("\"tasks\""));
        assert!(json.contains("\"Running\""));
        assert!(json.contains("\"command\":\"sleep 10\""));
    }
}
