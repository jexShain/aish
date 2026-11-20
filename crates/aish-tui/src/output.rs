//! Product-output sinks: the single place product text may enter the terminal.
//!
//! # Why not `println!`
//!
//! A frame is one *atomic* write. The renderer brackets it with a hidden
//! cursor, synchronized output and autowrap disabled, and it borrows the
//! terminal's row grid for the whole viewport: it remembers which row the live
//! band started on and rewrites only the rows whose text changed.
//!
//! A `println!` from anywhere else — a background task, an LLM streaming
//! callback, a log line — lands *inside* those bytes. The terminal then moves
//! the cursor and scrolls by an amount the renderer never saw, so its memo is
//! off by that many rows: the next frame repaints history, duplicates rows, or
//! erases the prompt. There is no way to recover from that inside the
//! renderer; the damaged bytes are already on the wire.
//!
//! So product output does not go to the terminal directly. It goes to an
//! [`OutputSink`], which is either:
//!
//! * [`StdoutSink`] — the legacy path, used when no renderer owns the screen
//!   (tests, one-shot CLI output, a REPL that is not in raw mode); or
//! * [`QueueSink`] — the interactive path. Rows are buffered and handed to the
//!   REPL, which commits them through its normal history channel
//!   ([`crate::frame::HistoryBatch`]) immediately *before* it paints the next
//!   frame. History is append-only and written once, so product output can
//!   never land mid-frame no matter which thread produced it.
//!
//! A sink is `Send + Sync` because producers (loggers, streaming deltas) run
//! on arbitrary threads while the REPL owns the main one. Ordering is
//! therefore only guaranteed per sink call, and [`QueueSink`] keeps even a
//! fragment stream (`raw`) coherent by buffering the unterminated tail.

use std::any::Any;
use std::collections::VecDeque;
use std::io::{self, Write};

use parking_lot::Mutex;

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
    /// The REPL needs `QueueSink`'s `drain` method, which cannot live on the
    /// trait: a sink that writes immediately has nothing to drain. An inherent
    /// method plus this hook keeps the trait minimal for implementors.
    fn as_any(&self) -> &dyn Any;
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

/// Lines and the unterminated tail of a [`QueueSink`].
///
/// Both live behind one mutex so a fragment stream and a `line` call from
/// different threads cannot reorder a row relative to its own tail.
#[derive(Debug, Default)]
struct QueueState {
    /// Complete rows, oldest first.
    lines: VecDeque<String>,
    /// Text after the last `\n` of the most recent `raw` call.
    partial: String,
}

/// Sink that queues lines for the [`Repl`](crate::repl::Repl) to append to its
/// history channel.
///
/// Producers never touch the terminal: the REPL drains the queue at the top of
/// every frame and commits the rows through the history channel, which is
/// write-once and atomic. That is what makes it safe for a background task to
/// print while the user is typing.
#[derive(Debug, Default)]
pub struct QueueSink {
    state: Mutex<QueueState>,
}

impl QueueSink {
    /// Empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take every completed row, oldest first.
    ///
    /// The unterminated tail stays buffered: it is not a row yet, and
    /// committing it early would split one logical line into two history rows.
    pub fn drain(&self) -> Vec<String> {
        let mut state = self.state.lock();
        state.lines.drain(..).collect()
    }
}

impl OutputSink for QueueSink {
    fn line(&self, text: &str) {
        // A pending fragment precedes this line in the output stream, so it is
        // committed first; otherwise the rows would swap places.
        self.flush_raw();
        // The sink supplies the terminator. A single trailing newline is the
        // caller saying "this is one row", not a request for a blank row; any
        // other newline splits the text into several rows.
        let body = text.strip_suffix('\n').unwrap_or(text);
        let mut state = self.state.lock();
        for row in body.split('\n') {
            state.lines.push_back(row.to_string());
        }
    }

    fn raw(&self, text: &str) {
        let mut state = self.state.lock();
        let mut rest = text;
        while let Some(newline) = rest.find('\n') {
            state.partial.push_str(&rest[..newline]);
            let row = std::mem::take(&mut state.partial);
            state.lines.push_back(row);
            rest = &rest[newline + 1..];
        }
        state.partial.push_str(rest);
    }

    fn flush_raw(&self) {
        let mut state = self.state.lock();
        if state.partial.is_empty() {
            return;
        }
        let row = std::mem::take(&mut state.partial);
        state.lines.push_back(row);
    }

    fn request_repaint(&self) {}

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test double for [`StdoutSink`]: the same byte contract, without writing
    /// to fd 1 (which libtest does not capture).
    #[derive(Debug, Default)]
    struct RecordingSink {
        written: Mutex<Vec<u8>>,
        flushes: Mutex<usize>,
    }

    impl RecordingSink {
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
    }

    #[test]
    fn queue_sink_buffers_partial_raw_until_flush() {
        let sink = QueueSink::new();

        // A fragment without a newline is not a row yet.
        sink.raw("partial");
        assert!(
            sink.drain().is_empty(),
            "unterminated fragment must stay buffered"
        );

        // The newline of the next fragment completes the buffered prefix.
        sink.raw(" row\n");
        assert_eq!(sink.drain(), ["partial row"]);

        // A tail after the last newline waits for `flush_raw`.
        sink.raw("tail");
        assert!(sink.drain().is_empty(), "tail committed too early");
        sink.flush_raw();
        assert_eq!(sink.drain(), ["tail"]);

        // Flushing an empty buffer commits nothing.
        sink.flush_raw();
        assert!(sink.drain().is_empty(), "flush_raw invented a row");
    }

    #[test]
    fn queue_sink_line_appends_newline() {
        let sink = QueueSink::new();

        // No trailing newline in the input: the sink supplies it, so exactly
        // one complete row comes out.
        sink.line("hello");
        assert_eq!(sink.drain(), ["hello"]);

        // A pending fragment is committed before the line, preserving order.
        sink.raw("dangling");
        sink.line("next");
        assert_eq!(sink.drain(), ["dangling", "next"]);

        // An empty line is still a row.
        sink.line("");
        assert_eq!(sink.drain(), [""]);

        // Drain takes everything: a second drain is empty.
        assert!(sink.drain().is_empty(), "drain did not empty the queue");
    }

    #[test]
    fn stdout_sink_writes_verbatim() {
        let sink = RecordingSink::default();
        sink.line("one");
        sink.raw("two");
        sink.raw(" and a half");
        sink.flush_raw();
        assert_eq!(sink.bytes(), "one\ntwo and a half");
        assert_eq!(sink.flushes(), 1);
        // Repaint requests are a no-op for a sink that writes immediately.
        sink.request_repaint();
        assert_eq!(sink.bytes(), "one\ntwo and a half");
    }
}
