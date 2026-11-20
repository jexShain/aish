//! Process-global product-output sink.
//!
//! # Why a sink at all
//!
//! A frame is one *atomic* write. The renderer brackets it with a hidden
//! cursor, synchronized output and autowrap disabled, and it borrows the
//! terminal's row grid for the whole viewport: it remembers which row the live
//! band started on and rewrites only the rows whose text changed.
//!
//! A `println!` from anywhere else — a background task, an LLM streaming
//! callback, a child process relay — lands *inside* those bytes. The terminal
//! then moves the cursor and scrolls by an amount the renderer never saw, so
//! its memo is off by that many rows: the next frame repaints history,
//! duplicates rows, or erases the prompt. There is no way to recover from that
//! inside the renderer; the damaged bytes are already on the wire.
//!
//! So product output does not go to the terminal directly. It goes to an
//! [`OutputSink`], which is either:
//!
//! * [`StdoutSink`] — the legacy path, used when no renderer owns the screen
//!   (tests, one-shot CLI output, a REPL that is not in raw mode); or
//! * a queue sink owned by the TUI crate, which buffers rows and hands them to
//!   the REPL. The REPL commits them through its normal history channel
//!   immediately *before* it paints the next frame, so product output can
//!   never land mid-frame no matter which thread produced it.
//!
//! # Why the trait lives in `aish-core`
//!
//! Producers exist at every layer. The PTY relay (`aish-pty`) is the lowest
//! one and must not learn about the TUI, so the trait, the global slot and the
//! plain-stdout default live here; `aish-tui` re-exports them and adds its
//! queue implementation. That keeps a single process-wide sink instead of one
//! per crate, which is what makes the "does a renderer own the screen?"
//! question answerable from the lowest layer.
//!
//! A sink is `Send + Sync` because producers (loggers, streaming deltas, PTY
//! relay threads) run on arbitrary threads while the REPL owns the main one.
//! Ordering is therefore only guaranteed per sink call, and a queue sink keeps
//! even a fragment stream (`raw`) coherent by buffering the unterminated tail.

use std::any::Any;
use std::io::{self, Write};
use std::sync::{Arc, LazyLock};

use parking_lot::RwLock;

/// Sink for product output that must never interleave with a frame.
///
/// Implementors must be cheap to call from any thread: the REPL, loggers and
/// streaming callbacks all write through the same object while a paint may be
/// in progress on another thread.
pub trait OutputSink: Send + Sync {
    /// Append one finished line (newline added by the sink).
    ///
    /// This is the normal path for command output and log records: `text` is
    /// one row, and it is terminated for you.
    fn line(&self, text: &str);

    /// Append a raw fragment (streaming deltas); the sink buffers it and
    /// commits complete lines. Trailing partial line is flushed by `flush_raw`.
    ///
    /// Use this when the producer cannot know where a row ends (an LLM token
    /// stream, a child process's byte chunks): every `\n` in `text` completes
    /// a row, and whatever follows the last `\n` stays buffered.
    fn raw(&self, text: &str);

    /// Commit any buffered partial raw line.
    ///
    /// Call when the producer knows a row ended without a newline (end of
    /// stream, end of a tool result). A no-op when nothing is buffered.
    fn flush_raw(&self);

    /// Ask for a repaint (no-op for sinks that write immediately).
    ///
    /// Implementations that buffer (a future scrollback-aware sink) may use
    /// this to schedule one; the REPL already repaints every frame.
    fn request_repaint(&self);

    /// Downcast hook, so the REPL can recognize the concrete sink it drains.
    ///
    /// The REPL needs the queue sink's `drain` method, which cannot live on
    /// the trait: a sink that writes immediately has nothing to drain. An
    /// inherent method plus this hook keeps the trait minimal for implementors.
    fn as_any(&self) -> &dyn Any;

    /// True when a renderer owns the screen: producers must NOT write fd 1
    /// directly and must route through `line`/`raw` instead.
    ///
    /// This is the switch relay code consults before touching stdout. Getting
    /// it wrong is not a cosmetic problem — a byte written to fd 1 while a
    /// frame is on the wire corrupts the renderer's row memo.
    fn owns_screen(&self) -> bool {
        false
    }
}

/// Default sink: writes straight to stdout (legacy behavior, used by tests
/// and by the legacy shell path).
///
/// Nothing is buffered and nothing is coordinated with a renderer, so this is
/// only correct when no frame owns the screen. It exists so callers that are
/// agnostic about the UI mode have something to write to.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdoutSink;

impl OutputSink for StdoutSink {
    fn line(&self, text: &str) {
        let mut out = io::stdout().lock();
        // A closed pipe is how `aish ... | head` ends: nothing to report and
        // nothing to retry, but a real error is worth a trace.
        if let Err(err) = out
            .write_all(text.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
        {
            tracing::debug!(%err, "stdout sink write failed");
        }
        let _ = out.flush();
    }

    fn raw(&self, text: &str) {
        let mut out = io::stdout().lock();
        if let Err(err) = out.write_all(text.as_bytes()) {
            tracing::debug!(%err, "stdout sink write failed");
        }
        let _ = out.flush();
    }

    fn flush_raw(&self) {
        let _ = io::stdout().flush();
    }

    fn request_repaint(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Process-wide sink used by producers that cannot be handed one directly
/// (loggers, LLM streaming callbacks, the PTY relay, background tasks).
///
/// `None` means "nobody installed one": [`global_sink`] then hands out the
/// shared [`StdoutSink`], so such a producer degrades to legacy
/// `println!`-like behavior instead of panicking. `RwLock` rather than
/// `OnceLock` because the shell installs the sink *after* its first logger
/// might have been built, and tests must be able to replace it.
static GLOBAL_SINK: RwLock<Option<Arc<dyn OutputSink>>> = RwLock::new(None);

/// The uninstalled fallback, allocated once so the hot path is a refcount bump
/// instead of an allocation per write.
static FALLBACK_SINK: LazyLock<Arc<dyn OutputSink>> = LazyLock::new(|| Arc::new(StdoutSink));

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
pub fn global_sink() -> Arc<dyn OutputSink> {
    if let Some(sink) = GLOBAL_SINK.read().as_ref() {
        return Arc::clone(sink);
    }
    Arc::clone(&FALLBACK_SINK)
}

/// Whether a renderer currently owns the screen.
///
/// Relay code calls this on every chunk, so it must not allocate: it inspects
/// the installed sink in place instead of going through [`global_sink`].
pub fn screen_owned() -> bool {
    GLOBAL_SINK
        .read()
        .as_ref()
        .is_some_and(|sink| sink.owns_screen())
}

/// Convenience: route a byte buffer as text through the sink.
///
/// Bytes are relayed verbatim (`raw` semantics), because the producer is a
/// child process whose chunks split at arbitrary offsets: a chunk boundary is
/// not a row boundary, and inventing one would split a single output line into
/// two history rows. Invalid UTF-8 is replaced lossily — the sink API is
/// text-shaped, and a relay that already normalizes its stream never reaches
/// this path with arbitrary binary.
pub fn write_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    global_sink().raw(&String::from_utf8_lossy(bytes));
}

#[cfg(test)]
mod tests {
    use super::*;

    use parking_lot::Mutex;

    /// The installed sink is process state, so the tests touching it must not
    /// run concurrently: one's install would otherwise be visible to the
    /// other's "nothing is installed" assertion.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Test double for [`StdoutSink`]: the same byte contract, without writing
    /// to fd 1 (which libtest does not capture).
    #[derive(Debug, Default)]
    struct RecordingSink {
        written: Mutex<Vec<u8>>,
        flushes: Mutex<usize>,
        owns_screen: bool,
    }

    impl RecordingSink {
        fn with_screen(owns_screen: bool) -> Self {
            Self {
                owns_screen,
                ..Self::default()
            }
        }

        fn bytes(&self) -> String {
            String::from_utf8(self.written.lock().clone()).expect("utf-8")
        }

        fn flushes(&self) -> usize {
            *self.flushes.lock()
        }
    }

    impl OutputSink for RecordingSink {
        fn line(&self, text: &str) {
            let mut written = self.written.lock();
            written.extend_from_slice(text.as_bytes());
            written.push(b'\n');
        }

        fn raw(&self, text: &str) {
            self.written.lock().extend_from_slice(text.as_bytes());
        }

        fn flush_raw(&self) {
            *self.flushes.lock() += 1;
        }

        fn request_repaint(&self) {}

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn owns_screen(&self) -> bool {
            self.owns_screen
        }
    }

    /// Return the process to the pristine "nobody installed a sink" state,
    /// even when an assertion in the guarded test panics.
    struct GlobalGuard;

    impl Drop for GlobalGuard {
        fn drop(&mut self) {
            clear_global_sink();
        }
    }

    /// Serialize plus reset: the two properties every global-sink test needs.
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
            "the fallback must not claim to own the screen"
        );
        assert!(
            !global_sink().owns_screen(),
            "StdoutSink must not own the screen"
        );
    }

    #[test]
    fn install_and_clear_switch_the_global_sink() {
        let (_guard, _global) = lock_global();

        let sink: Arc<dyn OutputSink> = Arc::new(RecordingSink::with_screen(true));
        install_global_sink(Arc::clone(&sink));

        let installed = global_sink();
        assert!(
            installed.as_any().is::<RecordingSink>(),
            "installed sink was not returned"
        );
        // Every caller observes the same instance, so they share one queue.
        assert!(
            Arc::ptr_eq(&installed, &sink),
            "global_sink cloned the sink"
        );
        assert!(screen_owned(), "screen_owned ignored the installed sink");

        clear_global_sink();
        assert!(
            global_sink().as_any().is::<StdoutSink>(),
            "clear_global_sink did not restore the fallback"
        );
        assert!(!screen_owned(), "screen_owned survived clear_global_sink");
    }

    #[test]
    fn write_bytes_relays_verbatim_through_the_sink() {
        let (_guard, _global) = lock_global();

        let sink = Arc::new(RecordingSink::with_screen(true));
        install_global_sink(Arc::clone(&sink) as Arc<dyn OutputSink>);

        // Verbatim: the relay must not invent terminators or drop newlines,
        // and a chunk that ends mid-line must stay one fragment.
        write_bytes(b"a\nb");
        assert_eq!(sink.bytes(), "a\nb");

        write_bytes(b"");
        assert_eq!(sink.bytes(), "a\nb", "empty buffer wrote bytes");

        write_bytes(b"\n");
        assert_eq!(sink.bytes(), "a\nb\n");

        // The relay decides when a row ended: `write_bytes` must not commit
        // the unterminated tail itself, or a chunk boundary would split a row.
        assert_eq!(sink.flushes(), 0, "write_bytes flushed the raw tail");
    }
}
