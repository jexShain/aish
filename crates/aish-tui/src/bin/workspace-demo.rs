//! Interactive prototype of the fullscreen workspace UI.
//!
//! Enters the alternate screen and lets every interaction be exercised without
//! a shell behind it:
//!
//! * `1`–`4`, `Tab` / `Shift+Tab` — tabs
//! * `↑`/`↓`, `j`/`k` — selection
//! * `a` — answer (only when the row waits on you)
//! * `s` — say something to the row
//! * `r` — retry (only when the row is retryable)
//! * `x` — stop
//! * `q` / `Ctrl+C` — leave
//!
//! The layout, the cell grid and the key loop are the production types; only
//! the fixture data and the hook reactions below are for the demo.
//!
//! Run with `cargo run -p aish-tui --bin workspace-demo`.

use std::io;

use aish_tui::workspace::{
    AgentLine, AgentState, Composer, LogLine, StageLine, Tab, TaskLine, Workspace,
};
use aish_tui::{run, AppHooks, Key};

struct Fixture {
    workspace: Workspace,
    /// The last action the hook took, shown in the log so the demo proves the
    /// key reached it.
    notices: Vec<String>,
}

impl AppHooks for Fixture {
    fn snapshot(&self) -> Workspace {
        let mut workspace = self.workspace.clone();
        workspace.log = self
            .notices
            .iter()
            .rev()
            .take(4)
            .enumerate()
            .map(|(index, text)| LogLine {
                at: format!("{}", index + 1),
                text: text.clone(),
            })
            .collect();
        workspace
    }

    fn on_idle(&mut self) -> bool {
        false
    }

    fn on_key(&mut self, key: Key, workspace: &mut Workspace) {
        let Some(agent) = workspace.selected_agent().cloned() else {
            return;
        };
        match key {
            Key::Retry => {
                self.notices.push(format!("retry {}", agent.id));
                self.workspace.agents[workspace.selected].state = AgentState::Running;
            }
            Key::Stop => {
                self.notices.push(format!("stop {}", agent.id));
                self.workspace.agents[workspace.selected].state = AgentState::Failed;
            }
            _ => {}
        }
    }

    fn on_composer(&mut self, ticket: &str, purpose: Composer, text: &str) {
        let verb = match purpose {
            Composer::Answer => "answer",
            Composer::Say => "say",
            Composer::Closed => return,
        };
        self.notices.push(format!("{verb} {ticket}: {text}"));
        if purpose == Composer::Answer {
            if let Some(row) = self.workspace.agents.iter_mut().find(|a| a.id == ticket) {
                row.state = AgentState::Running;
                row.activity = "answered".to_string();
            }
        }
    }
}

fn main() -> io::Result<()> {
    run(Fixture {
        workspace: sample(),
        notices: Vec::new(),
    })
}

/// A fixed workspace, so the layout can be reviewed without a live shell.
fn sample() -> Workspace {
    Workspace {
        title: "auth-refresh".to_string(),
        goal: "Rotate tokens on every use".to_string(),
        agents: vec![
            AgentLine {
                id: "tk-1".to_string(),
                retryable: false,
                name: "orchestrator".to_string(),
                state: AgentState::Running,
                activity: "planning the split".to_string(),
                elapsed: "0:42".to_string(),
            },
            AgentLine {
                id: "tk-2".to_string(),
                retryable: true,
                name: "build".to_string(),
                state: AgentState::Waits,
                activity: "Trust this folder?".to_string(),
                elapsed: "1:03".to_string(),
            },
            AgentLine {
                id: "tk-3".to_string(),
                retryable: false,
                name: "review".to_string(),
                state: AgentState::Planned,
                activity: String::new(),
                elapsed: "—".to_string(),
            },
            AgentLine {
                id: "tk-4".to_string(),
                retryable: false,
                name: "cleanup".to_string(),
                state: AgentState::Done,
                activity: "removed the branch".to_string(),
                elapsed: "0:08".to_string(),
            },
        ],
        stages: vec![
            StageLine {
                name: "Plan".to_string(),
                done: true,
                current: false,
                progress: String::new(),
            },
            StageLine {
                name: "Build".to_string(),
                done: false,
                current: true,
                progress: "1/3".to_string(),
            },
            StageLine {
                name: "Review".to_string(),
                done: false,
                current: false,
                progress: String::new(),
            },
        ],
        tasks: vec![
            TaskLine {
                id: "t1a2060".to_string(),
                summary: "cargo test --workspace".to_string(),
                elapsed: "0:12".to_string(),
                finished: false,
            },
            TaskLine {
                id: "t1a2055".to_string(),
                summary: "cargo clippy --all-targets".to_string(),
                elapsed: "1:41".to_string(),
                finished: true,
            },
        ],
        log: vec![LogLine {
            at: "1".to_string(),
            text: "press a / s / r / x on a row".to_string(),
        }],
        clock: "12:07".to_string(),
        context_percent: 4,
        selected: 1,
        tab: Tab::Mission,
        draft: None,
        draft_cursor: 0,
        composer: Composer::Closed,
    }
}
