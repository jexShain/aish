//! The fullscreen app: alternate screen, key loop, and frame scheduling.
//!
//! [`App::run`] owns the terminal for the duration of the fullscreen mode and
//! returns when the user leaves it. Two things it deliberately does **not**
//! do:
//!
//! * it never touches the same screen as the inline shell — the alternate
//!   screen is entered on the way in and left on the way out, so the user's
//!   scrollback comes back byte-for-byte;
//! * it never decides *what* the shell is doing. [`AppHooks::snapshot`] is
//!   pulled once per frame and [`AppHooks::on_key`] may update the shell; the
//!   app is a view over that state.

use std::io::{self, Read, Write};

use crate::screen::{Diff, Screen};
use crate::workspace::{draw, Palette, Tab, Workspace};

/// Alternate-screen entry; leaving it restores the previous screen exactly.
const ENTER_ALT: &str = "\x1b[?1049h";
/// Alternate-screen exit.
const LEAVE_ALT: &str = "\x1b[?1049l";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
/// Synchronized output (DEC 2026): a frame lands as one paint, not a flicker.
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

/// One recognised key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// Leave the fullscreen mode.
    Close,
    /// Switch to a specific tab.
    Tab(Tab),
    /// Next tab, wrapping.
    NextTab,
    /// Previous tab, wrapping.
    PreviousTab,
    /// Move the selection up.
    Up,
    /// Move the selection down.
    Down,
    /// Anything else, already consumed.
    Ignore,
}

impl Key {
    /// Parse the first key out of `bytes`, returning it and its byte length.
    ///
    /// `None` means "not enough bytes yet": a partial escape sequence is never
    /// reported as a malformed key, or the caller would drop the rest of a
    /// sequence split across two reads.
    pub fn parse(bytes: &[u8]) -> Option<(Key, usize)> {
        let first = *bytes.first()?;
        Some(match first {
            b'q' => (Key::Close, 1),
            b'1' => (Key::Tab(Tab::Mission), 1),
            b'2' => (Key::Tab(Tab::Agents), 1),
            b'3' => (Key::Tab(Tab::Tasks), 1),
            b'4' => (Key::Tab(Tab::Log), 1),
            b'\t' => (Key::NextTab, 1),
            b'k' => (Key::Up, 1),
            b'j' => (Key::Down, 1),
            0x1b => match bytes.get(1) {
                None => return None,
                Some(b'[') => match bytes.get(2) {
                    None => return None,
                    Some(b'Z') => (Key::PreviousTab, 3),
                    Some(b'A') => (Key::Up, 3),
                    Some(b'B') => (Key::Down, 3),
                    Some(_) => (Key::Ignore, 3),
                },
                // A bare Escape closes; a longer sequence is not ours.
                Some(_) => (Key::Close, 1),
            },
            _ => (Key::Ignore, 1),
        })
    }
}

/// What the app needs from the shell.
pub trait AppHooks {
    /// The current view of the shell, pulled once per frame.
    fn snapshot(&self) -> Workspace;
    /// Whether the user asked to leave (Shift+Tab from inside the app, or a
    /// quit from the shell side).
    fn should_close(&self) -> bool;
    /// Called after the app has applied a key to its own view state, so the
    /// shell can react (open a pane, stop a task, …).
    fn on_key(&mut self, _key: Key, _workspace: &mut Workspace) {}
}

/// Run the fullscreen mode on the real terminal until the user leaves.
///
/// Returns after the alternate screen has been restored. `on_close` is not
/// needed: the caller regains control on return.
pub fn run<H: AppHooks>(mut hooks: H) -> io::Result<()> {
    let palette = Palette::default();
    let mut stdout = io::stdout();
    let mut workspace = hooks.snapshot();

    // Raw mode is what makes a key press reach `read` at all: cooked mode
    // buffers a whole line, so every key would wait for Enter.
    crossterm::terminal::enable_raw_mode()?;
    let result = (|| -> io::Result<()> {
        stdout.write_all(ENTER_ALT.as_bytes())?;
        stdout.write_all(HIDE_CURSOR.as_bytes())?;
        stdout.flush()?;

        let (width, height) = crossterm::terminal::size()?;
        let mut screen = Screen::new(width, height);
        draw(&workspace, &palette, &mut screen);
        write_diff(&mut stdout, &screen.render_full())?;
        let mut previous = screen.clone();

        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut buffer = [0u8; 32];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            if hooks.should_close() {
                return Ok(());
            }
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            pending.extend_from_slice(&buffer[..read]);
            // A read is not a key press: several keys can arrive in one read,
            // and one sequence can be split across two.
            while let Some((key, consumed)) = Key::parse(&pending) {
                pending.drain(..consumed);
                if key == Key::Close {
                    return Ok(());
                }
                apply_key(key, &mut workspace);
                hooks.on_key(key, &mut workspace);
                draw(&workspace, &palette, &mut screen);
                write_diff(&mut stdout, &screen.diff(&previous))?;
                previous = screen.clone();
            }
        }
    })();

    // Restoring the alternate screen is what returns the user's scrollback
    // exactly as it was, so it happens on every exit path including errors.
    let _ = stdout.write_all(LEAVE_ALT.as_bytes());
    let _ = stdout.write_all(SHOW_CURSOR.as_bytes());
    let _ = stdout.flush();
    let _ = crossterm::terminal::disable_raw_mode();
    result
}

/// Apply a navigation key to the view state.
fn apply_key(key: Key, workspace: &mut Workspace) {
    match key {
        Key::Close | Key::Ignore => {}
        Key::Tab(tab) => workspace.tab = tab,
        Key::NextTab => workspace.tab = workspace.tab.next(),
        Key::PreviousTab => workspace.tab = workspace.tab.previous(),
        Key::Up => workspace.selected = workspace.selected.saturating_sub(1),
        Key::Down => {
            let last = workspace.agents.len().saturating_sub(1);
            workspace.selected = (workspace.selected + 1).min(last);
        }
    }
}

fn write_diff(out: &mut impl Write, diff: &Diff) -> io::Result<()> {
    out.write_all(SYNC_BEGIN.as_bytes())?;
    out.write_all(&diff.bytes)?;
    out.write_all(SYNC_END.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_select_tabs() {
        assert_eq!(Key::parse(b"1"), Some((Key::Tab(Tab::Mission), 1)));
        assert_eq!(Key::parse(b"4"), Some((Key::Tab(Tab::Log), 1)));
        assert_eq!(Key::parse(b"9"), Some((Key::Ignore, 1)));
    }

    #[test]
    fn tab_and_back_tab_cycle() {
        assert_eq!(Key::parse(b"\t"), Some((Key::NextTab, 1)));
        // Shift+Tab arrives as CSI Z.
        assert_eq!(Key::parse(b"\x1b[Z"), Some((Key::PreviousTab, 3)));
    }

    #[test]
    fn arrows_move_the_selection() {
        assert_eq!(Key::parse(b"\x1b[A"), Some((Key::Up, 3)));
        assert_eq!(Key::parse(b"\x1b[B"), Some((Key::Down, 3)));
        assert_eq!(Key::parse(b"j"), Some((Key::Down, 1)));
        assert_eq!(Key::parse(b"k"), Some((Key::Up, 1)));
    }

    #[test]
    fn a_partial_escape_sequence_is_never_reported_as_a_key() {
        // The caller must keep buffering instead of dropping the tail.
        assert_eq!(Key::parse(b"\x1b"), None);
        assert_eq!(Key::parse(b"\x1b["), None);
        assert_eq!(Key::parse(b""), None);
    }

    #[test]
    fn q_closes() {
        assert_eq!(Key::parse(b"q"), Some((Key::Close, 1)));
    }

    #[test]
    fn selection_clamps_at_both_ends() {
        let mut workspace = Workspace {
            agents: (0..3)
                .map(|i| crate::workspace::AgentLine {
                    name: format!("a{i}"),
                    state: crate::workspace::AgentState::Planned,
                    activity: String::new(),
                    elapsed: String::new(),
                })
                .collect(),
            selected: 0,
            ..Default::default()
        };
        apply_key(Key::Up, &mut workspace);
        assert_eq!(workspace.selected, 0, "Up at the top must not wrap");
        for _ in 0..10 {
            apply_key(Key::Down, &mut workspace);
        }
        assert_eq!(workspace.selected, 2, "Down past the end must stop");
    }
}
