use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::theme;

/// Global lock that serializes animation frame rendering against ad-hoc
/// terminal writes (notably tracing log lines). Both the animation threads
/// and [`LogLineWriter`] must hold this lock while emitting bytes to stdout,
/// guaranteeing a log event can never interleave with a partial frame.
static FRAME_LOCK: Mutex<()> = Mutex::new(());

/// Whether an animation frame is currently on screen. Read by
/// [`LogLineWriter`] to decide whether the current line must be wiped
/// before a log line is emitted.
static ANIMATION_FRAME_VISIBLE: AtomicBool = AtomicBool::new(false);

/// `io::Write` adapter used by the tracing subscriber (via `MakeWriter`) so
/// log lines never corrupt the interactive screen.
///
/// # Two modes
///
/// * A renderer owns the screen (the dual-channel TUI installed a sink whose
///   [`owns_screen`](aish_core::output::OutputSink::owns_screen) is true).
///   Writing fd 1 would land inside a frame and desynchronise the renderer's
///   row memo, so the bytes go through the sink instead: it queues rows that
///   the REPL commits immediately before its next paint.
/// * No renderer (legacy REPL, one-shot CLI, tests). Nothing owns the screen,
///   so the line is written straight to stdout. A visible animation frame is
///   wiped first so the log does not glue onto "Thinking 1.7s".
///
/// The subscriber formats one record per `write` call and terminates it with
/// `\n`, so the buffer is one complete row: the sink's `line` is the right
/// entry point (a `raw` fragment stream would be for a producer whose chunk
/// boundaries are not row boundaries).
pub struct LogLineWriter {
    started: bool,
}

impl LogLineWriter {
    pub fn new() -> Self {
        Self { started: false }
    }

    /// Wipe an in-flight animation frame and write `buf` to stdout.
    ///
    /// Callers must hold [`FRAME_LOCK`]: the animation thread paints through
    /// the same lock, so a log line can never interleave with a partial frame.
    fn write_direct(&mut self, buf: &[u8]) -> io::Result<usize> {
        if !self.started {
            self.started = true;
            if ANIMATION_FRAME_VISIBLE.load(Ordering::SeqCst) {
                // Wipe the partially rendered animation line so the log
                // starts on a fresh, independent line instead of gluing
                // onto "Thinking 1.7s".
                print!("\r\x1b[2K");
                ANIMATION_FRAME_VISIBLE.store(false, Ordering::SeqCst);
            }
        }
        io::stdout().write_all(buf)?;
        Ok(buf.len())
    }
}

impl Default for LogLineWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl io::Write for LogLineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Resolve the sink once: the TUI path must never touch fd 1, and it
        // must not take FRAME_LOCK either (the animation threads are idle
        // there, and the queue sink already serialises its own state).
        let sink = aish_core::output::global_sink();
        if sink.owns_screen() {
            // The sink supplies the row terminator; a trailing `\n` is the
            // subscriber saying "one record", not a request for a blank row.
            let text = String::from_utf8_lossy(buf);
            sink.line(text.trim_end_matches('\n'));
            // `line` is an enqueue, so there is nothing to flush: flushing per
            // record would tear a frame that is mid-paint.
            return Ok(buf.len());
        }

        let _guard = FRAME_LOCK.lock();
        self.write_direct(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        // A queue sink has nothing to flush (rows are committed by the REPL),
        // and flushing stdout while a frame is on the wire is what tears it.
        if !aish_core::output::global_sink().owns_screen() {
            let _ = io::stdout().flush();
        }
        Ok(())
    }
}
/// Thread-safe animation wrapper that displays a spinner with elapsed time
/// in a background thread.
///
/// Usage: `start(text)` to begin, `stop()` to end. Thread-safe via interior
/// mutability — wrap in `Arc` for sharing across threads.
pub struct SharedAnimation {
    active: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl SharedAnimation {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }

    /// Start a shimmer animation with the given label text.
    pub fn start(&self, text: &str) {
        self.stop();

        let active = self.active.clone();
        active.store(true, Ordering::SeqCst);

        let text = text.to_string();
        let start_time = Instant::now();

        let handle = thread::Builder::new()
            .name("aish-animation".into())
            .spawn(move || {
                let _frame = FRAME_LOCK.lock();
                // Hide cursor
                print!("\x1b[?25l");
                let _ = io::stdout().flush();
                drop(_frame);

                while active.load(Ordering::SeqCst) {
                    // Auto-stop when a tool needs interactive terminal input
                    // (sudo password, ssh, vim, etc.) so the prompt is visible.
                    if aish_tools::bash::interactive_input_active() {
                        break;
                    }
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let time_ms = start_time.elapsed().as_millis() as u64;
                    let shimmered = theme::shimmer_text(&text, time_ms);
                    {
                        let _frame = FRAME_LOCK.lock();
                        if elapsed > 0.1 {
                            print!(
                                "\r\x1b[K{} {}",
                                shimmered,
                                theme::dim(&format!("{:.1}s", elapsed))
                            );
                        } else {
                            print!("\r\x1b[K{}", shimmered);
                        }
                        ANIMATION_FRAME_VISIBLE.store(true, Ordering::SeqCst);
                        let _ = io::stdout().flush();
                    }
                    for _ in 0..10 {
                        if !active.load(Ordering::SeqCst)
                            || aish_tools::bash::interactive_input_active()
                        {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                }

                // Clear animation line and restore cursor
                let _frame = FRAME_LOCK.lock();
                print!("\r\x1b[2K\x1b[?25h");
                ANIMATION_FRAME_VISIBLE.store(false, Ordering::SeqCst);
                let _ = io::stdout().flush();
            })
            .ok();

        *self.handle.lock() = handle;
    }

    /// Stop the animation and clear the line.
    pub fn stop(&self) {
        self.active.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().take() {
            let _ = h.join();
        }
    }
}

impl Default for SharedAnimation {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SharedAnimation {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Shimmer animation for sub-agent thinking lines (`  └─ explore · 思考中 2.1s`).
///
/// Separate from [`SharedAnimation`] so the main-loop animation can stay off
/// while sub-agent progress remains visibly alive.
pub struct SubAgentThinkingAnimation {
    active: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl SubAgentThinkingAnimation {
    pub fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }

    /// Animate `prefix` (dimmed) + shimmer sweep on `label`.
    pub fn start(&self, prefix: &str, label: &str) {
        self.stop();

        let active = self.active.clone();
        active.store(true, Ordering::SeqCst);

        let prefix = theme::dim(prefix);
        let label = label.to_string();
        let start_time = Instant::now();

        let handle = thread::Builder::new()
            .name("aish-subagent-animation".into())
            .spawn(move || {
                {
                    let _frame = FRAME_LOCK.lock();
                    print!("\x1b[?25l");
                    let _ = io::stdout().flush();
                }

                while active.load(Ordering::SeqCst) {
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let time_ms = start_time.elapsed().as_millis() as u64;
                    let shimmered = theme::shimmer_text(&label, time_ms);
                    {
                        let _frame = FRAME_LOCK.lock();
                        if elapsed > 0.1 {
                            print!(
                                "\r\x1b[K{}{} {}",
                                prefix,
                                shimmered,
                                theme::dim(&format!("{:.1}s", elapsed))
                            );
                        } else {
                            print!("\r\x1b[K{}{}", prefix, shimmered);
                        }
                        ANIMATION_FRAME_VISIBLE.store(true, Ordering::SeqCst);
                        let _ = io::stdout().flush();
                    }
                    for _ in 0..10 {
                        if !active.load(Ordering::SeqCst) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                }

                {
                    let _frame = FRAME_LOCK.lock();
                    print!("\r\x1b[2K\x1b[?25h");
                    ANIMATION_FRAME_VISIBLE.store(false, Ordering::SeqCst);
                    let _ = io::stdout().flush();
                }
            })
            .ok();

        *self.handle.lock() = handle;
    }

    pub fn stop(&self) {
        self.active.store(false, Ordering::SeqCst);
        if let Some(h) = self.handle.lock().take() {
            let _ = h.join();
        }
    }
}

impl Default for SubAgentThinkingAnimation {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SubAgentThinkingAnimation {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::any::Any;
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::fd::AsRawFd;
    use std::sync::Arc;

    use aish_core::output::{clear_global_sink, install_global_sink, OutputSink};

    /// The installed sink is process state, so every test here must be
    /// serialized: one's install would otherwise be visible to another's
    /// "nothing is installed" assertion, and the fd 1 redirection would
    /// capture the wrong writer.
    static SINK_LOCK: Mutex<()> = Mutex::new(());

    /// Serialize plus reset, restoring the pristine slot even on panic.
    struct SinkGuard;

    impl Drop for SinkGuard {
        fn drop(&mut self) {
            clear_global_sink();
        }
    }

    fn lock_sink() -> (parking_lot::MutexGuard<'static, ()>, SinkGuard) {
        let guard = SINK_LOCK.lock();
        clear_global_sink();
        (guard, SinkGuard)
    }

    /// Records what reached the sink, so a test can tell the TUI path (rows
    /// queued here, fd 1 untouched) from the legacy path (bytes on fd 1).
    #[derive(Debug, Default)]
    struct RecordingSink {
        lines: Mutex<Vec<String>>,
        owns_screen: bool,
    }

    impl RecordingSink {
        fn owning_screen() -> Self {
            Self {
                owns_screen: true,
                ..Self::default()
            }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().clone()
        }
    }

    impl OutputSink for RecordingSink {
        fn line(&self, text: &str) {
            self.lines.lock().push(text.to_string());
        }

        fn raw(&self, text: &str) {
            self.line(text);
        }

        fn flush_raw(&self) {}

        fn request_repaint(&self) {}

        fn as_any(&self) -> &dyn Any {
            self
        }

        fn owns_screen(&self) -> bool {
            self.owns_screen
        }
    }

    /// RAII redirection of fd 1 into a temporary file, restored on drop.
    ///
    /// libtest's output capture only intercepts `print!`, not raw fd 1 writes,
    /// so the only way to observe "the writer did not touch the terminal" is
    /// to own the descriptor. The caller holds [`SINK_LOCK`], so no other test
    /// here can race the redirection.
    struct CapturedStdout {
        file: File,
        saved: libc::c_int,
    }

    impl CapturedStdout {
        fn start() -> Self {
            let file = tempfile::tempfile().expect("temp file for stdout capture");
            let _ = io::stdout().flush();
            // SAFETY: [Category 8 — FFI] `dup`/`dup2` on valid fds. The saved
            // descriptor is restored (and closed) by `Drop`, so a panic in the
            // test body cannot leave fd 1 pointing at the capture file.
            let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
            assert!(saved >= 0, "dup(STDOUT_FILENO) failed");
            let redirected = unsafe { libc::dup2(file.as_raw_fd(), libc::STDOUT_FILENO) };
            assert!(redirected >= 0, "dup2 onto STDOUT_FILENO failed");
            Self { file, saved }
        }

        fn written(mut self) -> Vec<u8> {
            let _ = io::stdout().flush();
            let mut written = Vec::new();
            self.file
                .seek(SeekFrom::Start(0))
                .expect("rewind capture file");
            self.file
                .read_to_end(&mut written)
                .expect("read capture file");
            written
        }
    }

    impl Drop for CapturedStdout {
        fn drop(&mut self) {
            let _ = io::stdout().flush();
            // SAFETY: [Category 8 — FFI] restore and close the saved descriptor.
            unsafe {
                libc::dup2(self.saved, libc::STDOUT_FILENO);
                libc::close(self.saved);
            }
        }
    }

    /// A renderer owns the screen, so the record must reach the sink and never
    /// fd 1: a byte written during a frame desynchronises the renderer's row
    /// memo (issue: Langfuse 410 warnings flooded the TUI).
    #[test]
    fn log_lines_reach_the_sink_when_a_renderer_owns_the_screen() {
        let (_guard, _global) = lock_sink();
        let sink = Arc::new(RecordingSink::owning_screen());
        install_global_sink(Arc::clone(&sink) as Arc<dyn OutputSink>);

        // A visible frame must survive: the sink path has nothing to wipe.
        ANIMATION_FRAME_VISIBLE.store(true, Ordering::SeqCst);

        let capture = CapturedStdout::start();
        let mut writer = LogLineWriter::new();
        writer
            .write_all(b"2026-01-01T00:00:00Z  WARN langfuse: 410 Gone\n")
            .expect("write through the sink");
        writer.flush().expect("flush is a no-op for a queue sink");
        let written = capture.written();

        let record = "2026-01-01T00:00:00Z  WARN langfuse: 410 Gone";
        // libtest writes its own progress to fd 1 from other threads, so the
        // redirect is not necessarily empty; what must never appear is the
        // record itself.
        assert!(
            !String::from_utf8_lossy(&written).contains(record),
            "the TUI path wrote the record to fd 1: {:?}",
            String::from_utf8_lossy(&written)
        );
        assert_eq!(
            sink.lines(),
            [record],
            "the sink must receive the record exactly once, without its newline"
        );
        assert!(
            ANIMATION_FRAME_VISIBLE.load(Ordering::SeqCst),
            "the sink path must not wipe the animation frame"
        );

        ANIMATION_FRAME_VISIBLE.store(false, Ordering::SeqCst);
    }

    /// No renderer (legacy REPL, one-shot CLI): the record goes straight to
    /// stdout and a visible animation frame is wiped first, as before.
    #[test]
    fn log_lines_write_directly_when_nothing_owns_the_screen() {
        let (_guard, _global) = lock_sink();
        let sink = Arc::new(RecordingSink::default());
        install_global_sink(Arc::clone(&sink) as Arc<dyn OutputSink>);

        ANIMATION_FRAME_VISIBLE.store(true, Ordering::SeqCst);

        let capture = CapturedStdout::start();
        let mut writer = LogLineWriter::new();
        writer
            .write_all(b"2026-01-01T00:00:00Z  WARN legacy: visible\n")
            .expect("write to stdout");
        writer.flush().expect("flush stdout");
        let written = capture.written();

        assert!(
            sink.lines().is_empty(),
            "a sink that does not own the screen must not receive rows"
        );
        // The record itself goes to fd 1 verbatim. (The frame wipe uses
        // `print!`, which libtest captures before it reaches the descriptor,
        // and libtest writes its own progress from other threads, so the
        // redirect is neither empty nor escape-free by contract — only the
        // record's arrival is.)
        assert!(
            String::from_utf8_lossy(&written).ends_with("WARN legacy: visible\n"),
            "the legacy path must write the record straight to fd 1: {:?}",
            String::from_utf8_lossy(&written)
        );
        // The wipe branch still ran, which is what the flag proves.
        assert!(
            !ANIMATION_FRAME_VISIBLE.load(Ordering::SeqCst),
            "the wipe must clear the frame-visible flag"
        );
    }

    /// Regression test for issue #490: log lines written through
    /// [`LogLineWriter`] while an animation is active must not deadlock and
    /// must complete (frame lock serializes animation frames against log
    /// writes). Output goes to the test-captured stdout; we assert the
    /// interactions finish and the visibility flag is reset after stop.
    #[test]
    fn log_writer_and_animation_do_not_deadlock_or_leave_frame_visible() {
        // The animation thread paints fd 1, so it must not race the capture.
        let (_guard, _global) = lock_sink();

        let anim = SharedAnimation::new();
        anim.start("思考中");

        let writer_threads: Vec<_> = (0..4)
            .map(|_| {
                thread::spawn(|| {
                    for _ in 0..50 {
                        let mut w = LogLineWriter::new();
                        w.write_all(b"2026-01-01T00:00:00Z  WARN test: log line\n")
                            .unwrap();
                        w.flush().unwrap();
                    }
                })
            })
            .collect();

        // Give writers time to interleave with animation frames.
        thread::sleep(Duration::from_millis(200));
        anim.stop();

        for t in writer_threads {
            t.join().expect("writer threads must finish");
        }

        assert!(
            !ANIMATION_FRAME_VISIBLE.load(Ordering::SeqCst),
            "animation stop must clear the frame-visible flag"
        );
    }
}
