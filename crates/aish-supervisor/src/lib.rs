//! Unified supervision snapshot: agents, background tasks, live sessions.
//!
//! This crate is pure state aggregation — no rendering, no terminal I/O. A
//! future TUI scheduler panel subscribes to [`SupervisorSnapshot`] values and
//! renders them however it likes.

pub mod agents;
pub mod sessions;
pub mod snapshot;
pub mod tasks;

pub use agents::{AgentRun, AgentState};
pub use sessions::{SessionRole, SessionRow};
pub use snapshot::{ChangeKind, Supervisor, SupervisorSnapshot};
pub use tasks::{TaskRow, TaskState};
