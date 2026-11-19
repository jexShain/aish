//! Dual-channel renderer: history committed into scrollback, viewport kept live.
//!
//! # Model
//!
//! Two row channels that must never be confused:
//!
//! * **History** — finished rows. [`Renderer::draw`] erases the live band,
//!   writes the batch top-down from the band's first row, then continues with
//!   the viewport in the same downward flow and acks the batch id once the bytes
//!   were flushed. Rows that overflow the bottom scroll the terminal, which is
//!   what pushes finished rows into scrollback. Those rows are never rewritten.
//! * **Viewport** — a complete mirror of the mutable area, re-supplied in full
//!   every frame. It hugs whatever history is still on screen, and is diffed row
//!   by row against the previous frame; a spinner repaints exactly one row.
//!
//! # Invariants
//!
//! 1. History finality is decided by the product. Nothing here infers "done"
//!    from a row scrolling out of the visible area.
//! 2. History is append-only with monotonic ids. A batch at or below
//!    [`Renderer::acked_history_id`] is dropped, so a replayed paint cannot
//!    duplicate rows; the watermark advances only after the flush.
//! 3. Every paint is atomic: everything between the leading `\x1b[?25l` and the
//!    closing `\x1b[?25h` leaves in a single `write_all`, wrapped in DEC 2026
//!    synchronized output when the terminal supports it, with autowrap disabled
//!    so a full-width row cannot wrap and scroll the frame.
//! 4. Top rows are never deleted. Terminals such as Terminal.app archive rows
//!    that leave the top into scrollback, so deleting or clearing whole rows
//!    there would mutate committed history. Only the live band is touched, and
//!    only with `EL` (erase to end of line) or `ED` (erase to end of display).
//! 5. Component dirtiness is reference-based (`Arc` identity, see
//!    [`crate::component`]), never a deep string diff of component output.
//!
//! # Geometry
//!
//! The viewport hugs history. `last_top` remembers the row the viewport was
//! painted on; a commit replaces the live band with `history + viewport` as one
//! contiguous block written downward from that row, so the viewport ends up
//! directly beneath the committed rows and the terminal scrolls if the block
//! runs past the bottom. When nothing is committed, the viewport is rewritten
//! in place at the same anchor. Only a first paint (or a resize, via
//! [`Renderer::invalidate`]) bottom-anchors it at `height - len + 1`.
//!
//! Because the block is emitted as one downward flow, no absolute row is ever
//! computed for the viewport after a commit — which is exactly what keeps a
//! committed row from being painted over by the live band when the block does
//! not fill the screen.

use std::io::{self, Write};

use crate::frame::{Frame, HistoryBatch};
use crate::render::{visible_width, PreparedLine};

/// Hide the cursor while rows are being moved.
const HIDE_CURSOR: &str = "\x1b[?25l";
/// Restore the cursor once the frame is complete.
const SHOW_CURSOR: &str = "\x1b[?25h";
/// Begin a synchronized-output transaction (DEC 2026).
const SYNC_BEGIN: &str = "\x1b[?2026h";
/// End a synchronized-output transaction (DEC 2026).
const SYNC_END: &str = "\x1b[?2026l";
/// Disable autowrap (DECAWM): a full-width row must not wrap into a new line.
const AUTOWRAP_OFF: &str = "\x1b[?7l";
/// Re-enable autowrap.
const AUTOWRAP_ON: &str = "\x1b[?7h";
/// Reset rendition so erasures use the default background (BCE-safe).
const SEGMENT_RESET: &str = "\x1b[0m";
/// Erase from the cursor to the end of the line.
const ERASE_TO_EOL: &str = "\x1b[K";
/// Erase from the cursor to the end of the display.
const ERASE_TO_EOD: &str = "\x1b[J";
/// CRLF: a bare LF would not return to column 1 in raw mode.
const CRLF: &[u8] = b"\r\n";

/// What the terminal can do, and how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCaps {
    /// DEC 2026 synchronized output, so a multi-row paint is not torn.
    pub synchronized_output: bool,
    /// Columns available to a prepared row.
    pub width: u16,
    /// Visible rows; the viewport never occupies more than these.
    pub height: u16,
}

impl Default for TerminalCaps {
    fn default() -> Self {
        Self {
            synchronized_output: true,
            width: 80,
            height: 24,
        }
    }
}

/// Renders [`Frame`]s into a byte sink.
///
/// `W` is any [`Write`] — a `Vec<u8>` in tests, so the exact escape sequences
/// can be asserted byte for byte.
pub struct Renderer<W: Write> {
    out: W,
    caps: TerminalCaps,
    /// Highest history batch id that is on the wire.
    acked_id: u64,
    /// Rows painted by the previous frame, in frame order.
    last_viewport: Vec<PreparedLine>,
    /// 1-based row of `last_viewport[0]`, if a viewport was painted.
    last_top: Option<u16>,
    /// History rows committed through this renderer (diagnostics and tests).
    history_rows: usize,
}

impl<W: Write> Renderer<W> {
    /// Build a renderer for `out`. Nothing is written until the first
    /// [`draw`](Renderer::draw).
    pub fn new(out: W, caps: TerminalCaps) -> Self {
        Self {
            out,
            caps,
            acked_id: 0,
            last_viewport: Vec::new(),
            last_top: None,
            history_rows: 0,
        }
    }

    /// Highest history batch id whose rows were written and flushed.
    pub fn acked_history_id(&self) -> u64 {
        self.acked_id
    }

    /// Number of history rows committed so far.
    pub fn history_rows(&self) -> usize {
        self.history_rows
    }

    /// Current capabilities.
    pub fn caps(&self) -> TerminalCaps {
        self.caps
    }

    /// Recover the sink, e.g. to assert on the captured bytes in tests.
    pub fn into_inner(self) -> W {
        self.out
    }

    /// Forget the frame memo so the next paint rewrites every row.
    ///
    /// Use after anything disturbed the screen behind the renderer's back
    /// (external program output, scrollback clear). Resize also calls this.
    pub fn invalidate(&mut self) {
        self.last_viewport.clear();
        self.last_top = None;
    }

    /// Adopt a new geometry. Rows must be re-prepared at the new width, so the
    /// whole viewport is scheduled for repaint.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.caps.width = width;
        self.caps.height = height;
        self.invalidate();
    }

    /// Draw one frame. All bytes for the frame leave in a single `write_all`
    /// bracketed by cursor hiding and (when supported) synchronized output.
    pub fn draw(&mut self, frame: Frame) -> io::Result<()> {
        let height = self.caps.height.max(1) as usize;
        let rows = frame.viewport.len().min(height);
        let viewport = &frame.viewport[..rows];

        let mut buf = Vec::with_capacity(64 + rows * (self.caps.width as usize + 12));
        self.paint_begin(&mut buf);

        // The watermark check comes first: an empty batch with a stale id must
        // not move the ack backwards.
        let committed = match frame.history.as_ref() {
            Some(batch) if batch.id <= self.acked_id => {
                tracing::trace!(
                    id = batch.id,
                    acked = self.acked_id,
                    "dropping history batch at or below the ack watermark"
                );
                self.place_viewport(viewport, &mut buf);
                None
            }
            Some(batch) if batch.rows.is_empty() => {
                // An empty batch advances the watermark without emitting bytes
                // (so the product may release its buffer); the screen is
                // unchanged, so the viewport needs no repaint either.
                Some((batch.id, 0))
            }
            Some(batch) => Some(self.commit_history(batch, viewport, &mut buf)),
            None => {
                self.place_viewport(viewport, &mut buf);
                None
            }
        };

        self.paint_end(&mut buf);

        self.out.write_all(&buf)?;
        self.out.flush()?;

        // The ack is only durable once the rows are on the wire.
        if let Some((id, count)) = committed {
            self.acked_id = id;
            self.history_rows += count;
        }
        Ok(())
    }

    /// Open a paint transaction: hide the cursor, open the synchronized-output
    /// block (when supported), and disable autowrap so a full-width row cannot
    /// wrap and scroll the frame mid-write.
    fn paint_begin(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(HIDE_CURSOR.as_bytes());
        if self.caps.synchronized_output {
            buf.extend_from_slice(SYNC_BEGIN.as_bytes());
        }
        buf.extend_from_slice(AUTOWRAP_OFF.as_bytes());
    }

    /// Close the paint transaction in the exact reverse order: end the sync
    /// block, restore autowrap, then show the cursor.
    fn paint_end(&self, buf: &mut Vec<u8>) {
        if self.caps.synchronized_output {
            buf.extend_from_slice(SYNC_END.as_bytes());
        }
        buf.extend_from_slice(AUTOWRAP_ON.as_bytes());
        buf.extend_from_slice(SHOW_CURSOR.as_bytes());
    }

    /// First row (1-based) of a bottom-anchored band of `rows` rows. An empty
    /// band collapses onto the bottom row.
    fn band_top(&self, rows: usize) -> u16 {
        let height = self.caps.height.max(1);
        let rows = (rows as u16).min(height);
        (height - rows).saturating_add(1).min(height)
    }

    /// Commit history and paint the viewport directly below it, as one
    /// contiguous block that the terminal can scroll.
    ///
    /// Writing the viewport in the same flow is what keeps committed rows
    /// intact: absolute-positioning the viewport at the bottom would overwrite
    /// the last history rows whenever the block did not fill the screen.
    fn commit_history(
        &mut self,
        batch: &HistoryBatch,
        viewport: &[PreparedLine],
        buf: &mut Vec<u8>,
    ) -> (u64, usize) {
        let height = self.caps.height.max(1);
        let width = self.caps.width;
        let count = batch.rows.len();
        let rows = viewport.len();

        // Erase the live band before anything scrolls: rows pushed off the top
        // by the history write must be finished history or blanks, never a
        // stale in-progress frame. Erasure is per line (`EL`) from column 1, so
        // no line is ever deleted and committed rows above are untouched.
        if let Some(top) = self.last_top {
            for offset in 0..self.last_viewport.len().min(height as usize) {
                goto(buf, top.saturating_add(offset as u16).min(height));
                buf.extend_from_slice(ERASE_TO_EOL.as_bytes());
            }
        }

        // The block starts where the live band started; on the very first
        // frame it starts on the bottom row and scrolls rows in. `\r\n`, not
        // `\n`: in raw mode (the shell owns the tty) a bare LF moves down
        // without returning to column 1.
        let start = self.last_top.unwrap_or(height).min(height);
        goto(buf, start);
        for (index, row) in batch.rows.iter().enumerate() {
            if index > 0 {
                buf.extend_from_slice(CRLF);
            }
            write_row(buf, row, width);
        }
        buf.extend_from_slice(CRLF);
        for (index, line) in viewport.iter().enumerate() {
            if index > 0 {
                buf.extend_from_slice(CRLF);
            }
            write_row(buf, line, width);
        }

        // Physical row of the viewport's first line after the terminal applied
        // any scroll: the block needed `count + rows` rows from `start`, and
        // everything past the bottom shifts the whole block up by the overflow.
        let overflow = (start as usize + count + rows).saturating_sub(1 + height as usize);
        let top_after = (start as usize + count).saturating_sub(overflow);
        self.last_top = (rows > 0).then(|| top_after.min(height as usize).max(1) as u16);
        self.last_viewport = viewport.to_vec();
        (batch.id, count)
    }

    /// Rewrite the live viewport at its anchor, diffing rows when possible.
    fn place_viewport(&mut self, viewport: &[PreparedLine], buf: &mut Vec<u8>) {
        let height = self.caps.height.max(1);
        let rows = viewport.len();
        let previous_rows = self.last_viewport.len();
        let previous_top = self.last_top;

        if rows == 0 {
            // Nothing to place. The band left by the previous frame is cleared
            // to the end of the display; no whole line is ever deleted.
            if let Some(top) = previous_top {
                goto(buf, top);
                buf.extend_from_slice(ERASE_TO_EOD.as_bytes());
            }
            self.last_viewport.clear();
            self.last_top = None;
            return;
        }

        // The viewport hugs history: it keeps the row the previous frame left
        // it on, and only a first paint (or a resize) bottom-anchors it.
        // Re-anchoring to the bottom would make the row jump away from the
        // history it is supposed to sit under.
        let top = previous_top.unwrap_or_else(|| self.band_top(rows));

        // A band taller than the room left below its anchor must be written as
        // one downward flow: the write then scrolls, and rows leaving the top
        // are finished history (never live rows, which sit at or below the
        // anchor). The anchor shifts up by exactly that overflow.
        if top.saturating_add(rows as u16 - 1) > height {
            if let Some(old_top) = previous_top {
                // The old band is live area and is about to be overwritten;
                // clear it first so a short old band cannot leave stale cells
                // below the new one.
                goto(buf, old_top);
                buf.extend_from_slice(ERASE_TO_EOD.as_bytes());
            }
            goto(buf, top);
            for (index, line) in viewport.iter().enumerate() {
                if index > 0 {
                    buf.extend_from_slice(CRLF);
                }
                write_row(buf, line, self.caps.width);
            }
            let overflow = (top as usize + rows - 1).saturating_sub(height as usize);
            self.last_top = Some((top as usize).saturating_sub(overflow).max(1) as u16);
            self.last_viewport = viewport.to_vec();
            return;
        }

        // Fast path: same rows, same anchor, so this is a pure in-place update —
        // one cursor move per dirty row.
        if previous_rows == rows && previous_top == Some(top) {
            let mut changed_any = false;
            for (index, line) in viewport.iter().enumerate() {
                let unchanged = self
                    .last_viewport
                    .get(index)
                    .is_some_and(|prev| prev.text == line.text && prev.width == line.width);
                if unchanged {
                    continue;
                }
                changed_any = true;
                goto(buf, top + index as u16);
                write_row(buf, line, self.caps.width);
            }
            if !changed_any {
                return;
            }
            self.last_viewport = viewport.to_vec();
            return;
        }

        // Structural repaint: the row count changed (or there is no previous
        // band). Rewrite the whole band at its anchor and clear whatever the
        // previous band left below it — `ED`, never a line delete.
        for (index, line) in viewport.iter().enumerate() {
            goto(buf, (top + index as u16).min(height));
            write_row(buf, line, self.caps.width);
        }
        let below = top.saturating_add(rows as u16);
        if below <= height {
            goto(buf, below);
            buf.extend_from_slice(ERASE_TO_EOD.as_bytes());
        }
        self.last_viewport = viewport.to_vec();
        self.last_top = Some(top);
    }
}

/// Move the cursor to column 1 of `row` (1-based, clamped to at least 1).
fn goto(buf: &mut Vec<u8>, row: u16) {
    buf.extend_from_slice(b"\x1b[");
    buf.extend_from_slice(row.max(1).to_string().as_bytes());
    buf.extend_from_slice(b";1H");
}

/// Emit one prepared row at the cursor, clearing the cells it does not cover.
///
/// Two cases, both of which exist to avoid destroying a cell:
///
/// * Non-ASCII rows are reset and erased *before* the text. `unicode-width` can
///   over-count combining-heavy scripts, so a row it measures as full may render
///   short and leave stale cells from the previous occupant — and those cells
///   would be baked into a committed history row. The leading `EL` clears the
///   whole line from column 1, so no trailing erase is needed.
/// * ASCII rows erase their tail only when they do not fill the row. With
///   autowrap disabled a full-width row leaves the cursor on the last column,
///   where `EL` would delete the character just written (and leave a
///   pending-wrap cell behind).
fn write_row(buf: &mut Vec<u8>, line: &PreparedLine, terminal_width: u16) {
    if !line.text.is_ascii() {
        buf.extend_from_slice(SEGMENT_RESET.as_bytes());
        buf.extend_from_slice(ERASE_TO_EOL.as_bytes());
        buf.extend_from_slice(line.text.as_bytes());
        return;
    }
    buf.extend_from_slice(line.text.as_bytes());
    if visible_width(&line.text) < terminal_width {
        buf.extend_from_slice(ERASE_TO_EOL.as_bytes());
    }
}
