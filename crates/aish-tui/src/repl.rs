//! A minimal but complete REPL over the dual-channel renderer.
//!
//! The REPL is deliberately small: it owns an [`Editor`], a [`Renderer`] and a
//! byte-level [`InputParser`], and it knows only four things about the product
//! behind it — old history, the prompt, completion sources, and a submit
//! callback. Everything the user sees is one of the two renderer channels:
//!
//! * accepted lines (and their command output) are **history** — written once,
//!   never rewritten;
//! * the prompt, the completion menu and the ghost suggestion are the
//!   **viewport** — re-supplied in full every frame and diffed by the renderer.
//!
//! # Product output
//!
//! Anything the product prints while the REPL owns the screen (command output,
//! log records, an LLM token stream) must not reach the terminal directly: it
//! would land inside a frame and desync the renderer's row memo. Producers
//! write to a [`crate::output::OutputSink`] instead, and
//! [`ReplConfig::sink`] names the one this REPL drains — a
//! [`QueueSink`] whose rows are committed as history immediately before every
//! paint.
//!
//! # Testability
//!
//! [`Repl`] is generic over `Read`/`Write`, so a unit test drives it with
//! `Cursor<Vec<u8>>` keystroke bytes and asserts on the exact escape sequences
//! that reached the sink. Nothing here reads `std::io::stdin` directly except
//! [`run_standalone`], which is the only entry point that also owns raw mode.
//!
//! # Supervision
//!
//! Two optional integrations sit on top of the line editor and are both
//! opt-in:
//!
//! * [`ReplConfig::hud`] — one status row appended below the editor and the
//!   completion menu, re-supplied (and re-ticked) every frame;
//! * [`ReplConfig::panel`] — a full-screen supervision panel opened with
//!   Ctrl+S (or by returning [`ReplAction::OpenPanel`] from a submit
//!   callback), which replaces the editor until it is dismissed.
//!
//! With neither configured, nothing in this module changes what reaches the
//! terminal: no extra row, no extra chord, no extra paint.

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use aish_supervisor::SupervisorSnapshot;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::caps::detect_caps;
use crate::completion::{
    common_prefix, token_range, Candidate, CompletionSource, WordListSource, MAX_CANDIDATES,
};
use crate::component::{render_lines_to_viewport, InputHandled, Interactive};
use crate::editor::Editor;
use crate::frame::{Frame, HistoryBatch};
use crate::highlight::{DIM, RESET};
use crate::hud::HudState;
use crate::input::{InputEvent, InputParser, Key, ESCAPE_INACTIVITY};
use crate::output::{OutputSink, QueueSink};
use crate::panel::{PanelAction, SupervisorPanel};
use crate::render::{prepare_line, visible_width};
use crate::renderer::{Renderer, TerminalCaps};

/// Completion menu rows the viewport may show at once.
pub const MENU_ROWS: usize = 8;

/// Maximum output rows one command may commit (a `yes` run must not fill the
/// scrollback with a million rows in one batch).
const MAX_OUTPUT_ROWS: usize = 200;

/// Snapshot provider for the HUD row (called once per frame).
pub type HudSource = Arc<dyn Fn() -> Option<SupervisorSnapshot> + Send + Sync>;

/// Shared handle the shell side uses to open (and act on) the supervision
/// panel.
///
/// The REPL never performs the action itself: [`PanelHandle::on_action`]
/// receives whatever the user picked and the product decides what it means
/// (`StopTask` needs the task registry, `AttachSession` the PTY daemon).
pub struct PanelHandle {
    /// Snapshot provider, called every frame the panel is open.
    pub snapshot: Arc<dyn Fn() -> SupervisorSnapshot + Send + Sync>,
    /// Called with the action the user picked; the shell performs it.
    pub on_action: Arc<dyn Fn(PanelAction) + Send + Sync>,
}

impl fmt::Debug for PanelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Both fields are transports, not configuration worth reading back.
        f.debug_struct("PanelHandle").finish_non_exhaustive()
    }
}

/// Prompt provider: called once per frame to supply the prompt string.
///
/// A product whose prompt carries live state (working directory, git branch,
/// exit code, background-task badge) supplies one of these instead of a
/// [`ReplConfig::prompt`] constant, so the prompt is re-rendered on every
/// paint without the REPL knowing anything about that state.
pub type PromptSource = Arc<dyn Fn() -> String + Send + Sync>;

/// How the REPL is configured.
#[derive(Clone)]
pub struct ReplConfig {
    /// Prompt drawn before the first editor row.
    ///
    /// Ignored while [`ReplConfig::prompt_provider`] is configured.
    pub prompt: String,
    /// Dynamic prompt, taking precedence over [`ReplConfig::prompt`].
    ///
    /// `None` (the default) keeps the prompt a constant, so the REPL paints
    /// exactly what it painted before dynamic prompts existed.
    pub prompt_provider: Option<PromptSource>,
    /// Where accepted lines are appended, if anywhere.
    pub history_path: Option<PathBuf>,
    /// Compute a ghost completion from the sources while typing.
    pub auto_suggest: bool,
    /// Sink whose queued output is committed as history before each paint.
    ///
    /// `None` (the default) keeps the REPL a pure input component: it prints
    /// nothing the product did not submit and drains nothing. A
    /// [`QueueSink`] is drained; any other sink writes on its own, so the REPL
    /// leaves it alone.
    pub sink: Option<Arc<dyn OutputSink>>,
    /// Snapshot provider for the status row, called once per frame.
    ///
    /// `None` (the default) and a provider returning `None` both hide the row,
    /// so the REPL paints exactly what it painted before the HUD existed.
    pub hud: Option<HudSource>,
    /// Supervision panel, opened by Ctrl+S and dismissed by Esc or `q`.
    ///
    /// `None` (the default) makes Ctrl+S a no-op — the chord is not consumed
    /// and reaches the editor as it always did.
    pub panel: Option<Arc<PanelHandle>>,
}

impl ReplConfig {
    /// Single-line prompt with auto-suggestion on, no persisted history and no
    /// product output.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            prompt_provider: None,
            history_path: None,
            auto_suggest: true,
            sink: None,
            hud: None,
            panel: None,
        }
    }
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self::new("❯ ")
    }
}

impl fmt::Debug for ReplConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `Arc<dyn OutputSink>` has no `Debug`, and a sink is a transport, not
        // configuration worth reading back.
        f.debug_struct("ReplConfig")
            .field("prompt", &self.prompt)
            .field(
                "prompt_provider",
                &self.prompt_provider.as_ref().map(|_| "<provider>"),
            )
            .field("history_path", &self.history_path)
            .field("auto_suggest", &self.auto_suggest)
            .field("sink", &self.sink.as_ref().map(|_| "<sink>"))
            .field("hud", &self.hud.as_ref().map(|_| "<hud>"))
            .field("panel", &self.panel)
            .finish()
    }
}

/// What the product wants the REPL to do after a line was submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplAction {
    /// Keep running with an empty editor.
    Continue,
    /// Leave the REPL (EOF is also an exit).
    Exit,
    /// Commit `text` as further history rows (one per physical line, blank
    /// lines skipped). Command output goes here; it never becomes recallable
    /// input.
    AppendHistory(String),
    /// Clear the live screen: the separator row is committed, the renderer
    /// forgets its memo and repaints everything.
    Clear,
    /// Open the supervision panel (Ctrl+S).
    ///
    /// Ignored unless [`ReplConfig::panel`] is configured.
    OpenPanel,
}

/// Interactive line reader over the dual-channel renderer.
pub struct Repl<W: Write, R: Read> {
    renderer: Renderer<W>,
    reader: R,
    editor: Editor,
    config: ReplConfig,
    sources: Vec<Box<dyn CompletionSource>>,
    parser: InputParser,
    /// Monotonic source of [`HistoryBatch`] ids.
    history_id: u64,
    /// Rows finished since the last frame, as raw text: they are prepared at
    /// the current width when flushed, so a resize between the commit and the
    /// paint still produces rows the terminal can address.
    pending_rows: Vec<String>,
    /// Re-read the terminal geometry on every frame (only when the REPL was
    /// built for the real terminal, never in tests).
    auto_resize: bool,
    /// Candidates shown under the prompt until the next keystroke.
    menu: Vec<Candidate>,
    /// First Ctrl+C cleared the buffer; the next one exits.
    ctrl_c_armed: bool,
    /// Status row state; only painted while the configured provider returns a
    /// snapshot.
    hud: HudState,
    /// Supervision panel, created on first open (it keeps the selected tab and
    /// row across opens).
    panel: SupervisorPanel,
    /// Whether the panel currently owns the screen.
    panel_open: bool,
    /// Whether [`Repl::finish`] already wrote the terminal restore sequence.
    restored: bool,
}

impl<W: Write, R: Read> Repl<W, R> {
    /// Build a REPL for the geometry of the current terminal.
    pub fn new(
        writer: W,
        reader: R,
        config: ReplConfig,
        sources: Vec<Box<dyn CompletionSource>>,
    ) -> Self {
        let mut repl = Self::with_caps(writer, reader, config, sources, detect_caps());
        repl.auto_resize = true;
        repl
    }

    /// Build a REPL for an explicit geometry (tests pin it).
    pub fn with_caps(
        writer: W,
        reader: R,
        config: ReplConfig,
        sources: Vec<Box<dyn CompletionSource>>,
        caps: TerminalCaps,
    ) -> Self {
        let mut editor = Editor::new(config.prompt.clone());
        if let Some(path) = config.history_path.as_deref() {
            editor.set_history(load_history(path));
        }
        Self {
            renderer: Renderer::new(writer, caps),
            reader,
            editor,
            config,
            sources,
            parser: InputParser::new(),
            history_id: 0,
            pending_rows: Vec::new(),
            auto_resize: false,
            menu: Vec::new(),
            ctrl_c_armed: false,
            hud: HudState::new(),
            panel: SupervisorPanel::new(),
            panel_open: false,
            restored: false,
        }
    }

    /// Whether the reader has bytes ready within [`ESCAPE_INACTIVITY`].
    ///
    /// Used to decide whether a pending lone ESC heads a longer sequence (a
    /// real key press) or is the Escape key itself. `Read` has no readiness
    /// API, so this polls the file descriptor when the reader is a real stdin;
    /// a test reader (a `Cursor`) always reports ready, which keeps the tests
    /// deterministic.
    fn reader_ready(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = std::io::stdin().as_raw_fd();
            let mut poll_fd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let timeout = ESCAPE_INACTIVITY.as_millis() as libc::c_int;
            // `poll` may be interrupted; a negative return also means "not
            // ready", which settles the ESC rather than blocking.
            unsafe { libc::poll(&mut poll_fd, 1, timeout) > 0 }
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    /// Adopt the terminal's current geometry before painting.
    ///
    /// A resize invalidates the renderer's frame memo and changes the width
    /// every row is fitted to, so nothing prepared before it can be reused.
    fn sync_geometry(&mut self) {
        if !self.auto_resize {
            return;
        }
        let Ok((width, height)) = crossterm::terminal::size() else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        let caps = self.renderer.caps();
        if width != caps.width || height != caps.height {
            self.renderer.resize(width, height);
        }
    }

    /// The editor, e.g. to inspect the pending buffer after [`Repl::run`].
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// Mutable editor access, e.g. to enable multi-line mode before
    /// [`Repl::run`] (where Enter inserts a newline and Ctrl+D submits).
    pub fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    /// Candidates produced by the most recent Tab.
    ///
    /// The field is cleared by the next keystroke (the menu is dismissed) but
    /// kept across exit, so a caller can inspect what the last completion
    /// offered.
    pub fn menu(&self) -> &[Candidate] {
        &self.menu
    }

    /// The renderer, e.g. for committed-row diagnostics.
    pub fn renderer(&self) -> &Renderer<W> {
        &self.renderer
    }

    /// Recover the sink and the source (tests assert on the captured bytes).
    pub fn into_parts(self) -> (W, R) {
        (self.renderer.into_inner(), self.reader)
    }

    /// Read, dispatch and repaint until EOF or a product exit.
    ///
    /// `on_submit` is called once per accepted line (including empty lines) and
    /// receives the text exactly as the editor held it.
    pub fn run<F>(&mut self, mut on_submit: F) -> io::Result<()>
    where
        F: FnMut(&str) -> ReplAction,
    {
        let mut chunk = [0u8; 4096];
        // Events decoded from the last read that neither the panel nor the
        // editor has seen yet. Opening the panel must not drop the rest of the
        // chunk: the keys that follow Ctrl+S (Tab, the Enter that picks a task)
        // belong to the panel, and whatever follows the dismissal belongs to
        // the editor again.
        let mut pending: VecDeque<InputEvent> = VecDeque::new();
        loop {
            if self.panel_open {
                self.draw_panel()?;
            } else {
                self.draw()?;
            }

            if pending.is_empty() {
                // A lone ESC may head a longer sequence (arrow keys, Alt+char),
                // so it must not be emitted immediately — but it also must not
                // wait forever, or the next keystroke typed 100 ms later would
                // be swallowed as `Alt(ch)`. Poll with the escape timeout and
                // settle the pending bytes when it expires.
                if !self.parser.pending().is_empty() && !self.reader_ready() {
                    pending.extend(self.parser.flush());
                    if !pending.is_empty() {
                        continue;
                    }
                }
                let read = match self.reader.read(&mut chunk) {
                    Ok(0) => {
                        // EOF: settle a trailing ambiguous sequence (a bare
                        // Escape) before leaving so no half-parsed key is
                        // dropped silently.
                        pending.extend(self.parser.flush());
                        if pending.is_empty() {
                            break;
                        }
                        continue;
                    }
                    Ok(n) => n,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                    Err(err) => return Err(err),
                };
                pending.extend(self.parser.feed(&chunk[..read]));
            }

            let mut exit = false;
            while let Some(event) = pending.pop_front() {
                if self.panel_open {
                    if self.dispatch_panel(&event) {
                        self.panel_open = false;
                    }
                    // The panel is the only view while it is open, so every
                    // keystroke repaints it (a tab switch must show its rows
                    // immediately); dismissing it repaints the editor instead.
                    if self.panel_open {
                        self.draw_panel()?;
                    } else {
                        self.draw()?;
                    }
                    continue;
                }
                // Per-event, not per-chunk: one read may carry several keys and
                // Right must see the ghost the preceding keys produced.
                if self.dispatch(&event, &mut on_submit)? {
                    exit = true;
                    break;
                }
                self.refresh_ghost();
                // A submitted line runs its command inline, which may queue
                // product output through the sink. Commit it now instead of
                // waiting for the next read: a piped batch of commands would
                // otherwise execute every one of them before the first
                // repaint, leaving their outputs below all the prompts.
                if self.sink_pending() {
                    self.draw()?;
                }
            }
            if exit {
                break;
            }
        }
        self.finish()
    }

    /// Re-read the prompt from the configured provider, if any.
    ///
    /// Called once at the top of every paint: the prompt reflects live shell
    /// state (working directory, exit code, background tasks), so it must be
    /// re-rendered before the frame that shows it. A constant prompt is left
    /// untouched.
    fn refresh_prompt(&mut self) {
        // Disjoint field borrows: the provider is `Fn + Send + Sync`, so it
        // needs no clone to be called while the editor is mutably borrowed.
        let Self { config, editor, .. } = self;
        if let Some(provider) = config.prompt_provider.as_ref() {
            editor.set_prompt(provider());
        }
    }

    /// Paint one frame: sink output, pending history rows, the editor and menu.
    fn draw(&mut self) -> io::Result<()> {
        self.sync_geometry();
        self.refresh_prompt();
        // Product output lands in the history channel *before* the viewport is
        // placed, so a row produced between two frames can never be written in
        // the middle of one.
        self.drain_sink();
        let width = self.renderer.caps().width;
        let mut rows = self.editor.render(width);
        rows.extend(self.menu_rows());
        // The status row is the viewport's last row: below the editor (so the
        // input line keeps its position) and above nothing, because history is
        // committed ahead of the whole viewport.
        if let Some(row) = self.hud_row(width) {
            rows.push(row);
        }
        let viewport = render_lines_to_viewport(rows, width);
        let history = self.take_history(width);
        self.renderer.draw(Frame { history, viewport })
    }

    /// The HUD row for this frame, or `None` when there is nothing to show.
    ///
    /// The provider is called exactly once per frame (it may query process
    /// state). The row is painted only while the supervisor is busy, while a
    /// left segment is set, or while context usage is non-zero: an idle shell
    /// with nothing configured must not grow a permanent `0%` bar.
    fn hud_row(&mut self, width: u16) -> Option<String> {
        let snapshot = (self.config.hud.as_ref()?)()?;
        self.hud.set_snapshot(&snapshot);
        if snapshot.is_busy() {
            self.hud.tick();
        }
        let visible = snapshot.is_busy() || !self.hud.left.is_empty() || self.hud.ctx_percent > 0;
        visible.then(|| self.hud.render(width))
    }

    /// Feed one event to the open panel; `true` when it asked to close.
    ///
    /// The panel is modal: it sees every event while it is open and the editor
    /// sees none of them. `Submit` hands the picked action to the product
    /// *before* reporting the close, so the product observes the action exactly
    /// once per Enter. A dismissal is not an action: `Close` is dropped.
    fn dispatch_panel(&mut self, event: &InputEvent) -> bool {
        // Defensive: without a handle there is no panel to be open.
        let Some(handle) = self.config.panel.clone() else {
            return true;
        };
        // A whole burst of keys can arrive in one read, so the panel's lists are
        // refreshed per event: the Tab and the Enter that follows it must see
        // the rows the user is looking at, not the ones from the previous frame.
        self.panel.set_snapshot(&(handle.snapshot)());
        let Some(key) = panel_key(event) else {
            return false;
        };
        match self.panel.handle_input(Event::Key(key)) {
            InputHandled::Submit => {
                if let Some(action) = self.panel.take_action() {
                    (handle.on_action)(action);
                }
                true
            }
            InputHandled::Cancel => {
                let _ = self.panel.take_action();
                true
            }
            _ => false,
        }
    }

    /// Paint the panel as the whole viewport (history still commits above it).
    fn draw_panel(&mut self) -> io::Result<()> {
        self.sync_geometry();
        self.refresh_prompt();
        self.drain_sink();
        // The panel is the only view while it is open, so it re-reads the
        // supervisor every frame: a task that finishes while the panel is up
        // must update its row without waiting for a keystroke.
        if let Some(handle) = self.config.panel.as_ref() {
            self.panel.set_snapshot(&(handle.snapshot)());
        }
        let caps = self.renderer.caps();
        let rows = self.panel.render(caps.width, caps.height);
        let viewport = render_lines_to_viewport(rows, caps.width);
        let history = self.take_history(caps.width);
        self.renderer.draw(Frame { history, viewport })
    }

    /// Whether the configured sink queued rows that are not on screen yet.
    ///
    /// Used to decide whether a keystroke must be followed by a repaint: a
    /// submitted command runs inline and may queue its output, and that output
    /// must not wait for the next read, or a piped batch would print every
    /// prompt before the first command's output.
    fn sink_pending(&self) -> bool {
        self.config
            .sink
            .as_deref()
            .and_then(|sink| sink.as_any().downcast_ref::<QueueSink>())
            .is_some_and(|queue| queue.has_pending())
    }

    /// Commit every row the configured sink queued since the last frame.
    ///
    /// Only a [`QueueSink`] queues: any other sink writes at the call site, so
    /// there is nothing to move into history. Rows are committed in order and
    /// verbatim — they are already finished and must not be re-prefixed with
    /// the prompt the way a submitted line is.
    fn drain_sink(&mut self) {
        let Some(sink) = self.config.sink.as_deref() else {
            return;
        };
        let Some(queue) = sink.as_any().downcast_ref::<QueueSink>() else {
            return;
        };
        for row in queue.drain() {
            self.commit_plain(&row);
        }
    }

    /// Turn the queued raw rows into the next history batch, if any.
    fn take_history(&mut self, width: u16) -> Option<HistoryBatch> {
        if self.pending_rows.is_empty() {
            return None;
        }
        self.history_id += 1;
        let rows = std::mem::take(&mut self.pending_rows)
            .iter()
            .map(|row| prepare_line(row, width))
            .collect();
        Some(HistoryBatch::new(self.history_id, rows))
    }

    /// Final paint: flush any rows still queued, then clear the live band so no
    /// half-edited prompt is left behind.
    ///
    /// The trailing restore sequence (default rendition, autowrap, cursor, and
    /// the newline that puts the parent prompt at column 1) is written here,
    /// once, through the frame sink. [`TerminalGuard`] then only has to undo
    /// raw mode: writing the sequence in both places emitted it twice, which
    /// shows up as duplicate `^[[0m^[[?7h^[[?25h` runs before the shell exits.
    fn finish(&mut self) -> io::Result<()> {
        self.drain_sink();
        let width = self.renderer.caps().width;
        let history = self.take_history(width);
        self.renderer.draw(Frame {
            history,
            viewport: Vec::new(),
        })?;
        // `\r\n` (not `\n`): the tty is still raw, so a bare newline would
        // leave the cursor in the middle of a line.
        self.renderer
            .write_trailing(format!("{RESTORE_SEQUENCE}\r\n").as_bytes())?;
        self.restored = true;
        Ok(())
    }

    /// Whether [`Repl::finish`] already wrote the terminal restore sequence.
    ///
    /// The entry points hand this to their [`TerminalGuard`] so it does not
    /// repeat the sequence on a normal exit; a panic (which unwinds past
    /// `finish`) leaves it `false` and the guard emits it exactly once.
    pub fn restored(&self) -> bool {
        self.restored
    }

    /// Handle one input unit; `Ok(true)` means the REPL should exit.
    fn dispatch<F>(&mut self, event: &InputEvent, on_submit: &mut F) -> io::Result<bool>
    where
        F: FnMut(&str) -> ReplAction,
    {
        // Any keystroke dismisses the completion menu; Tab rebuilds it below.
        self.menu.clear();

        match event {
            InputEvent::Key(Key::Ctrl('C')) => {
                if self.ctrl_c_armed {
                    return Ok(true);
                }
                self.ctrl_c_armed = true;
                self.editor.clear();
                return Ok(false);
            }
            InputEvent::Key(Key::Ctrl('D')) => {
                self.ctrl_c_armed = false;
                if self.editor.buffer().is_empty() {
                    return Ok(true);
                }
                let line = self.editor.take_buffer();
                return self.submit(&line, on_submit);
            }
            InputEvent::Key(Key::Ctrl('L')) => {
                self.ctrl_c_armed = false;
                self.clear_screen();
                return Ok(false);
            }
            InputEvent::Key(Key::Tab) => {
                self.ctrl_c_armed = false;
                self.complete();
                return Ok(false);
            }
            InputEvent::Key(Key::Ctrl('S')) if self.config.panel.is_some() => {
                // Only a configured panel claims the chord: without one it is
                // not consumed here and reaches the editor as it always did.
                self.ctrl_c_armed = false;
                self.menu.clear();
                self.panel_open = true;
                return Ok(false);
            }
            _ => {}
        }

        self.ctrl_c_armed = false;
        // Enter (submit) plus every editing binding live in the editor, so the
        // multi-line chords cannot drift between the two layers.
        if let Some(line) = self.editor.handle_event(event) {
            return self.submit(&line, on_submit);
        }
        Ok(false)
    }

    /// Commit an accepted line, then let the product react to it.
    fn submit<F>(&mut self, line: &str, on_submit: &mut F) -> io::Result<bool>
    where
        F: FnMut(&str) -> ReplAction,
    {
        self.menu.clear();
        self.commit_line(line);
        self.remember(line);

        match on_submit(line) {
            ReplAction::Continue => Ok(false),
            ReplAction::Exit => Ok(true),
            ReplAction::Clear => {
                self.clear_screen();
                Ok(false)
            }
            ReplAction::AppendHistory(text) => {
                // Output joins the *render* history only. Putting it in the
                // editor history would make Up recall a command's output as if
                // the user had typed it.
                for row in text.split('\n') {
                    if row.trim().is_empty() {
                        continue;
                    }
                    self.commit_plain(row);
                }
                Ok(false)
            }
            ReplAction::OpenPanel => {
                // A product-requested panel open (an external front end may
                // reserve a different chord). Ignored without a configured
                // panel, exactly like the chord itself.
                self.panel_open = self.config.panel.is_some();
                Ok(false)
            }
        }
    }

    /// Queue the submitted text as history rows, prompt included.
    ///
    /// The prompt comes from the editor, not the config: with a dynamic
    /// provider the config string is a placeholder, and the row must repeat the
    /// prompt that was actually on screen.
    fn commit_line(&mut self, line: &str) {
        let prompt = self.editor.prompt().to_string();
        let indent = " ".repeat(visible_width(&prompt) as usize);
        for (index, logical) in line.split('\n').enumerate() {
            let prefix = if index == 0 {
                prompt.as_str()
            } else {
                indent.as_str()
            };
            self.pending_rows.push(format!("{prefix}{logical}"));
        }
    }

    /// Queue one plain row (command output and other product text).
    fn commit_plain(&mut self, text: &str) {
        self.pending_rows.push(text.to_string());
    }

    /// Ctrl+L and `/clear`: separator row, forgotten memo, full repaint.
    fn clear_screen(&mut self) {
        self.menu.clear();
        self.pending_rows.push(String::new());
        self.renderer.invalidate();
    }

    /// Push an accepted line onto the editor history and the history file.
    fn remember(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        self.editor.push_history(line);
        let Some(path) = self.config.history_path.clone() else {
            return;
        };
        if let Err(err) = append_history(&path, line) {
            tracing::warn!(%err, path = %path.display(), "failed to append REPL history");
        }
    }

    /// Tab: insert the longest common prefix, list the candidates when there
    /// is still a choice to make.
    fn complete(&mut self) {
        let buffer = self.editor.buffer().to_string();
        let cursor = self.editor.cursor();
        let range = token_range(&buffer, cursor);
        let token = buffer[range.clone()].to_string();

        let candidates = self.gather(&buffer, cursor);
        if candidates.is_empty() {
            return;
        }

        if candidates.len() == 1 {
            let only = &candidates[0];
            if only.replacement != token {
                self.replace_token(&token, &only.replacement);
            }
            return;
        }

        let prefix = common_prefix(candidates.iter().map(|c| c.replacement.as_str()));
        if prefix.len() > token.len() && prefix.starts_with(&token) {
            self.replace_token(&token, &prefix);
        }
        self.menu = candidates;
    }

    /// Deduplicated candidates from every source, in source order, capped.
    fn gather(&self, line: &str, cursor: usize) -> Vec<Candidate> {
        let mut candidates: Vec<Candidate> = Vec::new();
        for source in &self.sources {
            for candidate in source.complete(line, cursor) {
                if candidates
                    .iter()
                    .any(|seen| seen.replacement == candidate.replacement)
                {
                    continue;
                }
                candidates.push(candidate);
                if candidates.len() == MAX_CANDIDATES {
                    return candidates;
                }
            }
        }
        candidates
    }

    /// Replace the token ending at the cursor with `replacement`.
    fn replace_token(&mut self, token: &str, replacement: &str) {
        self.editor
            .replace_before_cursor(token.chars().count(), replacement);
    }

    /// Refresh the ghost completion from the sources.
    ///
    /// The ghost is the part of the best candidate beyond the token being
    /// typed, so accepting it (Right / Ctrl+F) appends exactly what was shown.
    fn refresh_ghost(&mut self) {
        if !self.config.auto_suggest {
            self.editor.set_ghost(None);
            return;
        }
        let buffer = self.editor.buffer().to_string();
        let cursor = self.editor.cursor();
        // The open menu already lists every option; a ghost on top of it would
        // duplicate the first candidate.
        if buffer.is_empty() || cursor != buffer.len() || !self.menu.is_empty() {
            self.editor.set_ghost(None);
            return;
        }
        let token = &buffer[token_range(&buffer, cursor)];
        let Some(candidate) = self.gather(&buffer, cursor).into_iter().next() else {
            self.editor.set_ghost(None);
            return;
        };
        let ghost = candidate
            .replacement
            .strip_prefix(token)
            .filter(|suffix| !suffix.is_empty())
            .map(str::to_string);
        self.editor.set_ghost(ghost);
    }

    /// Dim rows listing the open completion menu.
    fn menu_rows(&self) -> Vec<String> {
        let mut rows = Vec::new();
        for candidate in self.menu.iter().take(MENU_ROWS) {
            let mut row = format!("  {DIM}{}{RESET}", candidate.display);
            if let Some(description) = &candidate.description {
                row.push_str(&format!("  {DIM}{description}{RESET}"));
            }
            rows.push(row);
        }
        if self.menu.len() > MENU_ROWS {
            rows.push(format!("  {DIM}… {}{RESET}", self.menu.len() - MENU_ROWS));
        }
        rows
    }
}

/// Translate a parsed [`InputEvent`] into the crossterm event the panel expects.
///
/// The panel is an [`Interactive`] component written against crossterm's
/// `Event`, while the REPL parses its own byte stream; this is the one adapter
/// between the two. Pastes and unrecognised bytes have no crossterm equivalent
/// and are dropped — the panel is a modal screen, so there is nothing for them
/// to mean.
fn panel_key(event: &InputEvent) -> Option<KeyEvent> {
    let code = match event {
        InputEvent::Char(ch) => KeyCode::Char(*ch),
        InputEvent::Key(key) => match key {
            Key::Enter | Key::ShiftEnter | Key::AltEnter => KeyCode::Enter,
            Key::Esc => KeyCode::Esc,
            Key::Backspace => KeyCode::Backspace,
            Key::Tab => KeyCode::Tab,
            Key::BackTab => KeyCode::BackTab,
            Key::Delete => KeyCode::Delete,
            Key::Insert => KeyCode::Insert,
            Key::Home => KeyCode::Home,
            Key::End => KeyCode::End,
            Key::Up | Key::CtrlUp => KeyCode::Up,
            Key::Down | Key::CtrlDown => KeyCode::Down,
            Key::Left | Key::CtrlLeft => KeyCode::Left,
            Key::Right | Key::CtrlRight => KeyCode::Right,
            Key::PageUp => KeyCode::PageUp,
            Key::PageDown => KeyCode::PageDown,
            Key::Ctrl(ch) | Key::Alt(ch) => KeyCode::Char(*ch),
        },
        InputEvent::Paste(_) | InputEvent::Unknown(_) => return None,
    };
    Some(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Read one history entry per line; a missing file is an empty history.
fn load_history(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Append every physical line of `entry` to the history file.
fn append_history(path: &Path, entry: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for line in entry.split('\n').filter(|line| !line.trim().is_empty()) {
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

// ── standalone entry point ─────────────────────────────────────────────────

/// Default history location: `$XDG_DATA_HOME/aish-tui/history.txt`.
pub fn default_history_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("aish-tui").join("history.txt"))
}

/// Reset rendition, re-enable autowrap and show the cursor.
const RESTORE_SEQUENCE: &str = "\x1b[0m\x1b[?7h\x1b[?25h";

/// Owns raw mode for the duration of a standalone REPL.
///
/// The restore sequence is written by [`Repl::finish`] through the frame sink,
/// so it shares that sink's flush order and reaches the terminal exactly once.
/// The guard writes it only when the REPL never got that far — a panic unwinds
/// past `finish` — and always puts the tty back into cooked mode.
pub(crate) struct TerminalGuard {
    /// Whether [`Repl::finish`] already wrote [`RESTORE_SEQUENCE`].
    restored: bool,
}

impl TerminalGuard {
    pub(crate) fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self { restored: false })
    }

    /// Hand the terminal back after [`Repl::run`] returned.
    ///
    /// A completed run already restored it from [`Repl::finish`]; repeating the
    /// sequence here is what put duplicate `^[[0m^[[?7h^[[?25h` runs in the
    /// user's scrollback before the shell exited.
    pub(crate) fn release(mut self, repl_restored: bool) {
        self.restored = repl_restored;
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if !self.restored {
            let mut out = io::stdout();
            // End the line so the parent prompt starts at column 1.
            let _ = out.write_all(format!("{RESTORE_SEQUENCE}\r\n").as_bytes());
            let _ = out.flush();
        }
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Run the demo REPL on the real terminal until `/exit`, Ctrl+D or EOF.
///
/// This is the demo behind the `aish-tui-repl` binary: PATH executables plus
/// `/help`, `/exit` and `/clear` complete; every other line runs under
/// `bash -c` and its output is committed as history rows. The product entry
/// point is [`crate::run_shell_repl`], which drives a real shell instead.
pub fn run_standalone(prompt: &str, history_path: Option<PathBuf>) -> io::Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "the aish-tui REPL needs a terminal on stdin and stdout",
        ));
    }
    let guard = TerminalGuard::enter()?;
    let config = ReplConfig {
        prompt: prompt.to_string(),
        prompt_provider: None,
        history_path,
        auto_suggest: true,
        sink: None,
        hud: None,
        panel: None,
    };
    let sources: Vec<Box<dyn CompletionSource>> = vec![Box::new(WordListSource::new(demo_words()))];
    let mut repl = Repl::new(
        io::BufWriter::with_capacity(8192, io::stdout()),
        io::stdin(),
        config,
        sources,
    );
    let result = repl.run(demo_submit);
    let restored = repl.restored();
    drop(repl);
    guard.release(restored);
    match result {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Maximum number of `PATH` executables offered as completions.
///
/// A real desktop `PATH` holds thousands of entries, more than a completion
/// source is worth scanning per keystroke.
const DEMO_WORD_CAP: usize = 500;

/// Upper bound on directory entries inspected while building the vocabulary.
const SCAN_CAP: usize = 20_000;

/// Completion vocabulary: builtins first, then executables found on `PATH`.
///
/// The cap keeps the *shortest* names: that is where the heavily used commands
/// live (`ls`, `cd`, `git`), and a cutoff by directory order would drop them
/// for whichever packages happened to be read last.
pub(crate) fn demo_words() -> Vec<String> {
    let mut words = vec![
        "/help".to_string(),
        "/exit".to_string(),
        "/clear".to_string(),
    ];
    let Some(path) = std::env::var_os("PATH") else {
        return words;
    };

    let mut found: Vec<String> = Vec::new();
    for dir in std::env::split_paths(&path) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            // Bound the scan: a pathological directory (a package cache, a
            // tensor blob dir) must not stall startup.
            if found.len() >= SCAN_CAP {
                break;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() || !is_executable(&meta) {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if !name.starts_with('.') {
                    found.push(name.to_string());
                }
            }
        }
    }

    found.sort_unstable();
    found.dedup();
    found.sort_by_key(|name| (name.chars().count(), name.clone()));
    words.extend(found.into_iter().take(DEMO_WORD_CAP));
    words
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    true
}

/// Demo product: slash commands, otherwise `bash -c` with committed output.
fn demo_submit(line: &str) -> ReplAction {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return ReplAction::Continue;
    }
    match trimmed {
        "/exit" | "/quit" => return ReplAction::Exit,
        "/clear" => return ReplAction::Clear,
        "/help" => {
            return ReplAction::AppendHistory(
                "\x1b[1maish-tui demo\x1b[0m  —  dual-channel history + viewport\n\
                 \x1b[2m  Enter        submit the line\x1b[0m\n\
                 \x1b[2m  Shift+Enter  insert a newline (Alt+Enter on legacy terminals)\x1b[0m\n\
                 \x1b[2m  Tab          complete from PATH; the menu lists the rest\x1b[0m\n\
                 \x1b[2m  Right/Ctrl+F accept the dim ghost suggestion\x1b[0m\n\
                 \x1b[2m  Ctrl+C       clear the line; twice exits\x1b[0m\n\
                 \x1b[2m  Ctrl+D       submit a multi-line buffer; on an empty line it exits\x1b[0m\n\
                 \x1b[2m  Ctrl+L       clear the live screen\x1b[0m\n\
                 \x1b[2m  /exit        leave the demo\x1b[0m"
                    .to_string(),
            );
        }
        other if other.starts_with('/') => {
            return ReplAction::AppendHistory(format!(
                "\x1b[2munknown command {other}; try /help\x1b[0m"
            ));
        }
        _ => {}
    }

    match Command::new("bash").arg("-c").arg(trimmed).output() {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            let text = text.trim_end();
            if text.is_empty() {
                return ReplAction::AppendHistory(format!(
                    "\x1b[2m(no output, status {})\x1b[0m",
                    output.status
                ));
            }
            let rows: Vec<&str> = text.split('\n').collect();
            let mut committed = rows
                .iter()
                .take(MAX_OUTPUT_ROWS)
                .copied()
                .collect::<Vec<_>>();
            if rows.len() > MAX_OUTPUT_ROWS {
                committed.push("\x1b[2m… output truncated\x1b[0m");
            }
            ReplAction::AppendHistory(committed.join("\n"))
        }
        Err(err) => ReplAction::AppendHistory(format!("\x1b[2mfailed to run bash: {err}\x1b[0m")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aish_supervisor::{SupervisorSnapshot, TaskRow, TaskState};
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    /// Wide, short geometry: viewport rows never scroll in these tests.
    const CAPS: TerminalCaps = TerminalCaps {
        synchronized_output: false,
        width: 80,
        height: 24,
    };

    fn words(list: &[&str]) -> Vec<Box<dyn CompletionSource>> {
        vec![Box::new(WordListSource::new(
            list.iter().map(|word| word.to_string()).collect(),
        ))]
    }

    fn repl(
        input: &[u8],
        sources: Vec<Box<dyn CompletionSource>>,
    ) -> Repl<Vec<u8>, Cursor<Vec<u8>>> {
        Repl::with_caps(
            Vec::new(),
            Cursor::new(input.to_vec()),
            ReplConfig {
                prompt: "> ".to_string(),
                prompt_provider: None,
                history_path: None,
                auto_suggest: false,
                sink: None,
                hud: None,
                panel: None,
            },
            sources,
            CAPS,
        )
    }

    /// Run a REPL over `input`, recording every submitted line.
    fn run(
        repl: &mut Repl<Vec<u8>, Cursor<Vec<u8>>>,
        action: impl FnMut(&str) -> ReplAction,
    ) -> Vec<String> {
        let submitted = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&submitted);
        let mut action = action;
        repl.run(move |line| {
            sink.lock().unwrap().push(line.to_string());
            action(line)
        })
        .expect("repl run");
        let out = submitted.lock().unwrap().clone();
        out
    }

    fn output(repl: Repl<Vec<u8>, Cursor<Vec<u8>>>) -> String {
        let (bytes, _) = repl.into_parts();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn repl_echoes_and_submits_line() {
        let mut repl = repl(b"abc\r", Vec::new());
        let submitted = run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(submitted, ["abc"]);
        // Exactly one row left the live area: the submitted line.
        assert_eq!(repl.renderer().history_rows(), 1);
        // The prompt, the typed text and the committed row all reached the sink.
        let bytes = output(repl);
        assert!(bytes.contains("> abc"), "committed row missing: {bytes:?}");
        assert!(bytes.contains("\x1b[?25l"), "cursor was never hidden");
    }

    #[test]
    fn repl_history_up_recalls_previous() {
        // Two submits, then Up restores the second line into the buffer.
        let mut repl = repl(b"one\rtwo\r\x1b[A", Vec::new());
        run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(repl.editor().buffer(), "two");
        let bytes = output(repl);
        assert!(bytes.contains("one") && bytes.contains("two"));
    }

    #[test]
    fn repl_ctrl_c_clears_then_exits() {
        // "xx", Ctrl+C (clears), "yy", Ctrl+C (clears), Ctrl+C (exits).
        let mut repl = repl(b"xx\x03yy\x03\x03", Vec::new());
        let submitted = run(&mut repl, |_| ReplAction::Continue);
        assert!(
            submitted.is_empty(),
            "Ctrl+C must not submit: {submitted:?}"
        );
        assert!(repl.editor().buffer().is_empty());
    }

    #[test]
    fn repl_tab_completion_fills_common_prefix() {
        let mut repl = repl(b"gi\t", words(&["git-add", "git-log", "ls"]));
        run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(repl.editor().buffer(), "git-");
        // Three candidates match the short prefix, so the menu is populated.
        assert_eq!(repl.menu().len(), 2, "menu: {:?}", repl.menu());
        let bytes = output(repl);
        assert!(bytes.contains("\x1b[2m"), "menu rows are not dimmed");
    }

    #[test]
    fn repl_multiline_shift_enter_inserts_newline() {
        // kitty/CSI-u Shift+Enter, then submit with Enter.
        let mut repl = repl(b"a\x1b[13;2ub\r", Vec::new());
        let submitted = run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(submitted, ["a\nb"]);
        assert!(repl.editor().buffer().is_empty(), "buffer survived submit");
    }

    #[test]
    fn repl_multiline_mode_submits_with_ctrl_d() {
        // In multi-line mode Enter inserts a newline; Ctrl+D submits the buffer.
        let mut repl = repl(b"first\rsecond\r\x04", Vec::new());
        repl.editor_mut().set_multiline_mode(true);
        let submitted = run(&mut repl, |_| ReplAction::Continue);
        // Each Enter keeps the newline it inserts, exactly as readline does.
        assert_eq!(submitted, ["first\nsecond\n"]);
        // Continuation rows are indented under the prompt, never re-prefixed.
        let bytes = output(repl);
        assert!(bytes.contains("> first"), "{bytes:?}");
        assert!(bytes.contains("  second"), "{bytes:?}");
    }

    #[test]
    fn repl_exit_restores_terminal_state() {
        let mut repl = repl(b"", Vec::new());
        run(&mut repl, |_| ReplAction::Continue);
        let bytes = output(repl);
        // Every paint closes with autowrap on and the cursor visible.
        assert!(
            bytes.contains("\x1b[?7h"),
            "autowrap not restored: {bytes:?}"
        );
        assert!(
            bytes.contains("\x1b[?25h"),
            "cursor not restored: {bytes:?}"
        );
    }

    #[test]
    fn repl_append_history_action_commits_row() {
        let mut repl = repl(b"run\r\x04", Vec::new());
        let submitted = run(&mut repl, |line| {
            if line == "run" {
                ReplAction::AppendHistory("out-1\nout-2".to_string())
            } else {
                ReplAction::Continue
            }
        });
        assert_eq!(submitted, ["run"]);
        // Three history rows were acked: the submitted line plus two outputs.
        assert_eq!(repl.renderer().history_rows(), 3);
        // Output is not recalled as input: only the accepted line is input
        // history, and the committed bytes carry both output rows.
        assert_eq!(repl.editor().history(), ["run"]);
        let bytes = output(repl);
        assert!(
            bytes.contains("out-1") && bytes.contains("out-2"),
            "{bytes:?}"
        );
    }

    #[test]
    fn repl_exit_action_stops_the_loop() {
        let mut repl = repl(b"quit\rnever\r", Vec::new());
        let submitted = run(&mut repl, |line| {
            if line == "quit" {
                ReplAction::Exit
            } else {
                ReplAction::Continue
            }
        });
        assert_eq!(submitted, ["quit"], "loop kept reading after Exit");
    }

    #[test]
    fn repl_ctrl_d_exits_on_empty_line_but_submits_text() {
        let mut repl = repl(b"text\x04\x04", Vec::new());
        let submitted = run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(submitted, ["text"]);
    }

    #[test]
    fn repl_ghost_suggests_and_right_accepts() {
        let mut repl = Repl::with_caps(
            Vec::new(),
            Cursor::new(b"gi\x1b[C".to_vec()),
            ReplConfig {
                prompt: "> ".to_string(),
                prompt_provider: None,
                history_path: None,
                auto_suggest: true,
                sink: None,
                hud: None,
                panel: None,
            },
            words(&["git-add", "git-log"]),
            CAPS,
        );
        run(&mut repl, |_| ReplAction::Continue);
        // Ghost "t-add" (the first candidate beyond the token) was shown and
        // Right walked over it as if it had been typed.
        assert_eq!(repl.editor().buffer(), "git-add");
    }

    #[test]
    fn repl_history_is_loaded_and_appended() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/history.txt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "previous\n").unwrap();

        let mut repl = Repl::with_caps(
            Vec::new(),
            Cursor::new(b"fresh\r\x1b[A".to_vec()),
            ReplConfig {
                prompt: "> ".to_string(),
                prompt_provider: None,
                history_path: Some(path.clone()),
                auto_suggest: false,
                sink: None,
                hud: None,
                panel: None,
            },
            Vec::new(),
            CAPS,
        );
        run(&mut repl, |_| ReplAction::Continue);
        // Up after submitting "fresh" recalls it; the file holds both lines.
        assert_eq!(repl.editor().buffer(), "fresh");
        let stored = std::fs::read_to_string(&path).unwrap();
        assert_eq!(stored, "previous\nfresh\n");
    }

    #[test]
    fn repl_ctrl_l_invalidates_and_separates() {
        let mut repl = repl(b"\x0c", Vec::new());
        run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(repl.renderer().history_rows(), 1, "separator row missing");
        // An invalidated renderer repaints the whole band from a fresh anchor.
        let bytes = output(repl);
        assert!(bytes.contains("\x1b[24;1H"), "no repaint anchor: {bytes:?}");
    }

    #[test]
    fn menu_is_capped_at_eight_rows() {
        let mut repl = repl(
            b"c\t",
            words(&[
                "cmd0", "cmd1", "cmd2", "cmd3", "cmd4", "cmd5", "cmd6", "cmd7", "cmd8", "cmd9",
            ]),
        );
        run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(repl.menu().len(), 10);
        let rows = repl.menu_rows();
        assert_eq!(rows.len(), MENU_ROWS + 1, "overflow hint missing");
        assert!(rows.last().unwrap().contains('…'));
    }

    /// REPL whose product output goes to a [`QueueSink`] the test keeps.
    fn repl_with_sink(input: &[u8], sink: &Arc<QueueSink>) -> Repl<Vec<u8>, Cursor<Vec<u8>>> {
        Repl::with_caps(
            Vec::new(),
            Cursor::new(input.to_vec()),
            ReplConfig {
                prompt: "> ".to_string(),
                prompt_provider: None,
                history_path: None,
                auto_suggest: false,
                sink: Some(Arc::clone(sink) as Arc<dyn OutputSink>),
                hud: None,
                panel: None,
            },
            Vec::new(),
            CAPS,
        )
    }

    #[test]
    fn repl_commits_sink_lines_before_drawing() {
        let sink = Arc::new(QueueSink::new());
        // Produced by a "background" thread before the REPL reads a key: the
        // rows must still be committed ahead of the first viewport paint.
        sink.line("from-thread-1");
        sink.line("from-thread-2");

        let mut repl = repl_with_sink(b"", &sink);
        run(&mut repl, |_| ReplAction::Continue);

        // Two queued rows became two history rows, drained by the REPL.
        assert_eq!(repl.renderer().history_rows(), 2);
        assert!(sink.drain().is_empty(), "REPL left rows in the sink");
        // Sink rows are product output: they must not be recalled as input.
        assert!(
            repl.editor().history().is_empty(),
            "sink rows entered history"
        );
        let bytes = output(repl);
        let first = bytes.find("from-thread-1").expect("first row missing");
        let second = bytes.find("from-thread-2").expect("second row missing");
        assert!(first < second, "sink rows out of order: {bytes:?}");
    }

    #[test]
    fn repl_without_sink_commits_nothing() {
        // The default path must stay byte-identical to the pre-sink REPL: no
        // history rows unless the product submits one.
        let mut repl = repl(b"", Vec::new());
        run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(repl.renderer().history_rows(), 0);
    }

    // ── HUD / panel integration ────────────────────────────────────────────

    fn task_row(id: &str, state: TaskState) -> TaskRow {
        TaskRow {
            task_id: id.to_string(),
            command: "sleep 8".to_string(),
            state,
            exit_code: None,
            started_at_ms: 0,
            finished_at_ms: None,
            output_path: String::new(),
        }
    }

    fn snapshot(tasks: Vec<TaskRow>) -> SupervisorSnapshot {
        SupervisorSnapshot {
            tasks,
            ..SupervisorSnapshot::default()
        }
    }

    /// REPL whose HUD row is supplied by `hud` and whose panel is `panel`.
    fn repl_with_supervision(
        input: &[u8],
        hud: Option<HudSource>,
        panel: Option<Arc<PanelHandle>>,
    ) -> Repl<Vec<u8>, Cursor<Vec<u8>>> {
        Repl::with_caps(
            Vec::new(),
            Cursor::new(input.to_vec()),
            ReplConfig {
                prompt: "> ".to_string(),
                prompt_provider: None,
                history_path: None,
                auto_suggest: false,
                sink: None,
                hud,
                panel,
            },
            Vec::new(),
            CAPS,
        )
    }

    /// The editor prompt as painted: `>` plus the reverse-video cursor cell.
    const EDITOR_PROMPT: &str = ">\u{1b}[7m";

    /// HUD source returning a fixed snapshot.
    fn fixed_hud(snap: SupervisorSnapshot) -> HudSource {
        Arc::new(move || Some(snap.clone()))
    }

    /// Panel handle whose actions are recorded, over a fixed snapshot.
    fn recording_panel(
        snap: SupervisorSnapshot,
    ) -> (Arc<PanelHandle>, Arc<Mutex<Vec<PanelAction>>>) {
        let actions = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&actions);
        let handle = Arc::new(PanelHandle {
            snapshot: Arc::new(move || snap.clone()),
            on_action: Arc::new(move |action| recorded.lock().unwrap().push(action)),
        });
        (handle, actions)
    }

    #[test]
    fn hud_row_appears_when_busy() {
        let mut repl = repl_with_supervision(
            b"",
            Some(fixed_hud(snapshot(vec![task_row(
                "t1",
                TaskState::Running,
            )]))),
            None,
        );
        run(&mut repl, |_| ReplAction::Continue);
        let bytes = output(repl);
        // The summary is the supervisor's own wording, echoed verbatim.
        assert!(bytes.contains("1 task"), "HUD row missing: {bytes:?}");
        // Busy paints advance the spinner, so the row carries a braille cell.
        assert!(
            bytes
                .chars()
                .any(|ch| crate::hud::SPINNER_FRAMES.contains(&ch)),
            "spinner never ticked: {bytes:?}"
        );
    }

    #[test]
    fn hud_hidden_when_idle() {
        let busy = repl_with_supervision(
            b"",
            Some(fixed_hud(snapshot(vec![task_row(
                "t1",
                TaskState::Running,
            )]))),
            None,
        );
        let mut busy = busy;
        run(&mut busy, |_| ReplAction::Continue);
        let busy_bytes = output(busy);

        let mut idle = repl_with_supervision(b"", Some(fixed_hud(snapshot(Vec::new()))), None);
        run(&mut idle, |_| ReplAction::Continue);
        let idle_bytes = output(idle);

        assert!(busy_bytes.contains("1 task"));
        // Nothing running: no summary, no spinner, no context bar.
        assert!(!idle_bytes.contains("task"), "idle HUD row: {idle_bytes:?}");
        assert!(
            !idle_bytes
                .chars()
                .any(|ch| crate::hud::SPINNER_FRAMES.contains(&ch)),
            "idle HUD spun: {idle_bytes:?}"
        );
    }

    #[test]
    fn no_hud_config_preserves_bytes() {
        // A provider returning `None` must paint exactly what a REPL with no
        // HUD at all paints: the row is opt-in, never a permanent fixture.
        let mut unset = repl(b"", Vec::new());
        run(&mut unset, |_| ReplAction::Continue);
        let unset_bytes = output(unset);

        let mut absent = repl_with_supervision(b"", Some(Arc::new(|| None)), None);
        run(&mut absent, |_| ReplAction::Continue);
        let absent_bytes = output(absent);

        assert_eq!(absent_bytes, unset_bytes);
    }

    #[test]
    fn ctrl_s_opens_panel() {
        let (panel, _actions) = recording_panel(snapshot(vec![task_row("t1", TaskState::Running)]));
        // Ctrl+S (0x13) opens on the agents tab, Tab switches to tasks, Esc
        // (0x1b) dismisses.
        let mut repl = repl_with_supervision(b"\x13\t\x1b", None, Some(panel));
        run(&mut repl, |_| ReplAction::Continue);
        let bytes = output(repl);

        // All three tabs are drawn with the panel's own header format, and the
        // running task has a row.
        for tab in ["Agents(", "Tasks(", "Sessions("] {
            assert!(bytes.contains(tab), "tab {tab} missing: {bytes:?}");
        }
        // The selected task has a row, painted with the panel's own marker.
        let row = "▸ sleep 8";
        assert!(bytes.contains(row), "task row missing: {bytes:?}");

        // The panel is full screen: from the first panel paint to the last one
        // the editor prompt (its `>` plus the reverse-video cursor cell) is
        // never painted. The prompt before that window is the initial frame;
        // the one after it is the dismissal restoring the editor.
        let opened = bytes.find("Agents(").expect("panel never painted");
        let closed = bytes.rfind(row).expect("panel row never painted");
        assert!(
            !bytes[opened..closed].contains(EDITOR_PROMPT),
            "editor painted under the panel: {bytes:?}"
        );
    }

    #[test]
    fn panel_esc_closes_without_action() {
        let (panel, actions) = recording_panel(snapshot(vec![task_row("t1", TaskState::Running)]));
        let mut repl = repl_with_supervision(b"\x13\t\x1b", None, Some(panel));
        let submitted = run(&mut repl, |_| ReplAction::Continue);

        assert!(submitted.is_empty(), "panel submitted: {submitted:?}");
        assert!(actions.lock().unwrap().is_empty(), "Esc ran an action");
        // Back to the editor: the prompt is painted again, after the panel.
        let bytes = output(repl);
        let panel = bytes.find("Agents(").expect("panel never painted");
        let editor = bytes
            .rfind(EDITOR_PROMPT)
            .expect("editor prompt never repainted");
        assert!(editor > panel, "editor did not come back: {bytes:?}");
    }

    #[test]
    fn panel_submit_invokes_callback() {
        let (panel, actions) = recording_panel(snapshot(vec![task_row("t1", TaskState::Running)]));
        // Ctrl+S, Tab (agents → tasks), Enter (stop t1), Esc to close the
        // editor afterwards.
        let mut repl = repl_with_supervision(b"\x13\t\r\x1b", None, Some(panel));
        run(&mut repl, |_| ReplAction::Continue);

        assert_eq!(
            *actions.lock().unwrap(),
            vec![PanelAction::StopTask("t1".to_string())],
            "the picked action never reached the product"
        );
    }

    #[test]
    fn panel_closed_by_action_returns_to_editor() {
        let (panel, actions) = recording_panel(snapshot(vec![task_row("t1", TaskState::Running)]));
        // Ctrl+S, Tab, Enter: the submit also closes the panel, so the trailing
        // "x" is typed into the editor and never into a dismissable panel.
        let mut repl = repl_with_supervision(b"\x13\t\rx", None, Some(panel));
        run(&mut repl, |_| ReplAction::Continue);

        assert_eq!(actions.lock().unwrap().len(), 1);
        assert_eq!(repl.editor().buffer(), "x");
    }

    #[test]
    fn ctrl_s_without_panel_config_is_not_consumed() {
        // Ctrl+S alone is not an editor binding; with no panel configured the
        // REPL must not swallow it *and* must not open anything. The trailing
        // Enter proves the loop kept reading normally.
        let mut repl = repl_with_supervision(b"ab\x13\r", None, None);
        let submitted = run(&mut repl, |_| ReplAction::Continue);
        assert_eq!(submitted, ["ab"], "Ctrl+S disturbed the editor");
    }

    #[test]
    fn prompt_provider_supplies_the_painted_prompt() {
        // The provider wins over the constant field on every frame.
        let mut repl = Repl::with_caps(
            Vec::new(),
            Cursor::new(b"ab\r".to_vec()),
            ReplConfig {
                prompt: "ignored> ".to_string(),
                prompt_provider: Some(Arc::new(|| "◆ aish /tmp➜ ".to_string())),
                history_path: None,
                auto_suggest: false,
                sink: None,
                hud: None,
                panel: None,
            },
            Vec::new(),
            CAPS,
        );
        run(&mut repl, |_| ReplAction::Continue);

        let bytes = output(repl);
        assert!(
            bytes.contains("◆ aish /tmp➜ ab"),
            "dynamic prompt never painted: {bytes:?}"
        );
        assert!(
            !bytes.contains("ignored> "),
            "constant prompt leaked: {bytes:?}"
        );
    }

    #[test]
    fn commit_line_repeats_the_prompt_actually_painted() {
        // The committed row must carry the prompt that was on screen, not the
        // (placeholder) config constant, so a submitted line never re-prefixes
        // itself with a stale prompt.
        let mut repl = Repl::with_caps(
            Vec::new(),
            Cursor::new(b"echo x\r".to_vec()),
            ReplConfig {
                prompt: "placeholder> ".to_string(),
                prompt_provider: Some(Arc::new(|| "◆ aish /tmp➜ ".to_string())),
                history_path: None,
                auto_suggest: false,
                sink: None,
                hud: None,
                panel: None,
            },
            Vec::new(),
            CAPS,
        );
        run(&mut repl, |_| ReplAction::Continue);
        let bytes = output(repl);

        assert!(
            bytes.contains("◆ aish /tmp➜ echo x"),
            "committed row lost the prompt: {bytes:?}"
        );
        assert!(
            !bytes.contains("placeholder> "),
            "placeholder prompt reached the terminal: {bytes:?}"
        );
    }

    #[test]
    fn prompt_provider_is_rerendered_for_every_frame() {
        // A prompt whose content changes between frames must show the new value
        // without a keystroke: that is what `cd` and exit-code changes rely on.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let mut repl = Repl::with_caps(
            Vec::new(),
            Cursor::new(b"a\r".to_vec()),
            ReplConfig {
                prompt: String::new(),
                prompt_provider: Some(Arc::new(move || {
                    let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    format!("[{n}] ")
                })),
                history_path: None,
                auto_suggest: false,
                sink: None,
                hud: None,
                panel: None,
            },
            Vec::new(),
            CAPS,
        );
        run(&mut repl, |_| ReplAction::Continue);
        let bytes = output(repl);

        assert!(
            calls.load(std::sync::atomic::Ordering::SeqCst) > 1,
            "provider was not called per frame"
        );
        // The cursor highlight follows the prompt immediately (no separating
        // space at an empty buffer), so match the marker only.
        assert!(bytes.contains("[1]"), "first frame missing: {bytes:?}");
    }

    #[test]
    fn submitted_line_is_painted_once() {
        // The editor's first row *is* the committed row, so the renderer must
        // not rewrite it: doing so printed every submitted line twice, which
        // under a pty (or a reflowing terminal) scrolls a duplicate into
        // scrollback.
        let mut repl = repl(b"echo A\recho B\r", Vec::new());
        run(&mut repl, |_| ReplAction::Continue);
        let bytes = output(repl);

        for line in ["> echo A", "> echo B"] {
            assert_eq!(
                bytes.matches(line).count(),
                1,
                "submitted row was painted more than once: {line:?} in {bytes:?}"
            );
        }
    }

    #[test]
    fn submitted_line_is_painted_once_after_scrolling() {
        // A viewport taller than the screen forces the block to scroll, which
        // is where the duplicate used to land in scrollback.
        let mut repl = Repl::with_caps(
            Vec::new(),
            Cursor::new(b"one\rtwo\rthree\r".to_vec()),
            ReplConfig {
                prompt: "> ".to_string(),
                prompt_provider: None,
                history_path: None,
                auto_suggest: false,
                sink: None,
                hud: None,
                panel: None,
            },
            Vec::new(),
            TerminalCaps {
                synchronized_output: false,
                width: 20,
                height: 2,
            },
        );
        run(&mut repl, |_| ReplAction::Continue);
        let bytes = output(repl);

        for line in ["> one", "> two", "> three"] {
            assert_eq!(
                bytes.matches(line).count(),
                1,
                "scrolled commit duplicated {line:?} in {bytes:?}"
            );
        }
    }

    #[test]
    fn exit_emits_single_restore_sequence() {
        // `finish` writes the restore through the frame sink and the guard
        // skips its own copy, so the sequence must appear exactly once.
        const RESTORE: &str = "\x1b[0m\x1b[?7h\x1b[?25h";
        let mut repl = repl(b"x\r", Vec::new());
        run(&mut repl, |_| ReplAction::Continue);
        let bytes = output(repl);
        assert_eq!(
            bytes.matches(RESTORE).count(),
            1,
            "restore sequence emitted more than once: {bytes:?}"
        );
    }
}
