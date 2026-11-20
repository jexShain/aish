//! Interactive demo REPL for the dual-channel renderer.
//!
//! Run `cargo run -p aish-tui --bin aish-tui-repl`. What to observe on a real
//! terminal:
//!
//! * every submitted line and every command's output is committed **once**:
//!   scroll up and each row has exactly one copy (the history channel);
//! * the prompt, the completion menu and the dim ghost suggestion live in the
//!   **viewport** and are repainted in place without touching committed rows;
//! * after exit the terminal is intact (autowrap on, cursor visible, raw mode
//!   off) because the guard restores every mode even on a panic.
//!
//! The REPL itself is [`aish_tui::repl::run_standalone`], shared verbatim with
//! `AISH_UI=v2` in `aish-cli`; this binary only supplies the demo wording.

use std::io;

/// Demo prompt: two glyphs and an arrow, wide enough to exercise prompt-aware
/// wrapping in the editor (it counts as 11 display columns).
const PROMPT: &str = "◆ aish-tui ➜ ";

fn main() -> io::Result<()> {
    match aish_tui::repl::run_standalone(PROMPT, aish_tui::repl::default_history_path()) {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}
