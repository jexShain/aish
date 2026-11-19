//! `aish-tui` — the dual-channel rendering kernel.
//!
//! The shell's UI is split into two channels that are never mixed:
//!
//! * **history**: finished rows that leave the live area and become part of the
//!   terminal's own scrollback. Written once, acked by id, never rewritten.
//! * **viewport**: the mutable region (prompt, spinner, status). The product
//!   re-supplies the whole thing every frame and the renderer diffs it.
//!
//! Entry points:
//!
//! * [`frame::Frame`] / [`frame::HistoryBatch`] — what a product submits;
//! * [`renderer::Renderer`] — turns frames into escape sequences;
//! * [`render::prepare_line`] — normalization into terminal-safe rows;
//! * [`component::Component`] / [`component::Container`] — reference-memoized
//!   component tree feeding the viewport.
//!
//! The contract invariants are documented on [`frame`] and [`renderer`]; they
//! are the reason this crate exists rather than ad-hoc `println!`s in the shell.

pub mod caps;
pub mod component;
pub mod editor;
pub mod frame;
pub mod highlight;
pub mod input;
pub mod render;
pub mod renderer;

pub use caps::detect_caps;
pub use component::{render_lines_to_viewport, Component, Container, InputHandled, Interactive};
pub use frame::{Frame, HistoryBatch};
pub use render::{
    prepare_line, prepare_lines, strip_escapes, visible_width, PreparedLine, TAB_WIDTH,
};
pub use renderer::{Renderer, TerminalCaps};
