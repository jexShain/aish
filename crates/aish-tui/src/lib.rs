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
pub use panel::{EntryTone, PanelAction, PanelEntry, PanelTab, SupervisorPanel};
pub use render::{
    prepare_line, prepare_lines, strip_escapes, visible_width, PreparedLine, TAB_WIDTH,
};
pub use renderer::{Renderer, TerminalCaps};
pub use repl::{PromptSource, Repl, ReplAction, ReplConfig};
pub use shell_repl::run_shell_repl;

pub use output::{
    clear_global_sink, global_sink, install_global_sink, screen_owned, write_bytes, OutputSink,
    QueueSink, StdoutSink,
};

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    /// The installed sink is process state, so the tests touching it must not
    /// run concurrently: one's install would otherwise be visible to the
    /// other's "nothing is installed" assertion.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    /// Serialize plus reset, restoring the pristine state even on panic.
    struct GlobalGuard;

    impl Drop for GlobalGuard {
        fn drop(&mut self) {
            clear_global_sink();
        }
    }

    fn lock_global() -> (parking_lot::MutexGuard<'static, ()>, GlobalGuard) {
        let guard = TEST_LOCK.lock();
        clear_global_sink();
        (guard, GlobalGuard)
    }

    #[test]
    fn global_sink_defaults_to_stdout() {
        let (_guard, _global) = lock_global();
        // No sink installed: producers still work, straight to stdout.
        assert!(
            global_sink().as_any().is::<StdoutSink>(),
            "uninstalled global sink must be a StdoutSink"
        );
        assert!(
            !screen_owned(),
            "nothing installed, nothing owns the screen"
        );
    }

    #[test]
    fn install_global_sink_overrides() {
        let (_guard, _global) = lock_global();

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
        assert!(screen_owned(), "the queue sink owns the screen");
    }

    #[test]
    fn write_bytes_reaches_the_installed_queue() {
        let (_guard, _global) = lock_global();

        let sink = Arc::new(QueueSink::new());
        install_global_sink(Arc::clone(&sink) as Arc<dyn OutputSink>);

        // The PTY relay hands over child chunks verbatim; the queue splits them
        // into rows, so a chunk boundary can never invent a row.
        write_bytes(b"a\nb");
        assert_eq!(
            sink.drain(),
            ["a"],
            "the unterminated tail must stay buffered"
        );

        // The newline of the next chunk completes the buffered prefix.
        write_bytes(b"\n");
        assert_eq!(sink.drain(), ["b"]);
    }
}
