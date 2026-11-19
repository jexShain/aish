//! Three-second demo of the dual-channel renderer.
//!
//! Shows a spinner plus counters in the viewport and appends one history row
//! every 500 ms. What to observe on a real terminal:
//!
//! (a) committed history rows are never rewritten — scroll up and every line is
//!     a single, final copy (monotonic ids, one write per batch);
//! (b) the viewport refreshes in place without flicker (DEC 2026 + row diff);
//! (c) after exit the terminal is intact: cursor visible, autowrap restored,
//!     raw mode off.
//!
//! Run `cargo run -p aish-tui --bin tui-demo`; `timeout 5 …` makes it
//! self-terminating for scripted checks. Ctrl+C exits early.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use aish_tui::render::visible_width;
use aish_tui::{detect_caps, prepare_lines, Frame, HistoryBatch, PreparedLine, Renderer};
use crossterm::tty::IsTty;

/// Frames per second cap.
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
/// One committed history row per tick.
const HISTORY_INTERVAL: Duration = Duration::from_millis(500);
/// Total demo duration.
const DEMO_DURATION: Duration = Duration::from_secs(3);

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn main() -> io::Result<()> {
    match run() {
        // A closed pipe (`| head`) is a normal way to stop watching the demo.
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn run() -> io::Result<()> {
    // The true geometry, unsullied: the renderer addresses rows absolutely, so
    // shrinking the height here would paint at rows the terminal does not have.
    let caps = detect_caps();

    // A pipe has no terminal modes to set and no keys to read: still paint the
    // frames (so the bytes can be inspected) but leave raw mode alone.
    let interactive = io::stdout().is_tty();
    let mut renderer = Renderer::new(io::BufWriter::with_capacity(8192, io::stdout()), caps);
    let _raw = if interactive {
        Some(RawModeGuard::enter()?)
    } else {
        None
    };

    let started = Instant::now();
    let mut next_history = Duration::ZERO;
    let mut history_id = 0u64;
    let mut committed_rows = 0usize;
    let mut tick = 0usize;
    let mut next_frame = Instant::now();
    let mut interrupted = false;

    while started.elapsed() < DEMO_DURATION {
        if interactive && interrupted_key()? {
            interrupted = true;
            break;
        }

        let elapsed = started.elapsed();
        let mut batch = None;
        if elapsed >= next_history {
            history_id += 1;
            next_history += HISTORY_INTERVAL;
            let rows = prepare_lines(
                [format!(
                    "\x1b[2mhistory #{history_id:02}\x1b[0m  committed at {:.1}s  (row {} of the scrollback)",
                    elapsed.as_secs_f32(),
                    committed_rows + 1
                )],
                caps.width,
            );
            committed_rows += rows.len();
            batch = Some(HistoryBatch::new(history_id, rows));
        }

        tick += 1;
        let viewport = build_viewport(&caps, tick, history_id, committed_rows);
        renderer.draw(Frame {
            history: batch,
            viewport,
        })?;

        next_frame += FRAME_INTERVAL;
        let now = Instant::now();
        if next_frame > now {
            std::thread::sleep(next_frame - now);
        } else {
            next_frame = now;
        }
    }

    // Commit one final line so the exit is visible in scrollback.
    let final_id = history_id + 1;
    let rows = prepare_lines(
        [format!(
            "\x1b[2mtui-demo finished after {:.2}s{}: {committed_rows} history rows committed\x1b[0m",
            started.elapsed().as_secs_f32(),
            if interrupted { " (Ctrl+C)" } else { "" }
        )],
        caps.width,
    );
    renderer.draw(Frame {
        history: Some(HistoryBatch::new(final_id, rows)),
        viewport: Vec::new(),
    })?;
    let acked = renderer.acked_history_id();
    let committed = renderer.history_rows();
    drop(renderer);

    // Diagnostics go to stderr so the painted frames stay clean.
    eprintln!(
        "tui-demo: acked history id {acked}, {committed} rows, interactive={interactive}, sync_output={}",
        aish_tui::detect_caps().synchronized_output
    );
    Ok(())
}

/// Build the live viewport: header, spinner + counters, rule, hint, footer.
fn build_viewport(
    caps: &aish_tui::TerminalCaps,
    tick: usize,
    history_id: u64,
    committed: usize,
) -> Vec<PreparedLine> {
    let spinner = SPINNER[tick % SPINNER.len()];
    let rule = "─".repeat((caps.width as usize).min(60));
    let footer = format!("geometry {}x{} cols", caps.width, caps.height);
    let rows = [
        "\x1b[36m── aish-tui dual-channel demo ──────────────────────────\x1b[0m".to_string(),
        format!("{spinner} \x1b[1mviewport\x1b[0m tick {tick:<6} history id {history_id:<3}"),
        format!("\x1b[2m{rule}\x1b[0m"),
        format!("已提交历史行: {committed}   Ctrl+C 退出   3 秒演示"),
        format!(
            "\x1b[2m{footer}, footer width {} of {}\x1b[0m",
            visible_width(&footer),
            caps.width
        ),
    ];
    prepare_lines(rows.iter().map(String::as_str), caps.width)
}

/// Non-blocking Ctrl+C check, so the demo exits promptly in raw mode.
fn interrupted_key() -> io::Result<bool> {
    use crossterm::event::{poll, read, Event, KeyCode, KeyEventKind, KeyModifiers};

    if !poll(Duration::ZERO)? {
        return Ok(false);
    }
    let Event::Key(key) = read()? else {
        return Ok(false);
    };
    Ok(key.kind == KeyEventKind::Press
        && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Owns raw mode and restores every terminal mode the demo changed, including
/// when the process is interrupted.
struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        // Reset attributes, re-enable autowrap, show the cursor, then leave raw
        // mode: a frame may have been torn mid-paint on the interrupt path.
        let _ = out.write_all(b"\x1b[0m\x1b[?7h\x1b[?25h");
        let _ = out.write_all(b"\r\n");
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
