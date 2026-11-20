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
//!   component tree feeding the viewport;
//! * [`output::OutputSink`] — the only way product text reaches the terminal
//!   while a frame owns the screen.
//!
//! The contract invariants are documented on [`frame`] and [`renderer`]; they
//! are the reason this crate exists rather than ad-hoc `println!`s in the shell.

pub mod caps;
pub mod completion;
pub mod component;
pub mod editor;
pub mod frame;
pub mod highlight;
pub mod hud;
pub mod input;
pub mod output;
pub mod panel;
pub mod render;
pub mod renderer;
pub mod repl;
pub mod shell_repl;

pub use caps::detect_caps;
pub use completion::{common_prefix, token_range, Candidate, CompletionSource, WordListSource};
pub use component::{render_lines_to_viewport, Component, Container, InputHandled, Interactive};
pub use frame::{Frame, HistoryBatch};
pub use hud::HudState;
pub use output::{OutputSink, QueueSink, StdoutSink};
pub use panel::{EntryTone, PanelAction, PanelEntry, PanelTab, SupervisorPanel};
pub use render::{
    prepare_line, prepare_lines, strip_escapes, visible_width, PreparedLine, TAB_WIDTH,
};
pub use renderer::{Renderer, TerminalCaps};
pub use repl::{PromptSource, Repl, ReplAction, ReplConfig};
pub use shell_repl::run_shell_repl;

use std::sync::Arc;

use parking_lot::RwLock;

/// Process-wide sink used by producers that cannot be handed one directly
/// (loggers, LLM streaming callbacks, background tasks).
///
/// `None` means "nobody installed one": [`global_sink`] then hands out a fresh
/// [`StdoutSink`], so such a producer degrades to legacy `println!`-like
/// behavior instead of panicking. `RwLock` rather than `OnceLock` because the
/// shell installs the sink *after* its first logger might have been built, and
/// tests must be able to replace it.
static GLOBAL_SINK: RwLock<Option<Arc<dyn OutputSink>>> = RwLock::new(None);

/// Install the process-wide sink.
///
/// Call this once the interactive renderer exists, before any background
/// producer starts writing: every later [`global_sink`] call returns it.
pub fn install_global_sink(sink: Arc<dyn OutputSink>) {
    *GLOBAL_SINK.write() = Some(sink);
}

/// Remove the installed sink, restoring the [`StdoutSink`] fallback.
///
/// The interactive REPL owns the terminal only while it runs; a launch that
/// hands the process back (fallback after a failure, a test driving both paths)
/// must not leave producers queueing rows nobody will drain.
pub fn clear_global_sink() {
    *GLOBAL_SINK.write() = None;
}

/// The installed process-wide sink, or a [`StdoutSink`] when none is.
///
/// The fallback is allocated per call on purpose: it is stateless, and keeping
/// one alive in the static would make the "was a sink installed?" question
/// unanswerable.
pub fn global_sink() -> Arc<dyn OutputSink> {
    GLOBAL_SINK
        .read()
        .as_ref()
        .map(Arc::clone)
        .unwrap_or_else(|| Arc::new(StdoutSink))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installed sink is process state, so the tests touching it must not
    /// run concurrently: one's install would otherwise be visible to the
    /// other's "nothing is installed" assertion.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Return the process to the pristine "nobody installed a sink" state.
    fn reset() {
        *GLOBAL_SINK.write() = None;
    }

    #[test]
    fn global_sink_defaults_to_stdout() {
        let _guard = TEST_LOCK.lock();
        reset();
        // No sink installed: producers still work, straight to stdout.
        assert!(
            global_sink().as_any().is::<StdoutSink>(),
            "uninstalled global sink must be a StdoutSink"
        );
    }

    #[test]
    fn install_global_sink_overrides() {
        let _guard = TEST_LOCK.lock();
        reset();

        let sink: Arc<dyn OutputSink> = Arc::new(QueueSink::new());
        install_global_sink(Arc::clone(&sink));

        let installed = global_sink();
        assert!(
            installed.as_any().is::<QueueSink>(),
            "installed sink was not returned"
        );
        // Every caller observes the same instance, so they share one queue.
        assert!(
            Arc::ptr_eq(&installed, &sink),
            "global_sink cloned the sink"
        );

        reset();
    }
}
