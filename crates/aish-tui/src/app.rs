//! The fullscreen app: alternate screen, event loop, and frame scheduling.
//!
//! [`run`] owns the terminal for the duration of the fullscreen mode and
//! returns when the user leaves it. Two things it deliberately does **not**
//! do:
//!
//! * it never touches the same screen as the inline shell — the alternate
//!   screen is entered on the way in and left on the way out, so the user's
//!   scrollback comes back byte-for-byte;
//! * it never decides *what* the shell is doing. [`AppHooks::snapshot`] is
//!   pulled once per frame and [`AppHooks::on_idle`] is the only place the
//!   shell is allowed to advance its own work.
//!
//! # Why an event loop instead of blocking reads
//!
//! A blocking `read` would leave the panel frozen between keystrokes, so a
//! worker finishing in the background could not be shown until the user
//! pressed something. The loop polls with a timeout instead: keystrokes wake
//! it immediately, and the timeout is what makes the panel live.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;

use crate::screen::{Diff, Screen};
use crate::workspace::{draw, Composer, Palette, Tab, Workspace};

/// Alternate-screen entry; leaving it restores the previous screen exactly.
const ENTER_ALT: &str = "\x1b[?1049h";
/// Alternate-screen exit.
const LEAVE_ALT: &str = "\x1b[?1049l";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
/// Synchronized output (DEC 2026): a frame lands as one paint, not a flicker.
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

/// How long the loop waits for a key before advancing the shell.
///
/// Short enough that a worker finishing is visible almost at once, long enough
/// that an idle panel costs nothing measurable.
pub const TICK: Duration = Duration::from_millis(500);

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
    /// Open the composer to answer the selected ticket's question.
    Answer,
    /// Open the composer to send the selected ticket a message.
    Say,
    /// Retry the selected ticket.
    Retry,
    /// Stop the selected ticket.
    Stop,
    /// Confirm what was typed.
    Submit,
    /// Abandon what was typed.
    Cancel,
    /// Anything else, already consumed.
    Ignore,
}

impl Key {
    /// Translate a crossterm event into a key, or `None` when it is not one we
    /// act on (a mouse move, a resize).
    ///
    /// Composer text is handled here too: when one is open, printable
    /// characters arrive as [`Key::Ignore`] and the caller feeds them to the
    /// draft, so the mapping stays in one place.
    pub fn from_event(event: &Event) -> Option<Self> {
        let Event::Key(key) = event else {
            return None;
        };
        Some(Self::from_key_event(*key))
    }

    /// Translate a key event.
    pub fn from_key_event(key: KeyEvent) -> Self {
        // Ctrl+C always means "get me out", even mid-sentence.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Key::Close;
        }
        match key.code {
            KeyCode::Esc => Key::Cancel,
            KeyCode::Enter => Key::Submit,
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    Key::PreviousTab
                } else {
                    Key::NextTab
                }
            }
            KeyCode::BackTab => Key::PreviousTab,
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Char('q') => Key::Close,
            KeyCode::Char('1') => Key::Tab(Tab::Mission),
            KeyCode::Char('2') => Key::Tab(Tab::Agents),
            KeyCode::Char('3') => Key::Tab(Tab::Tasks),
            KeyCode::Char('4') => Key::Tab(Tab::Log),
            KeyCode::Char('a') => Key::Answer,
            KeyCode::Char('s') => Key::Say,
            KeyCode::Char('r') => Key::Retry,
            KeyCode::Char('x') => Key::Stop,
            KeyCode::Char('j') => Key::Down,
            KeyCode::Char('k') => Key::Up,
            _ => Key::Ignore,
        }
    }

    /// The character this key types, if any.
    ///
    /// Only plain characters count: a modified key is a command, not text.
    pub fn typed_char(event: &Event) -> Option<char> {
        let Event::Key(key) = event else {
            return None;
        };
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char(ch) => Some(ch),
            _ => None,
        }
    }
}

/// What the app needs from the shell.
pub trait AppHooks {
    /// The current view of the shell, pulled once per frame.
    fn snapshot(&self) -> Workspace;

    /// Advance the shell's own work (a supervisor pass, a poll).
    ///
    /// Called on every loop tick, including the ones no key woke. Returning
    /// `true` means something changed and the frame must be repainted.
    fn on_idle(&mut self) -> bool {
        false
    }

    /// Act on a key once the app has applied it to its own view state.
    ///
    /// The workspace handed in is post-application, so a hook that answers a
    /// question already knows which row was selected when the key was pressed.
    fn on_key(&mut self, _key: Key, _workspace: &mut Workspace) {}

    /// Take a completed composer for a ticket.
    ///
    /// Split from [`AppHooks::on_key`] because submitting is the one action
    /// with text in it; keeping it separate means a hook cannot accidentally
    /// treat a navigation key as a message.
    fn on_composer(&mut self, _ticket: &str, _purpose: Composer, _text: &str) {}
}

/// Run the fullscreen mode on the real terminal until the user leaves.
pub fn run<H: AppHooks>(mut hooks: H) -> io::Result<()> {
    let palette = Palette::default();
    let mut stdout = io::stdout();

    // Raw mode is what makes a key press reach the loop at all: cooked mode
    // buffers a whole line, so every key would wait for Enter.
    terminal::enable_raw_mode()?;
    let result = (|| -> io::Result<()> {
        stdout.write_all(ENTER_ALT.as_bytes())?;
        stdout.write_all(HIDE_CURSOR.as_bytes())?;
        stdout.flush()?;

        let (width, height) = terminal::size()?;
        let mut screen = Screen::new(width, height);
        let mut workspace = hooks.snapshot();
        draw(&workspace, &palette, &mut screen);
        write_diff(&mut stdout, &screen.render_full())?;
        let mut previous = screen.clone();

        loop {
            // A resize invalidates every cell; regenerate the grid first.
            if let Some((new_width, new_height)) = check_resize(&mut screen) {
                let _ = (new_width, new_height);
            }

            let mut dirty = false;
            if event::poll(TICK)? {
                match event::read()? {
                    Event::Key(key_event) => {
                        let key = Key::from_key_event(key_event);
                        // With a composer open, a plain character is text
                        // before it is a command: that is what lets "a" be
                        // both "answer" and the letter in a message.
                        if workspace.composer != Composer::Closed {
                            if let Some(ch) = Key::typed_char(&Event::Key(key_event)) {
                                workspace.insert_char(ch);
                                draw(&workspace, &palette, &mut screen);
                                write_diff(&mut stdout, &screen.diff(&previous))?;
                                previous = screen.clone();
                                continue;
                            }
                            if key_event.code == KeyCode::Backspace {
                                workspace.backspace();
                                draw(&workspace, &palette, &mut screen);
                                write_diff(&mut stdout, &screen.diff(&previous))?;
                                previous = screen.clone();
                                continue;
                            }
                        }
                        if apply_key(key, &mut workspace, &mut hooks) {
                            return Ok(());
                        }
                        // The hook may have changed the shell's state (it
                        // answered a question, stopped a worker). Re-pull the
                        // view so that change is painted in this same frame
                        // instead of waiting for the next tick, which would
                        // look like the key did nothing.
                        workspace = merge_snapshot(hooks.snapshot(), &workspace);
                        dirty = true;
                    }
                    Event::Paste(text) if workspace.composer != Composer::Closed => {
                        // A paste is text for the composer, and nothing at all
                        // when nothing is being composed.
                        for ch in text.chars() {
                            workspace.insert_char(ch);
                        }
                        dirty = true;
                    }
                    Event::Resize(w, h) => {
                        screen = Screen::new(w, h);
                        previous = Screen::new(w, h);
                        dirty = true;
                    }
                    _ => {}
                }
            } else if hooks.on_idle() {
                // Something changed while nobody was typing: repaint from the
                // shell's new state.
                workspace = merge_snapshot(hooks.snapshot(), &workspace);
                dirty = true;
            }

            if dirty {
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
    let _ = terminal::disable_raw_mode();
    result
}

/// Apply a navigation key. Returns `true` when the app should close.
fn apply_key<H: AppHooks>(key: Key, workspace: &mut Workspace, hooks: &mut H) -> bool {
    if key == Key::Close {
        return true;
    }

    // A composer owns the keyboard: navigation keys move the cursor or close
    // it, so a half-typed message is never lost to a stray arrow key.
    if workspace.composer != Composer::Closed {
        match key {
            Key::Submit => {
                let Some(agent) = workspace.selected_agent() else {
                    workspace.close_composer();
                    return false;
                };
                let ticket = agent.id.clone();
                let (purpose, text) = workspace.take_draft();
                if !text.trim().is_empty() {
                    hooks.on_composer(&ticket, purpose, &text);
                }
                hooks.on_key(Key::Submit, workspace);
                return false;
            }
            Key::Cancel => {
                workspace.close_composer();
                return false;
            }
            _ => return false,
        }
    }

    match key {
        Key::Close => return true,
        Key::Tab(tab) => workspace.tab = tab,
        Key::NextTab => workspace.tab = workspace.tab.next(),
        Key::PreviousTab => workspace.tab = workspace.tab.previous(),
        Key::Up => workspace.step_selection(-1),
        Key::Down => workspace.step_selection(1),
        Key::Answer => {
            let has_question = workspace
                .selected_agent()
                .is_some_and(|agent| agent.state == crate::workspace::AgentState::Waits);
            if has_question {
                workspace.open_composer(Composer::Answer, "");
            }
        }
        Key::Say => {
            if workspace.selected_agent().is_some() {
                workspace.open_composer(Composer::Say, "");
            }
        }
        // Retry and Stop are gated inside the hook, which knows whether they
        // apply; the app only routes the key.
        Key::Retry | Key::Stop | Key::Submit | Key::Cancel | Key::Ignore => {}
    }
    // Exactly one dispatch per key: a hook that mutates state is not
    // idempotent, so calling it here *and* in a match arm would run it twice
    // (the second stop was being refused as already-ended).
    hooks.on_key(key, workspace);
    false
}

/// Combine a fresh shell snapshot with the app's own editing state.
///
/// The shell owns tickets; the app owns the cursor, the selection and the
/// half-typed message. Replacing the workspace wholesale would throw away a
/// draft mid-sentence (a tick between two keystrokes would erase it), so the
/// local fields are carried across.
fn merge_snapshot(fresh: Workspace, local: &Workspace) -> Workspace {
    Workspace {
        draft: local.draft.clone(),
        draft_cursor: local.draft_cursor,
        composer: local.composer,
        // Keep the selection only while it still points at a row: a ticket
        // that ended may have shrunk the list.
        selected: local.selected.min(fresh.agents.len().saturating_sub(1)),
        tab: local.tab,
        ..fresh
    }
}

/// Re-read the terminal size and rebuild the grid when it changed.
///
/// Returns the new geometry when a resize happened, so the caller can repaint
/// in full (a resized grid shares no cells with the old one).
fn check_resize(screen: &mut Screen) -> Option<(u16, u16)> {
    let (w, h) = terminal::size().ok()?;
    if w == screen.width() && h == screen.height() {
        return None;
    }
    *screen = Screen::new(w, h);
    Some((w, h))
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
    use crossterm::event::KeyEventState;
    use std::sync::Arc;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: crossterm::event::KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn digits_select_tabs() {
        assert_eq!(
            Key::from_key_event(key(KeyCode::Char('1'), KeyModifiers::NONE)),
            Key::Tab(Tab::Mission)
        );
        assert_eq!(
            Key::from_key_event(key(KeyCode::Char('4'), KeyModifiers::NONE)),
            Key::Tab(Tab::Log)
        );
        assert_eq!(
            Key::from_key_event(key(KeyCode::Char('9'), KeyModifiers::NONE)),
            Key::Ignore
        );
    }

    #[test]
    fn tab_and_shift_tab_cycle() {
        assert_eq!(
            Key::from_key_event(key(KeyCode::Tab, KeyModifiers::NONE)),
            Key::NextTab
        );
        assert_eq!(
            Key::from_key_event(key(KeyCode::Tab, KeyModifiers::SHIFT)),
            Key::PreviousTab
        );
        assert_eq!(
            Key::from_key_event(key(KeyCode::BackTab, KeyModifiers::NONE)),
            Key::PreviousTab
        );
    }

    #[test]
    fn arrows_and_vi_keys_move_the_selection() {
        for code in [KeyCode::Up, KeyCode::Char('k')] {
            assert_eq!(Key::from_key_event(key(code, KeyModifiers::NONE)), Key::Up);
        }
        for code in [KeyCode::Down, KeyCode::Char('j')] {
            assert_eq!(
                Key::from_key_event(key(code, KeyModifiers::NONE)),
                Key::Down
            );
        }
    }

    #[test]
    fn action_keys_are_distinct_from_typed_text() {
        // 'a' and 's' are commands, and the composer path is what makes them
        // text; navigation must never double as input.
        assert_eq!(
            Key::from_key_event(key(KeyCode::Char('a'), KeyModifiers::NONE)),
            Key::Answer
        );
        assert_eq!(
            Key::from_key_event(key(KeyCode::Char('s'), KeyModifiers::NONE)),
            Key::Say
        );
        assert_eq!(
            Key::from_key_event(key(KeyCode::Char('r'), KeyModifiers::NONE)),
            Key::Retry
        );
        assert_eq!(
            Key::from_key_event(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Key::Stop
        );
    }

    #[test]
    fn ctrl_c_closes_from_anywhere() {
        assert_eq!(
            Key::from_key_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Key::Close
        );
    }

    #[test]
    fn enter_submits_and_escape_cancels() {
        assert_eq!(
            Key::from_key_event(key(KeyCode::Enter, KeyModifiers::NONE)),
            Key::Submit
        );
        assert_eq!(
            Key::from_key_event(key(KeyCode::Esc, KeyModifiers::NONE)),
            Key::Cancel
        );
    }

    #[test]
    fn each_key_reaches_the_hook_exactly_once() {
        // Regression: Retry and Stop also called the hook inside their match
        // arm, so a single press stopped a ticket and then reported the second
        // attempt as "already ended".
        use crate::workspace::{AgentLine, AgentState, Workspace};
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counting(Arc<AtomicUsize>);
        impl AppHooks for Counting {
            fn snapshot(&self) -> Workspace {
                Workspace {
                    agents: vec![AgentLine {
                        id: "tk-1".to_string(),
                        retryable: true,
                        name: "row".to_string(),
                        state: AgentState::Waits,
                        activity: String::new(),
                        elapsed: String::new(),
                    }],
                    ..Workspace::default()
                }
            }
            fn on_key(&mut self, _: Key, _: &mut Workspace) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        for key in [Key::Stop, Key::Retry, Key::Down, Key::NextTab] {
            let counter = Arc::new(AtomicUsize::new(0));
            let mut hooks = Counting(Arc::clone(&counter));
            let mut workspace = hooks.snapshot();
            apply_key(key, &mut workspace, &mut hooks);
            assert_eq!(
                counter.load(Ordering::SeqCst),
                1,
                "{key:?} dispatched more than once"
            );
        }
    }

    #[test]
    fn typed_char_ignores_modified_keys() {
        let plain = Event::Key(key(KeyCode::Char('m'), KeyModifiers::NONE));
        assert_eq!(Key::typed_char(&plain), Some('m'));
        let ctrl = Event::Key(key(KeyCode::Char('m'), KeyModifiers::CONTROL));
        assert_eq!(Key::typed_char(&ctrl), None);
        let enter = Event::Key(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(Key::typed_char(&enter), None);
    }
}
