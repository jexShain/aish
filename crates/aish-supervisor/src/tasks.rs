//! Snapshot rows over the process-global background task registry.
//!
//! The registry in [`aish_tools::task`] is owned by the shell REPL, which
//! drains completion notices into the AI context. This module is strictly
//! read-only apart from [`stop`]: taking a snapshot MUST NOT consume those
//! notices, otherwise the model silently loses track of finished tasks.

use aish_tools::task::{self, TaskInfo, TaskStatus};

/// Lifecycle state of a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TaskState {
    Running,
    Completed,
    Failed,
    Stopped,
}

impl From<TaskStatus> for TaskState {
    fn from(status: TaskStatus) -> Self {
        match status {
            TaskStatus::Running => TaskState::Running,
            TaskStatus::Completed => TaskState::Completed,
            TaskStatus::Failed => TaskState::Failed,
            TaskStatus::Stopped => TaskState::Stopped,
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            TaskState::Running => "running",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Stopped => "stopped",
        };
        f.write_str(name)
    }
}

/// Snapshot row over aish_tools::task::TaskInfo.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskRow {
    pub task_id: String,
    pub command: String,
    pub state: TaskState,
    pub exit_code: Option<i32>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub output_path: String,
}

impl TaskRow {
    pub fn from_info(info: &TaskInfo) -> Self {
        Self {
            task_id: info.task_id.clone(),
            command: info.command.clone(),
            state: TaskState::from(info.status),
            exit_code: info.exit_code,
            started_at_ms: info.started_at_ms,
            finished_at_ms: info.finished_at_ms,
            output_path: info.output_path.clone(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.state == TaskState::Running
    }
}

/// Read the global registry (no side effects — never drains the notice queue).
///
/// Rows come back running-first, then newest finished, matching
/// [`aish_tools::task::list_tasks`].
pub fn snapshot() -> Vec<TaskRow> {
    task::list_tasks().iter().map(TaskRow::from_info).collect()
}

/// Number of tasks currently running.
pub fn running_count() -> usize {
    task::list_tasks()
        .iter()
        .filter(|info| info.status == TaskStatus::Running)
        .count()
}

/// Stop a task by id (delegates to [`aish_tools::task::task_stop`]).
pub fn stop(task_id: &str) -> Result<i32, String> {
    task::task_stop(task_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_until_finished(task_id: &str) -> TaskRow {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let row = snapshot()
                .into_iter()
                .find(|row| row.task_id == task_id)
                .expect("task present in snapshot");
            if !row.is_running() {
                return row;
            }
            assert!(Instant::now() < deadline, "task did not finish in time");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn snapshot_reflects_spawn() {
        let task_id = task::spawn_background("echo supervisor-spawn").unwrap();
        let row = wait_until_finished(&task_id);
        assert_eq!(row.state, TaskState::Completed);
        assert_eq!(row.exit_code, Some(0));
        assert_eq!(row.command, "echo supervisor-spawn");
        assert!(row.finished_at_ms.is_some());
        assert!(row.output_path.contains(&task_id));
    }

    #[test]
    fn snapshot_does_not_drain_notices() {
        let task_id = task::spawn_background("echo supervisor-notice").unwrap();
        wait_until_finished(&task_id);

        // Repeated snapshots must not consume the completion notice.
        assert!(snapshot().iter().any(|row| row.task_id == task_id));
        assert!(snapshot().iter().any(|row| row.task_id == task_id));

        let drained = task::drain_finished_tasks();
        assert!(
            drained.iter().any(|info| info.task_id == task_id),
            "snapshot() consumed the completion notice"
        );
    }

    #[test]
    fn state_mapping() {
        let cases = [
            (TaskStatus::Running, TaskState::Running),
            (TaskStatus::Completed, TaskState::Completed),
            (TaskStatus::Failed, TaskState::Failed),
            (TaskStatus::Stopped, TaskState::Stopped),
        ];
        for (status, expected) in cases {
            assert_eq!(TaskState::from(status), expected);
        }

        let info = TaskInfo {
            task_id: "t1".into(),
            command: "true".into(),
            status: TaskStatus::Failed,
            exit_code: Some(3),
            output_path: "/tmp/t1.log".into(),
            cursor: 0,
            started_at_ms: 10,
            finished_at_ms: Some(20),
        };
        let row = TaskRow::from_info(&info);
        assert_eq!(
            row,
            TaskRow {
                task_id: "t1".into(),
                command: "true".into(),
                state: TaskState::Failed,
                exit_code: Some(3),
                started_at_ms: 10,
                finished_at_ms: Some(20),
                output_path: "/tmp/t1.log".into(),
            }
        );
        assert!(!row.is_running());
    }

    #[test]
    fn running_count_tracks_live_tasks() {
        let task_id = task::spawn_background("sleep 5").unwrap();
        let row = snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .expect("spawned task visible in snapshot");
        assert!(row.is_running());
        assert!(running_count() > 0, "running task not counted");

        stop(&task_id).expect("stop running task");
        assert!(!snapshot()
            .into_iter()
            .find(|row| row.task_id == task_id)
            .is_some_and(|row| row.is_running()));
    }

    #[test]
    fn stop_unknown_task_errors() {
        assert!(stop("supervisor-unknown-task").is_err());
    }
}
