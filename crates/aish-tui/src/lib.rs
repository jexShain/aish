//! `aish-tui` — the fullscreen terminal UI.
//!
//! This crate owns the alternate-screen front end: an absolute-coordinate cell
//! grid the product repaints as a whole, plus the layout that arranges a
//! supervisor snapshot into tabs. It deliberately does **not** contain the
//! inline REPL: the shell keeps its streaming, scrollback-driven prompt, and
//! the two front ends are switched by the caller (`Shift+Tab`).
//!
//! Entry points:
//!
//! * [`screen::Screen`] — the cell grid and its per-cell diff;
//! * [`workspace::draw`] — the layout, a pure function of snapshot and size;
//! * [`render::prepare_line`] / [`render::visible_width`] — the shared text
//!   normalization (ANSI-aware width, TAB expansion, CJK handling).
//!
//! Keeping the layout pure (data in, cells out) is what makes the fullscreen UI
//! testable without a terminal, and what lets the same `Workspace` be fed from
//! a live shell or a fixture.

pub mod app;
pub mod render;
pub mod screen;
pub mod workspace;

pub use app::{run, AppHooks, Key};
pub use render::{prepare_line, prepare_lines, strip_escapes, visible_width, TAB_WIDTH};
pub use screen::{Cell, CellStyle, Diff, Screen};
pub use workspace::{
    draw, AgentLine, AgentState, Drawn, LogLine, Palette, StageLine, Tab, TaskLine, Workspace,
    LEFT_COLUMN, MIN_HEIGHT, MIN_WIDTH,
};
