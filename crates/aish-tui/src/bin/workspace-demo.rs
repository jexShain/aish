//! Prototype of the fullscreen workspace UI.
//!
//! Enters the alternate screen, paints the layout from a fixed sample, and
//! lets the navigation keys be exercised: `1`-`4` switch tab, `Tab`/`Shift+Tab`
//! cycle, `↑`/`↓` (or `j`/`k`) move the selection, `q` leaves.
//!
//! The point of this binary is to review the layout in a real terminal. The
//! layout ([`aish_tui::workspace`]), the cell grid ([`aish_tui::screen`]) and
//! the key loop ([`aish_tui::app`]) are the production types; only the sample
//! data below is fixture.
//!
//! Run with `cargo run -p aish-tui --bin workspace-demo`.

use std::io;

use aish_tui::workspace::{AgentLine, AgentState, LogLine, StageLine, Tab, TaskLine, Workspace};
use aish_tui::{run, AppHooks};

struct Fixture {
    workspace: Workspace,
}

impl AppHooks for Fixture {
    fn snapshot(&self) -> Workspace {
        self.workspace.clone()
    }

    fn should_close(&self) -> bool {
        false
    }
}

fn main() -> io::Result<()> {
    run(Fixture {
        workspace: sample(),
    })
}

/// A fixed workspace, so the layout can be reviewed without a live shell.
fn sample() -> Workspace {
    Workspace {
        title: "auth-refresh".to_string(),
        goal: "Rotate tokens on every use".to_string(),
        agents: vec![
            AgentLine {
                name: "orchestrator".to_string(),
                state: AgentState::Running,
                activity: "planning the split".to_string(),
                elapsed: "0:42".to_string(),
            },
            AgentLine {
                name: "build".to_string(),
                state: AgentState::Waits,
                activity: "Trust this folder?".to_string(),
                elapsed: "1:03".to_string(),
            },
            AgentLine {
                name: "review".to_string(),
                state: AgentState::Planned,
                activity: String::new(),
                elapsed: "—".to_string(),
            },
            AgentLine {
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
        log: vec![
            LogLine {
                at: "12:07".to_string(),
                text: "dispatched build".to_string(),
            },
            LogLine {
                at: "12:06".to_string(),
                text: "plan approved".to_string(),
            },
            LogLine {
                at: "12:04".to_string(),
                text: "session resumed".to_string(),
            },
        ],
        clock: "12:07".to_string(),
        context_percent: 4,
        selected: 1,
        tab: Tab::Mission,
    }
}
