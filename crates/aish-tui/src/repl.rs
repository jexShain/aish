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

use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::caps::detect_caps;
use crate::completion::{
    common_prefix, token_range, Candidate, CompletionSource, WordListSource, MAX_CANDIDATES,
};
use crate::component::render_lines_to_viewport;
use crate::editor::Editor;
use crate::frame::{Frame, HistoryBatch};
use crate::highlight::{DIM, RESET};
use crate::input::{InputEvent, InputParser, Key};
use crate::output::{OutputSink, QueueSink};
use crate::render::{prepare_line, visible_width};
use crate::renderer::{Renderer, TerminalCaps};

/// Completion menu rows the viewport may show at once.
pub const MENU_ROWS: usize = 8;

/// Maximum output rows one command may commit (a `yes` run must not fill the
/// scrollback with a million rows in one batch).
const MAX_OUTPUT_ROWS: usize = 200;

/// How the REPL is configured.
#[derive(Clone)]
pub struct ReplConfig {
    /// Prompt drawn before the first editor row.
    pub prompt: String,
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
}

impl ReplConfig {
    /// Single-line prompt with auto-suggestion on, no persisted history and no
    /// product output.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            history_path: None,
            auto_suggest: true,
            sink: None,
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
            .field("history_path", &self.history_path)
            .field("auto_suggest", &self.auto_suggest)
            .field("sink", &self.sink.as_ref().map(|_| "<sink>"))
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
        self.draw()?;
        let mut chunk = [0u8; 4096];
        loop {
            let read = match self.reader.read(&mut chunk) {
                Ok(0) => {
                    // EOF: settle a trailing ambiguous sequence (a bare Escape)
                    // before leaving so no half-parsed key is dropped silently.
                    for event in &self.parser.flush() {
                        self.dispatch(event, &mut on_submit)?;
                    }
                    break;
                }
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };

            let mut exit = false;
            for event in self.parser.feed(&chunk[..read]) {
                // Per-event, not per-chunk: one read may carry several keys and
                // Right must see the ghost the preceding keys produced.
                if self.dispatch(&event, &mut on_submit)? {
                    exit = true;
                    break;
                }
                self.refresh_ghost();
            }
            if exit {
                break;
            }
            self.draw()?;
        }
        self.finish()
    }

    /// Paint one frame: sink output, pending history rows, the editor and menu.
    fn draw(&mut self) -> io::Result<()> {
        self.sync_geometry();
        // Product output lands in the history channel *before* the viewport is
        // placed, so a row produced between two frames can never be written in
        // the middle of one.
        self.drain_sink();
        let width = self.renderer.caps().width;
        let mut rows = self.editor.render(width);
        rows.extend(self.menu_rows());
        let viewport = render_lines_to_viewport(rows, width);
        let history = self.take_history(width);
        self.renderer.draw(Frame { history, viewport })
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
    fn finish(&mut self) -> io::Result<()> {
        self.drain_sink();
        let width = self.renderer.caps().width;
        let history = self.take_history(width);
        self.renderer.draw(Frame {
            history,
            viewport: Vec::new(),
        })
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
        }
    }

    /// Queue the submitted text as history rows, prompt included.
    fn commit_line(&mut self, line: &str) {
        let indent = " ".repeat(visible_width(&self.config.prompt) as usize);
        for (index, logical) in line.split('\n').enumerate() {
            let prefix = if index == 0 {
                self.config.prompt.as_str()
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

/// Owns raw mode for the duration of a standalone REPL.
///
/// Every terminal mode the REPL changes is restored here, including on a panic
/// (which unwinds through this guard) — a REPL that leaves the tty in raw mode
/// makes the parent shell unusable.
pub(crate) struct TerminalGuard;

impl TerminalGuard {
    pub(crate) fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        // Reset rendition, re-enable autowrap, show the cursor, then end the
        // line so the parent prompt starts at column 1.
        let _ = out.write_all(b"\x1b[0m\x1b[?7h\x1b[?25h\r\n");
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Run the demo REPL on the real terminal until `/exit`, Ctrl+D or EOF.
///
/// This is the single implementation behind both the `aish-tui-repl` binary and
/// `AISH_UI=v2` in `aish-cli`: PATH executables plus `/help`, `/exit` and
/// `/clear` complete; every other line runs under `bash -c` and its output is
/// committed as history rows.
pub fn run_standalone(prompt: &str, history_path: Option<PathBuf>) -> io::Result<()> {
    if !io::stdout().is_terminal() || !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "the aish-tui REPL needs a terminal on stdin and stdout",
        ));
    }
    let guard = TerminalGuard::enter()?;
    let config = ReplConfig {
        prompt: prompt.to_string(),
        history_path,
        auto_suggest: true,
        sink: None,
    };
    let sources: Vec<Box<dyn CompletionSource>> = vec![Box::new(WordListSource::new(demo_words()))];
    let mut repl = Repl::new(
        io::BufWriter::with_capacity(8192, io::stdout()),
        io::stdin(),
        config,
        sources,
    );
    let result = repl.run(demo_submit);
    drop(repl);
    drop(guard);
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
                history_path: None,
                auto_suggest: false,
                sink: None,
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
                history_path: None,
                auto_suggest: true,
                sink: None,
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
                history_path: Some(path.clone()),
                auto_suggest: false,
                sink: None,
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
                history_path: None,
                auto_suggest: false,
                sink: Some(Arc::clone(sink) as Arc<dyn OutputSink>),
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
}
