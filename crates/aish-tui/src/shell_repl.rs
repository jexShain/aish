//! Drive a product shell from the dual-channel REPL.
//!
//! [`crate::repl::Repl`] only knows how to read a line and hand it to a
//! callback; it deliberately has no idea what a shell is. This module is the
//! glue for a *product* shell (`aish-shell`): it installs a
//! [`QueueSink`] as the process-wide sink so every row the product prints is
//! queued instead of written, then runs the REPL with that sink attached. The
//! REPL commits the queued rows through its history channel immediately before
//! each paint, which is what keeps product output from tearing a frame.
//!
//! # Ownership
//!
//! The callback is a plain `FnMut`, not `'static`: the caller (typically
//! `aish-cli`) builds the shell on its own stack and drives it from the
//! closure. The REPL owns the terminal only for the duration of the call; the
//! global sink is restored to "none" on the way out so a later legacy launch in
//! the same process (tests, a fallback after a REPL failure) writes directly
//! again.

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;

use crate::completion::{CompletionSource, WordListSource};
use crate::output::{OutputSink, QueueSink};
use crate::repl::{
    demo_words, HudSource, PanelHandle, PromptSource, Repl, ReplAction, ReplConfig, TerminalGuard,
};

/// Run the interactive shell REPL, calling `on_submit` for every accepted line.
///
/// `on_submit` returns the [`ReplAction`] the REPL should take: `Exit` leaves
/// the loop, `Continue` waits for the next line, `AppendHistory` commits extra
/// rows to the history channel.
///
/// `prompt` supplies the prompt string once per frame, so the product's live
/// state (working directory, exit code, background tasks) is visible without
/// the REPL knowing anything about it.
///
/// `hud` and `panel` are the supervision integrations: `None` for either means
/// the REPL paints and reads exactly what it did before the feature existed.
///
/// # Errors
///
/// Returns an [`io::Error`] when the terminal cannot be put into raw mode or a
/// write fails. A closed pipe (the user piping the session into `head`) is
/// reported as success by the caller, not here.
pub fn run_shell_repl<F>(
    prompt: PromptSource,
    history_path: Option<PathBuf>,
    hud: Option<HudSource>,
    panel: Option<Arc<PanelHandle>>,
    on_submit: F,
) -> io::Result<()>
where
    F: FnMut(&str) -> ReplAction,
{
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "the dual-channel REPL needs a terminal on stdin and stdout",
        ));
    }

    let guard = TerminalGuard::enter()?;
    let sink = Arc::new(QueueSink::new());
    install_sink(Arc::clone(&sink));

    let config = ReplConfig {
        // The provider is the only prompt source; the constant field is kept
        // empty so the two can never disagree.
        prompt: String::new(),
        prompt_provider: Some(prompt),
        history_path,
        auto_suggest: true,
        sink: Some(Arc::clone(&sink) as Arc<dyn OutputSink>),
        hud,
        panel,
    };
    let sources: Vec<Box<dyn CompletionSource>> = vec![Box::new(WordListSource::new(demo_words()))];
    let mut repl = Repl::new(
        io::BufWriter::with_capacity(8192, io::stdout()),
        io::stdin(),
        config,
        sources,
    );

    let result = repl.run(on_submit);
    let restored = repl.restored();
    drop(repl);

    // Restore the legacy sink before the terminal guard runs: the guard writes
    // its reset sequence straight to stdout, and any row the shell flushes
    // afterwards must reach the terminal, not a queue nobody drains.
    restore_sink();
    guard.release(restored);

    match result {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Install `sink` as the process-wide sink for the duration of a REPL run.
fn install_sink(sink: Arc<QueueSink>) {
    crate::install_global_sink(sink as Arc<dyn OutputSink>);
}

/// Return the process-wide sink to the "nobody installed one" state.
fn restore_sink() {
    crate::clear_global_sink();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The global sink is process state, so the tests that touch it must not
    /// run concurrently with each other.
    static TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn install_then_restore_swaps_the_global_sink() {
        let _guard = TEST_LOCK.lock();
        restore_sink();
        assert!(crate::global_sink().as_any().is::<crate::StdoutSink>());

        let sink = Arc::new(QueueSink::new());
        install_sink(Arc::clone(&sink));
        assert!(crate::global_sink().as_any().is::<QueueSink>());

        restore_sink();
        assert!(
            crate::global_sink().as_any().is::<crate::StdoutSink>(),
            "restore must hand producers back the legacy sink"
        );
    }

    #[test]
    fn submitted_lines_reach_the_callback() {
        let _guard = TEST_LOCK.lock();
        restore_sink();

        let mut repl = Repl::with_caps(
            Vec::new(),
            io::Cursor::new(b"echo one\necho two\n".to_vec()),
            ReplConfig {
                prompt: "❯ ".to_string(),
                prompt_provider: None,
                history_path: None,
                auto_suggest: false,
                sink: None,
                hud: None,
                panel: None,
            },
            Vec::new(),
            crate::TerminalCaps {
                width: 80,
                height: 24,
                ..crate::TerminalCaps::default()
            },
        );

        let mut submitted = Vec::new();
        repl.run(|line| {
            submitted.push(line.to_string());
            ReplAction::Continue
        })
        .expect("REPL run");

        assert_eq!(submitted, vec!["echo one", "echo two"]);
    }

    #[test]
    fn exit_action_stops_the_loop() {
        let _guard = TEST_LOCK.lock();
        restore_sink();

        let mut repl = Repl::with_caps(
            Vec::new(),
            io::Cursor::new(b"first\nsecond\n".to_vec()),
            ReplConfig {
                prompt: "❯ ".to_string(),
                prompt_provider: None,
                history_path: None,
                auto_suggest: false,
                sink: None,
                hud: None,
                panel: None,
            },
            Vec::new(),
            crate::TerminalCaps {
                width: 80,
                height: 24,
                ..crate::TerminalCaps::default()
            },
        );

        let mut submitted = Vec::new();
        repl.run(|line| {
            submitted.push(line.to_string());
            ReplAction::Exit
        })
        .expect("REPL run");

        assert_eq!(submitted, vec!["first"]);
    }
}
