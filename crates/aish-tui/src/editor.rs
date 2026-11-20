//! The prompt editor: a multi-line-capable line buffer with an emacs-ish
//! editing surface, shell history, undo and inline ghost completion.
//!
//! # Why the cursor is a byte offset
//!
//! `cursor` indexes `buffer` **as bytes**, and every mutation goes through
//! [`Editor::prev_boundary`] / [`Editor::next_boundary`]. A `char`-indexed
//! cursor forces a scan from the start on every keystroke and, worse, makes the
//! `Vec<char>` representation tempting — which breaks grapheme clusters and
//! cannot be handed to `&str` APIs. A byte offset keeps insert/delete `O(1)`
//! at the cursor and `O(len)` for the copy, and the boundary helpers make it
//! impossible to land in the middle of a multi-byte scalar. The invariant is
//! `buffer.is_char_boundary(cursor)` after every public method.
//!
//! # Rendering
//!
//! [`Editor::render`] returns display rows with the prompt prepended to the
//! first row, soft-wrapped at the requested width, with a reverse-video cursor
//! cell and an optional dim ghost completion. Rows carry ANSI escapes; the
//! renderer treats them as opaque strings and measures display width with
//! `unicode-width`.
//!
//! # History semantics
//!
//! History is only entered from the draft: the first Up saves the current
//! buffer, later Downs walk back to it and restore it. Any edit while a history
//! entry is showing leaves history mode (the entry stays in the buffer), so the
//! next Up starts from the newest entry again instead of re-editing an old
//! line invisibly.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::highlight::{DIM, RESET};
use crate::input::{InputEvent, Key};

/// How many undo snapshots are retained.
const UNDO_DEPTH: usize = 100;

/// Characters a tab expands to when it enters the buffer.
const TAB_EXPANSION: &str = "    ";

/// Prompt line editor.
///
/// All mutating methods snapshot the pre-change `(buffer, cursor)` pair onto a
/// bounded undo stack and leave history mode first.
#[derive(Debug, Clone, Default)]
pub struct Editor {
    buffer: String,
    /// Byte offset into `buffer`, always a UTF-8 boundary.
    cursor: usize,
    history: Vec<String>,
    /// `Some(i)` while showing `history[i]`; `None` while editing the draft.
    history_index: Option<usize>,
    /// The buffer text captured when history mode was entered.
    draft: String,
    multiline_mode: bool,
    prompt: String,
    ghost: Option<String>,
    undo: Vec<(String, usize)>,
    /// Display column to restore on vertical movement (sticky column).
    preferred_col: Option<usize>,
}

impl Editor {
    /// New editor with the given prompt; starts in single-line mode.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            ..Self::default()
        }
    }

    // ── accessors ──────────────────────────────────────────────────────────

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn set_prompt(&mut self, prompt: impl Into<String>) {
        self.prompt = prompt.into();
    }

    pub fn multiline_mode(&self) -> bool {
        self.multiline_mode
    }

    pub fn set_multiline_mode(&mut self, multiline: bool) {
        self.multiline_mode = multiline;
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Replace the history (e.g. loaded from the session store).
    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.history_index = None;
        self.draft.clear();
    }

    pub fn ghost(&self) -> Option<&str> {
        self.ghost.as_deref()
    }

    pub fn set_ghost(&mut self, ghost: Option<String>) {
        // An empty suggestion is no suggestion; otherwise an invisible ghost
        // would swallow a Right keypress.
        self.ghost = ghost.filter(|g| !g.is_empty());
    }

    /// Append the ghost completion at the cursor and consume it.
    ///
    /// Returns whether a ghost was applied, so the caller can decide to
    /// request a fresh suggestion.
    pub fn accept_ghost(&mut self) -> bool {
        let Some(ghost) = self.ghost.take() else {
            return false;
        };
        self.insert_str(&ghost);
        true
    }

    // ── cursor-boundary helpers ────────────────────────────────────────────

    /// Start byte of the character before `idx` (clamped to 0).
    fn prev_boundary(&self, idx: usize) -> usize {
        self.buffer[..idx]
            .chars()
            .next_back()
            .map(|c| idx - c.len_utf8())
            .unwrap_or(0)
    }

    /// Start byte of the character at `idx` (clamped to the buffer length).
    fn next_boundary(&self, idx: usize) -> usize {
        self.buffer[idx..]
            .chars()
            .next()
            .map(|c| idx + c.len_utf8())
            .unwrap_or(self.buffer.len())
    }

    /// Keep `cursor` inside the buffer and on a scalar boundary.
    fn clamp_cursor(&mut self) {
        if self.cursor > self.buffer.len() {
            self.cursor = self.buffer.len();
        }
        while self.cursor > 0 && !self.buffer.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
    }

    /// Byte offset of the start of the cursor's logical line.
    fn line_start(&self, at: usize) -> usize {
        self.buffer[..at].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    /// Byte offset of the cursor's logical line end (the `\n` or buffer end).
    fn line_end(&self, at: usize) -> usize {
        self.buffer[at..]
            .find('\n')
            .map(|i| at + i)
            .unwrap_or(self.buffer.len())
    }

    // ── undo / history bookkeeping ─────────────────────────────────────────

    /// Snapshot before a mutation and leave history mode.
    fn begin_edit(&mut self) {
        if self.undo.len() == UNDO_DEPTH {
            self.undo.remove(0);
        }
        self.undo.push((self.buffer.clone(), self.cursor));
        self.history_index = None;
        self.preferred_col = None;
    }

    /// Restore the state before the last mutation.
    pub fn undo(&mut self) -> bool {
        let Some((buffer, cursor)) = self.undo.pop() else {
            return false;
        };
        self.buffer = buffer;
        self.cursor = cursor;
        self.clamp_cursor();
        self.preferred_col = None;
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    // ── insertion ──────────────────────────────────────────────────────────

    /// Insert one printable character at the cursor. `'\n'` inserts a newline;
    /// other control characters are ignored.
    pub fn insert_char(&mut self, ch: char) {
        if ch == '\n' {
            self.newline();
            return;
        }
        if ch.is_control() {
            return;
        }
        self.begin_edit();
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Insert pasted text: CRLF and lone CR become `\n`, tabs expand to spaces,
    /// other control characters are dropped.
    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let text = normalize_text(text);
        if text.is_empty() {
            return;
        }
        self.begin_edit();
        self.buffer.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    /// Insert a newline at the cursor (Shift+Enter / Alt+Enter, and plain Enter
    /// in multi-line mode).
    pub fn newline(&mut self) {
        self.begin_edit();
        self.buffer.insert(self.cursor, '\n');
        self.cursor += 1;
    }

    /// Enter key semantics: submit in single-line mode, newline in multi-line
    /// mode. Returns the submitted text when the buffer was taken.
    pub fn enter(&mut self) -> Option<String> {
        if self.multiline_mode {
            self.newline();
            None
        } else {
            Some(self.take_buffer())
        }
    }

    // ── deletion ───────────────────────────────────────────────────────────

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let start = self.prev_boundary(self.cursor);
        self.begin_edit();
        self.buffer.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    /// Discard the buffer without submitting it.
    ///
    /// History is untouched (the line was never accepted) and the cleared
    /// buffer is undoable, so Ctrl+C then Ctrl+Z restores the draft.
    pub fn clear(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        self.begin_edit();
        self.buffer.clear();
        self.cursor = 0;
        self.ghost = None;
    }

    /// Replace the `chars` characters before the cursor with `replacement`.
    ///
    /// This is the completion edit: one undo snapshot for the whole swap, so
    /// Ctrl+Z restores the token the user had typed instead of unwinding the
    /// replacement one character at a time.
    pub fn replace_before_cursor(&mut self, chars: usize, replacement: &str) -> bool {
        let mut start = self.cursor;
        for _ in 0..chars {
            if start == 0 {
                break;
            }
            start = self.prev_boundary(start);
        }
        if start == self.cursor && replacement.is_empty() {
            return false;
        }
        self.begin_edit();
        self.buffer.replace_range(start..self.cursor, replacement);
        self.cursor = start + replacement.len();
        self.ghost = None;
        true
    }

    /// Delete the character under the cursor.
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.buffer.len() {
            return false;
        }
        let end = self.next_boundary(self.cursor);
        self.begin_edit();
        self.buffer.replace_range(self.cursor..end, "");
        true
    }

    /// Ctrl-W: kill the word before the cursor (trailing whitespace first, then
    /// the run of non-whitespace).
    pub fn delete_word_backward(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        let head = &self.buffer[..self.cursor];
        let mut start = head.len();
        for (idx, ch) in head.char_indices().rev() {
            if !ch.is_whitespace() {
                break;
            }
            start = idx;
        }
        for (idx, ch) in head[..start].char_indices().rev() {
            if ch.is_whitespace() {
                break;
            }
            start = idx;
        }
        if start == self.cursor {
            return false;
        }
        self.begin_edit();
        self.buffer.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    /// Ctrl-U: kill from the start of the logical line to the cursor.
    pub fn kill_to_line_start(&mut self) -> bool {
        let start = self.line_start(self.cursor);
        if start == self.cursor {
            return false;
        }
        self.begin_edit();
        self.buffer.replace_range(start..self.cursor, "");
        self.cursor = start;
        true
    }

    /// Ctrl-K: kill from the cursor to the end of the logical line. At a line
    /// end the newline itself is killed, so repeated Ctrl-K joins lines.
    pub fn kill_to_line_end(&mut self) -> bool {
        let mut end = self.line_end(self.cursor);
        if end == self.cursor {
            if end >= self.buffer.len() {
                return false;
            }
            end += 1;
        }
        self.begin_edit();
        self.buffer.replace_range(self.cursor..end, "");
        true
    }

    // ── movement ───────────────────────────────────────────────────────────

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_boundary(self.cursor);
        }
        self.preferred_col = None;
    }

    /// Move right; at the end of the buffer a pending ghost is accepted first
    /// (Right walks over a prediction as if it had been typed).
    pub fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor = self.next_boundary(self.cursor);
        } else if self.ghost.is_some() {
            self.accept_ghost();
        }
        self.preferred_col = None;
    }

    /// Up: previous logical line inside a multi-line buffer, otherwise the
    /// previous history entry.
    pub fn move_up(&mut self) {
        if self.buffer.contains('\n') {
            self.move_line_up();
        } else {
            self.history_prev();
        }
    }

    /// Down: next logical line inside a multi-line buffer, otherwise the next
    /// history entry (restoring the draft at the bottom).
    pub fn move_down(&mut self) {
        if self.buffer.contains('\n') {
            self.move_line_down();
        } else {
            self.history_next();
        }
    }

    fn move_line_up(&mut self) {
        let start = self.line_start(self.cursor);
        if start == 0 {
            self.preferred_col = None;
            return;
        }
        let col = self.display_col_in_line(start, self.cursor);
        let col = *self.preferred_col.get_or_insert(col);
        // Previous line runs from its own start to the `\n` at `start - 1`.
        let prev_start = self.line_start(start - 1);
        self.cursor = self.col_to_byte(prev_start, start - 1, col);
    }

    fn move_line_down(&mut self) {
        let start = self.line_start(self.cursor);
        let end = self.line_end(self.cursor);
        if end >= self.buffer.len() {
            self.preferred_col = None;
            return;
        }
        let col = self.display_col_in_line(start, self.cursor);
        let col = *self.preferred_col.get_or_insert(col);
        let next_start = end + 1;
        let next_end = self.line_end(next_start);
        self.cursor = self.col_to_byte(next_start, next_end, col);
    }

    /// Display columns from `line_start` to `at`.
    fn display_col_in_line(&self, line_start: usize, at: usize) -> usize {
        self.buffer[line_start..at.max(line_start)]
            .chars()
            .map(char_width)
            .sum()
    }

    /// Byte offset of the character starting at display column `col` inside
    /// `[start, end)`, clamped to the end of the range.
    fn col_to_byte(&self, start: usize, end: usize, col: usize) -> usize {
        let mut used = 0usize;
        for (offset, ch) in self.buffer[start..end].char_indices() {
            if used >= col {
                return start + offset;
            }
            used += char_width(ch);
        }
        end
    }

    /// Ctrl-A: start of the logical line.
    pub fn home(&mut self) {
        self.cursor = self.line_start(self.cursor);
        self.preferred_col = None;
    }

    /// Ctrl-E: end of the logical line.
    pub fn end(&mut self) {
        self.cursor = self.line_end(self.cursor);
        self.preferred_col = None;
    }

    // ── history ────────────────────────────────────────────────────────────

    /// Step to the previous (older) history entry, saving the draft first.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.draft = std::mem::take(&mut self.buffer);
                let idx = self.history.len() - 1;
                self.history_index = Some(idx);
                self.buffer = self.history[idx].clone();
            }
            Some(idx) if idx > 0 => {
                self.history_index = Some(idx - 1);
                self.buffer = self.history[idx - 1].clone();
            }
            Some(_) => {}
        }
        self.cursor = self.buffer.len();
        self.clamp_cursor();
        self.preferred_col = None;
        self.ghost = None;
    }

    /// Step to the next (newer) history entry; past the newest, restore the
    /// draft captured when history mode was entered.
    pub fn history_next(&mut self) {
        let Some(idx) = self.history_index else {
            return;
        };
        if idx + 1 < self.history.len() {
            self.history_index = Some(idx + 1);
            self.buffer = self.history[idx + 1].clone();
        } else {
            self.history_index = None;
            self.buffer = std::mem::take(&mut self.draft);
        }
        self.cursor = self.buffer.len();
        self.clamp_cursor();
        self.preferred_col = None;
        self.ghost = None;
    }

    // ── key dispatch ───────────────────────────────────────────────────────

    /// Apply one [`InputEvent`], returning the submitted line when Enter ends
    /// the edit.
    ///
    /// This is the binding table: it is the only place that decides that a
    /// single-line Enter submits while a Shift+Enter always inserts a newline,
    /// so the multi-line chords cannot drift between input layers.
    pub fn handle_event(&mut self, event: &InputEvent) -> Option<String> {
        match event {
            InputEvent::Char(ch) => {
                self.insert_char(*ch);
                None
            }
            InputEvent::Paste(text) => {
                self.insert_str(text);
                None
            }
            InputEvent::Key(key) => self.handle_key(*key),
            InputEvent::Unknown(_) => None,
        }
    }

    fn handle_key(&mut self, key: Key) -> Option<String> {
        match key {
            Key::Enter => self.enter(),
            Key::ShiftEnter | Key::AltEnter => {
                self.newline();
                None
            }
            Key::Backspace => {
                self.backspace();
                None
            }
            Key::Delete => {
                self.delete();
                None
            }
            Key::Left => {
                self.move_left();
                None
            }
            Key::Right => {
                self.move_right();
                None
            }
            Key::Up => {
                self.move_up();
                None
            }
            Key::Down => {
                self.move_down();
                None
            }
            Key::Home => {
                self.home();
                None
            }
            Key::End => {
                self.end();
                None
            }
            Key::CtrlUp | Key::CtrlDown | Key::CtrlLeft | Key::CtrlRight => {
                self.move_word(key);
                None
            }
            Key::Ctrl('A') => {
                self.home();
                None
            }
            Key::Ctrl('E') => {
                self.end();
                None
            }
            Key::Ctrl('W') => {
                self.delete_word_backward();
                None
            }
            Key::Ctrl('U') => {
                self.kill_to_line_start();
                None
            }
            Key::Ctrl('K') => {
                self.kill_to_line_end();
                None
            }
            Key::Ctrl('Z') => {
                self.undo();
                None
            }
            Key::Ctrl('F') => {
                // Ctrl+F accepts the ghost only where Right would; mid-buffer it
                // is a forward motion, matching readline.
                self.move_right();
                None
            }
            _ => None,
        }
    }

    /// Word-wise horizontal motion (Ctrl+Left / Ctrl+Right), readline style:
    /// skip the run the caret is inside or adjacent to, then cross the
    /// separator run, landing on the far side of the next word.
    fn move_word(&mut self, key: Key) {
        match key {
            Key::CtrlLeft => {
                let target = self.word_boundary(self.cursor, true);
                self.cursor = target;
            }
            Key::CtrlRight => {
                let target = self.word_boundary(self.cursor, false);
                self.cursor = target;
            }
            Key::CtrlUp => self.move_line_up(),
            Key::CtrlDown => self.move_line_down(),
            _ => {}
        }
        self.clamp_cursor();
        self.preferred_col = None;
    }

    /// Walk `from` backwards (`backward`) or forwards over one word plus the
    /// separator run beyond it. Words are runs of alphanumerics; every other
    /// character (punctuation, space) separates.
    fn word_boundary(&self, from: usize, backward: bool) -> usize {
        if backward {
            let mut at = from;
            let mut saw_word = false;
            for (idx, ch) in self.buffer[..from].char_indices().rev() {
                if ch.is_alphanumeric() {
                    at = idx;
                    saw_word = true;
                } else if saw_word {
                    return at;
                } else {
                    at = idx;
                }
            }
            at
        } else {
            let mut at = self.buffer.len();
            let mut saw_word = false;
            for (idx, ch) in self.buffer[from..].char_indices() {
                if ch.is_alphanumeric() {
                    saw_word = true;
                } else if saw_word {
                    return from + idx;
                } else {
                    at = from + idx;
                }
            }
            at
        }
    }

    // ── submit ─────────────────────────────────────────────────────────────

    /// Take the buffer for submission, pushing it onto history (deduplicating
    /// against the newest entry) and resetting the editing state.
    pub fn take_buffer(&mut self) -> String {
        let text = std::mem::take(&mut self.buffer);
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.cursor = 0;
        self.history_index = None;
        self.draft.clear();
        self.ghost = None;
        self.undo.clear();
        self.preferred_col = None;
        text
    }

    /// Push an externally submitted line onto history.
    pub fn push_history(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.trim().is_empty() || self.history.last() == Some(&entry) {
            return;
        }
        self.history.push(entry);
    }

    // ── rendering ──────────────────────────────────────────────────────────

    /// Render the editor at `width` columns.
    ///
    /// The prompt prefixes the first row; the buffer soft-wraps, hard-wrapping
    /// at the width the terminal itself would use. The cursor is painted with
    /// reverse video (a space when it sits past the last character), and a
    /// ghost completion is appended in dim text when the cursor is at the end
    /// of the buffer.
    pub fn render(&self, width: u16) -> Vec<String> {
        let width = width.max(1) as usize;
        let prompt_w = self.prompt.width();
        // The first row of the first logical line shares space with the prompt;
        // never let that budget reach zero (a prompt wider than the terminal
        // degrades to one column rather than looping).
        let first_avail = width.saturating_sub(prompt_w).max(1);

        let mut rows: Vec<String> = Vec::new();
        let mut cursor_row = 0usize;
        let mut cursor_col = 0u16;
        let mut cursor_found = false;

        let mut offset = 0usize;
        for (logical_index, logical) in self.buffer.split('\n').enumerate() {
            let cursor_local = if self.cursor >= offset && self.cursor <= offset + logical.len() {
                Some(self.cursor - offset)
            } else {
                None
            };
            let (lines, hit) = wrap_logical(
                logical,
                if logical_index == 0 {
                    first_avail
                } else {
                    width
                },
                width,
                cursor_local,
            );
            if let Some((row, col)) = hit {
                cursor_row = rows.len() + row;
                let prompt_offset = if logical_index == 0 && row == 0 {
                    prompt_w
                } else {
                    0
                };
                cursor_col = (prompt_offset + col) as u16;
                cursor_found = true;
            }
            rows.extend(lines);
            // +1 for the '\n' that `split` consumed.
            offset += logical.len() + 1;
        }

        if rows.is_empty() {
            rows.push(String::new());
        }
        rows[0] = format!("{}{}", self.prompt, rows[0]);

        if !cursor_found {
            // Defensive: every cursor position inside the buffer maps to a row,
            // but a future mutation must never leave the caret unrendered.
            cursor_row = rows.len() - 1;
            cursor_col = rows[cursor_row].width() as u16;
        }

        let mut rows = crate::highlight::render_with_cursor(rows, cursor_row, cursor_col);

        // Ghost completion is painted after the cursor cell: the caret is what
        // the suggested text would replace, so the dim span starts immediately
        // to its right. Only meaningful at the end of the buffer.
        if self.cursor == self.buffer.len() {
            if let Some(ghost) = &self.ghost {
                let ghost = one_line(ghost);
                let taken = usize::from(cursor_col) + 1;
                let room = width.saturating_sub(taken);
                let ghost = truncate_to_width(&ghost, room);
                if !ghost.is_empty() {
                    if let Some(row) = rows.get_mut(cursor_row) {
                        row.push_str(DIM);
                        row.push_str(&ghost);
                        row.push_str(RESET);
                    }
                }
            }
        }

        rows
    }
}

/// Soft-wrap one logical line into display rows.
///
/// `first` is the budget for row 0 (the prompt eats into it), `rest` for every
/// following row. Returns the rows and the cursor's `(row, display column)`
/// when the cursor byte offset fell inside this line.
fn wrap_logical(
    text: &str,
    first: usize,
    rest: usize,
    cursor: Option<usize>,
) -> (Vec<String>, Option<(usize, usize)>) {
    let first = first.max(1);
    let rest = rest.max(1);
    let mut rows: Vec<String> = vec![String::new()];
    let mut col = 0usize;
    let mut hit: Option<(usize, usize)> = None;
    let mut byte = 0usize;

    for ch in text.chars() {
        let avail = if rows.len() == 1 { first } else { rest };
        let cw = char_width(ch);
        // A wide glyph that does not fit in the last column moves to the next
        // row rather than splitting across the terminal's edge. A cursor resting
        // exactly at the budget therefore records the *next* row below.
        if col + cw > avail && col > 0 {
            rows.push(String::new());
            col = 0;
        }
        if cursor == Some(byte) {
            hit = Some((rows.len() - 1, col));
        }
        rows.last_mut().expect("row 0 exists").push(ch);
        col += cw;
        byte += ch.len_utf8();
    }
    if cursor == Some(byte) {
        hit = Some((rows.len() - 1, col));
    }

    if let Some((row, hit_col)) = hit {
        let avail = if row == 0 { first } else { rest };
        if hit_col >= avail && row + 1 == rows.len() {
            rows.push(String::new());
            hit = Some((row + 1, 0));
        }
    }
    (rows, hit)
}

/// Display width of one character; control characters take no cells.
fn char_width(ch: char) -> usize {
    if ch.is_control() {
        0
    } else {
        ch.width().unwrap_or(0)
    }
}

/// Normalize pasted text: CRLF / CR → `\n`, tabs → spaces, other controls
/// dropped.
fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str(TAB_EXPANSION),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// Collapse newlines/tabs into single spaces for a one-row ghost suggestion.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// Truncate `text` to at most `max_width` display columns.
///
/// Used for the ghost suggestion, which must not push the row past the terminal
/// width — an overflowing row would soft-wrap and desynchronize the renderer's
/// row budget from the terminal's.
fn truncate_to_width(text: &str, max_width: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut used = 0usize;
    for ch in text.chars() {
        let cw = char_width(ch);
        if used + cw > max_width {
            break;
        }
        used += cw;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip every escape span so assertions can talk about visible text.
    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < s.len() {
            let ch = s[i..].chars().next().unwrap();
            if ch == '\x1b' {
                let bytes = s.as_bytes();
                let mut j = i + 1;
                if bytes.get(j) == Some(&b'[') {
                    j += 1;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    j += 1;
                } else {
                    j += 1;
                }
                i = j.min(s.len());
            } else {
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out
    }

    #[test]
    fn editor_insert_and_backspace() {
        let mut editor = Editor::new("> ");
        for ch in "hello".chars() {
            editor.insert_char(ch);
        }
        assert_eq!(editor.buffer(), "hello");
        assert_eq!(editor.cursor(), 5);

        assert!(editor.backspace());
        assert_eq!(editor.buffer(), "hell");
        assert_eq!(editor.cursor(), 4);

        // Typing at a moved cursor inserts in the middle.
        editor.home();
        assert_eq!(editor.cursor(), 0);
        editor.insert_char('X');
        assert_eq!(editor.buffer(), "Xhell");
        assert_eq!(editor.cursor(), 1);

        // Backspace at the start of the buffer does nothing and pushes no undo
        // entry, so a later undo cannot resurrect a no-op.
        editor.home();
        assert!(!editor.backspace());
        assert_eq!(editor.buffer(), "Xhell");
        assert_eq!(editor.cursor(), 0);

        // Delete removes at the cursor.
        assert!(editor.delete());
        assert_eq!(editor.buffer(), "hell");
        assert_eq!(editor.cursor(), 0);

        // Submit takes the buffer and resets the editor.
        let submitted = editor.take_buffer();
        assert_eq!(submitted, "hell");
        assert!(editor.buffer().is_empty());
        assert_eq!(editor.cursor(), 0);
        assert_eq!(editor.history(), ["hell".to_string()]);
    }

    #[test]
    fn editor_cursor_never_splits_utf8() {
        let mut editor = Editor::new("> ");
        editor.insert_str("a世界é");
        assert_eq!(editor.buffer(), "a世界é");

        // Walk left over every character; each step must land on a boundary and
        // consume exactly one scalar.
        let mut visited = Vec::new();
        loop {
            let at = editor.cursor();
            assert!(
                editor.buffer().is_char_boundary(at),
                "cursor {at} is not a char boundary"
            );
            visited.push(editor.buffer()[at..].chars().next().map(|c| c.to_string()));
            if at == 0 {
                break;
            }
            editor.move_left();
        }
        // Four lefts from the end reach 0; the visited tails are é, é界, 界世…
        assert_eq!(visited.len(), 5);

        // Deleting backwards eats whole scalars only.
        editor.end();
        assert!(editor.backspace());
        assert_eq!(editor.buffer(), "a世界");
        assert!(editor.backspace());
        assert_eq!(editor.buffer(), "a世");
        assert!(editor.backspace());
        assert_eq!(editor.buffer(), "a");
        assert!(editor.buffer().is_char_boundary(editor.cursor()));
    }

    #[test]
    fn editor_undo_restores_previous_state() {
        let mut editor = Editor::new("$ ");
        editor.insert_str("echo hi");
        editor.insert_str(" there");
        assert_eq!(editor.buffer(), "echo hi there");

        assert!(editor.undo());
        assert_eq!(editor.buffer(), "echo hi");
        assert_eq!(editor.cursor(), 7);

        assert!(editor.undo());
        assert_eq!(editor.buffer(), "");
        assert_eq!(editor.cursor(), 0);

        // Nothing left to undo.
        assert!(!editor.undo());
        assert!(!editor.can_undo());

        // The stack is bounded; the oldest snapshots are dropped rather than
        // growing without limit.
        let mut editor = Editor::new("");
        for _ in 0..UNDO_DEPTH + 50 {
            editor.insert_char('x');
        }
        let mut undos = 0;
        while editor.undo() {
            undos += 1;
        }
        assert_eq!(undos, UNDO_DEPTH);
    }

    #[test]
    fn editor_history_up_down_restores_draft() {
        let mut editor = Editor::new("> ");
        editor.set_history(vec!["first".into(), "second".into()]);

        editor.insert_str("my draft");
        editor.move_up();
        assert_eq!(editor.buffer(), "second");
        editor.move_up();
        assert_eq!(editor.buffer(), "first");
        // At the oldest entry, Up stays put.
        editor.move_up();
        assert_eq!(editor.buffer(), "first");

        editor.move_down();
        assert_eq!(editor.buffer(), "second");
        editor.move_down();
        // Past the newest: the draft comes back, unsullied.
        assert_eq!(editor.buffer(), "my draft");
        assert_eq!(editor.cursor(), "my draft".len());
        // At the draft, Down is a no-op (no wrap-around).
        editor.move_down();
        assert_eq!(editor.buffer(), "my draft");

        // Editing leaves history mode: the next Up starts from the newest entry.
        editor.move_up();
        editor.insert_char('!');
        assert_eq!(editor.buffer(), "second!");
        editor.move_up();
        assert_eq!(editor.buffer(), "second");
    }

    #[test]
    fn editor_render_soft_wraps_at_width() {
        let mut editor = Editor::new("> ");
        editor.insert_str("0123456789abcde");
        // Prompt is 2 columns, so the first row fits 8 buffer columns at width
        // 10 and every later row fits 10.
        let rows = editor.render(10);
        assert_eq!(rows.len(), 2, "rows: {rows:?}");
        assert_eq!(plain(&rows[0]), "> 01234567");
        assert_eq!(plain(&rows[1]), "89abcde ");

        // The cursor (end of buffer) sits on the last row.
        assert!(rows[1].contains(crate::highlight::CURSOR_ON));
        assert!(!rows[0].contains(crate::highlight::CURSOR_ON));

        // Enough text for three rows: 8 + 10 + remainder.
        let mut long = Editor::new("> ");
        long.insert_str("0123456789abcdefghij");
        let rows = long.render(10);
        assert_eq!(rows.len(), 3, "rows: {rows:?}");
        assert_eq!(plain(&rows[0]), "> 01234567");
        assert_eq!(plain(&rows[1]), "89abcdefgh");
        assert_eq!(plain(&rows[2]), "ij ");

        // A narrow terminal still produces at least one column of buffer.
        let rows = editor.render(2);
        assert!(rows.len() > 1);
        assert!(plain(&rows[0]).starts_with("> "));

        // Multi-line buffers wrap per logical line and keep the prompt only on
        // the first row.
        let mut multi = Editor::new("$ ");
        multi.set_multiline_mode(true);
        multi.insert_str("ab\ncd");
        let rows = multi.render(20);
        assert_eq!(rows.len(), 2);
        assert_eq!(plain(&rows[0]), "$ ab");
        // The caret rests after "cd", so the row carries the cursor cell.
        assert_eq!(plain(&rows[1]), "cd ");
        assert!(rows[1].contains(crate::highlight::CURSOR_ON));

        // Empty buffer: one row holding the prompt plus the cursor cell.
        let empty = Editor::new("> ");
        let rows = empty.render(20);
        assert_eq!(rows.len(), 1);
        assert_eq!(plain(&rows[0]), ">  ");
    }

    #[test]
    fn editor_ghost_accepted_on_right() {
        let mut editor = Editor::new("> ");
        editor.insert_str("git ch");
        editor.set_ghost(Some("eckout main".into()));
        assert_eq!(editor.ghost(), Some("eckout main"));

        // The ghost is painted dim right after the cursor cell.
        let rows = editor.render(40);
        assert!(rows[0].contains(crate::highlight::DIM));
        assert!(plain(&rows[0]).contains("eckout main"));
        // …but it is not in the buffer until accepted.
        assert_eq!(editor.buffer(), "git ch");

        // Right at end of buffer accepts the whole suggestion.
        editor.move_right();
        assert_eq!(editor.buffer(), "git checkout main");
        assert_eq!(editor.ghost(), None);

        // Right mid-buffer moves instead of accepting.
        editor.home();
        editor.set_ghost(Some("zzz".into()));
        editor.move_right();
        assert_eq!(editor.cursor(), 1);
        assert_eq!(editor.buffer(), "git checkout main");
        assert_eq!(editor.ghost(), Some("zzz"));

        // accept_ghost is explicit and reports whether anything happened.
        assert!(editor.accept_ghost());
        assert!(!editor.accept_ghost());
        assert_eq!(editor.ghost(), None);

        // An empty hint is no hint (it would swallow a Right press).
        editor.set_ghost(Some(String::new()));
        assert_eq!(editor.ghost(), None);
    }

    #[test]
    fn editor_kill_and_word_deletion() {
        let mut editor = Editor::new("> ");
        editor.insert_str("git commit --amend");
        // Ctrl-W on trailing word.
        assert!(editor.delete_word_backward());
        assert_eq!(editor.buffer(), "git commit ");
        // Ctrl-U clears to line start.
        assert!(editor.kill_to_line_start());
        assert_eq!(editor.buffer(), "");
        assert!(!editor.kill_to_line_start());

        // Ctrl-K is line-relative: it kills only the caret's line tail. At a
        // line end it kills the newline too (emacs `kill-line`), so repeated
        // presses join lines upward; at the very end of the buffer it stops.
        editor.insert_str("abc\ndef");
        editor.move_up();
        assert_eq!(editor.cursor(), 3);
        assert!(editor.kill_to_line_end());
        assert_eq!(editor.buffer(), "abcdef");
        assert_eq!(editor.cursor(), 3);
        assert!(editor.kill_to_line_end());
        assert_eq!(editor.buffer(), "abc");
        assert!(!editor.kill_to_line_end());

        // Word deletion skips whitespace before the word.
        let mut editor = Editor::new("");
        editor.insert_str("foo bar   ");
        assert!(editor.delete_word_backward());
        assert_eq!(editor.buffer(), "foo ");
    }

    #[test]
    fn editor_multiline_enter_and_newline_chords() {
        let mut editor = Editor::new("> ");
        // Single-line Enter submits.
        editor.insert_str("ls");
        assert_eq!(editor.enter(), Some("ls".to_string()));
        assert_eq!(editor.buffer(), "");

        // Multi-line Enter inserts; the buffer grows rather than submitting.
        editor.set_multiline_mode(true);
        editor.insert_str("a");
        assert_eq!(editor.enter(), None);
        editor.insert_str("b");
        assert_eq!(editor.buffer(), "a\nb");

        // Vertical motion stays inside the buffer while it is multi-line.
        editor.end();
        editor.move_up();
        assert_eq!(editor.cursor(), 1);
        editor.move_up();
        assert_eq!(editor.cursor(), 1, "already on the first line");
        editor.move_down();
        assert_eq!(editor.cursor(), 3);
        // Sticky column: moving down through a short line and back restores the
        // original display column instead of collapsing to the short line's end.
        let mut col = Editor::new("");
        col.set_multiline_mode(true);
        col.insert_str("abcdefgh\nab\nabcdefgh");
        // Walk back to the first line: Home is line-relative, so go up twice.
        col.home();
        col.move_up();
        col.move_up();
        assert_eq!(col.cursor(), 0);
        for _ in 0..5 {
            col.move_right();
        }
        assert_eq!(col.cursor(), 5);
        col.move_down();
        // "[ab]" has no column 5, so the caret stops at its end…
        assert_eq!(col.cursor(), 11);
        col.move_down();
        // …and the remembered column 5 is restored on the next line.
        assert_eq!(col.cursor(), 17);

        // Enter inside a pasted block normalizes CRLF.
        editor.insert_str("x\r\ny");
        assert!(editor.buffer().contains("x\ny"));
    }

    #[test]
    fn editor_handle_event_bindings() {
        let mut editor = Editor::new("> ");
        // Printable and paste go into the buffer untouched.
        assert_eq!(editor.handle_event(&InputEvent::Char('a')), None);
        assert_eq!(editor.handle_event(&InputEvent::Paste("b\nc".into())), None);
        assert_eq!(editor.buffer(), "ab\nc");

        // Shift+Enter inserts a newline even though this is a single-line editor;
        // a plain Enter submits.
        assert_eq!(editor.handle_event(&InputEvent::Key(Key::ShiftEnter)), None);
        assert_eq!(editor.buffer(), "ab\nc\n");
        assert_eq!(
            editor.handle_event(&InputEvent::Key(Key::Enter)),
            Some("ab\nc\n".to_string())
        );
        assert_eq!(editor.buffer(), "");

        // Emacs chords are wired to the same operations as the methods.
        let mut editor = Editor::new("> ");
        editor.insert_str("one two");
        editor.handle_event(&InputEvent::Key(Key::Ctrl('W')));
        assert_eq!(editor.buffer(), "one ");
        editor.handle_event(&InputEvent::Key(Key::Ctrl('A')));
        assert_eq!(editor.cursor(), 0);
        editor.handle_event(&InputEvent::Key(Key::Ctrl('E')));
        assert_eq!(editor.cursor(), editor.buffer().len());
        editor.handle_event(&InputEvent::Key(Key::Ctrl('Z')));
        assert_eq!(editor.buffer(), "one two");
        editor.handle_event(&InputEvent::Key(Key::Ctrl('U')));
        assert_eq!(editor.buffer(), "");

        // Word motion and ghost acceptance.
        let mut editor = Editor::new("");
        editor.insert_str("alpha beta gamma");
        editor.handle_event(&InputEvent::Key(Key::Home));
        // Ctrl+Right lands on the end of the next word (readline `forward-word`),
        // Ctrl+Left on the start of the previous one.
        editor.handle_event(&InputEvent::Key(Key::CtrlRight));
        assert_eq!(editor.cursor(), 5);
        editor.handle_event(&InputEvent::Key(Key::CtrlRight));
        assert_eq!(editor.cursor(), 10);
        editor.handle_event(&InputEvent::Key(Key::CtrlLeft));
        assert_eq!(editor.cursor(), 6);
        editor.handle_event(&InputEvent::Key(Key::CtrlLeft));
        assert_eq!(editor.cursor(), 0);
        editor.set_ghost(Some("!".into()));
        editor.handle_event(&InputEvent::Key(Key::End));
        editor.handle_event(&InputEvent::Key(Key::Ctrl('F')));
        assert_eq!(editor.buffer(), "alpha beta gamma!");
    }

    #[test]
    fn clear_discards_the_buffer_and_undo_restores_it() {
        let mut editor = Editor::new("> ");
        editor.insert_str("half typed");
        editor.push_history("kept");
        editor.clear();
        assert_eq!(editor.buffer(), "");
        assert_eq!(editor.cursor(), 0);
        // Clearing is not a submit: history and the undo stack survive.
        assert_eq!(editor.history(), ["kept"]);
        assert!(editor.undo());
        assert_eq!(editor.buffer(), "half typed");
    }

    #[test]
    fn replace_before_cursor_swaps_the_token_in_one_undo() {
        let mut editor = Editor::new("> ");
        editor.insert_str("git sta");
        assert!(editor.replace_before_cursor(3, "status"));
        assert_eq!(editor.buffer(), "git status");
        assert_eq!(editor.cursor(), "git status".len());
        // One undo returns the typed token, not the replacement minus a char.
        assert!(editor.undo());
        assert_eq!(editor.buffer(), "git sta");
        // A multi-byte token is measured in chars, never bytes.
        editor.insert_str(" 漢字");
        assert!(editor.replace_before_cursor(2, "漢"));
        assert_eq!(editor.buffer(), "git sta 漢");
    }
}
