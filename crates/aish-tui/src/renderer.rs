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
use crate::render::{strip_escapes, visible_width, PreparedLine};

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
    /// 1-based row of `last_viewport[0]`, if a viewport was painted. Values
    /// above the screen height mean "scroll this many rows before writing".
    last_top: Option<usize>,
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

    /// Write `bytes` straight to the sink, outside any paint transaction.
    ///
    /// Only for sequences that are not part of a frame — the REPL's final
    /// restore is written here so it shares the sink's flush semantics instead
    /// of racing `io::stdout()` against a buffered writer.
    pub fn write_trailing(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.out.flush()
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
        // A viewport taller than the screen keeps its TAIL: the editor/prompt
        // sits at the bottom, so dropping the head is the only choice that
        // keeps the input line visible.
        let viewport = &frame.viewport[frame.viewport.len().saturating_sub(height)..];

        // Snapshot the band state: a failed write must not desync the memo
        // from the screen, or a retried frame would repaint history twice.
        let prev_top = self.last_top;
        let prev_viewport = self.last_viewport.clone();

        let mut buf = Vec::with_capacity(64 + viewport.len() * (self.caps.width as usize + 12));
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
                // An empty batch advances the watermark without emitting
                // history bytes (so the product may release its buffer), but
                // the viewport is re-supplied every frame and may have changed
                // — it still has to be placed.
                self.place_viewport(viewport, &mut buf);
                Some((batch.id, 0))
            }
            Some(batch) => Some(self.commit_history(batch, viewport, &mut buf)),
            None => {
                self.place_viewport(viewport, &mut buf);
                None
            }
        };

        self.paint_end(&mut buf);

        if let Err(err) = self.out.write_all(&buf).and_then(|()| self.out.flush()) {
            // Roll the memo back so the next frame repaints from the same
            // starting state; the ack watermark never advanced.
            self.last_top = prev_top;
            self.last_viewport = prev_viewport;
            return Err(err);
        }

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
    fn band_top(&self, rows: usize) -> usize {
        let height = self.caps.height.max(1);
        let rows = rows.min(height as usize);
        (height as usize - rows)
            .saturating_add(1)
            .min(height as usize)
    }

    /// How many leading history rows the previous frame already painted, in
    /// place and verbatim.
    ///
    /// The product commits the row it was just editing, and that row is
    /// normally still on screen: the editor's first viewport row *is* the
    /// prompt plus the typed line. Rewriting it would emit every submitted line
    /// twice, and a terminal that reflows (tmux, a pty that echoes) would push
    /// the duplicate into scrollback. Those rows are left untouched and the
    /// block continues below them.
    ///
    /// Comparison is on the *visible* text: the viewport row carries a
    /// zero-width cursor cell (`\x1b[7m\x1b[27m`) and committed rows never do,
    /// so a byte comparison would never match the one row it must.
    ///
    /// A row only counts as painted when the previous frame painted it at or
    /// above the bottom row (anything below has scrolled away) and with the
    /// same column budget — a resized row must be rewritten.
    fn already_painted_rows(&self, batch: &HistoryBatch) -> usize {
        let Some(top) = self.last_top else {
            return 0;
        };
        let height = self.caps.height.max(1) as usize;
        let mut painted = 0;
        for (offset, (old, new)) in self.last_viewport.iter().zip(batch.rows.iter()).enumerate() {
            if top + offset > height {
                break;
            }
            if old.width != new.width || strip_escapes(&old.text) != strip_escapes(&new.text) {
                break;
            }
            painted += 1;
        }
        painted
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
        let height = self.caps.height.max(1) as usize;
        let width = self.caps.width;
        let count = batch.rows.len();
        let rows = viewport.len();

        // Rows the previous frame already put on screen are not rewritten.
        let skip = self.already_painted_rows(batch);
        let fresh = &batch.rows[skip..];

        // Erase the live band before anything scrolls: rows pushed off the top
        // by the history write must be finished history or blanks, never a
        // stale in-progress frame. Erasure is per line (`EL`) from column 1, so
        // no line is ever deleted and committed rows above are untouched. The
        // band always ends at or above the bottom row, so no scroll is needed.
        // Rows that simply become history keep their cells: erasing and
        // rewriting them is what painted a submitted line twice.
        if let Some(top) = self.last_top {
            for offset in skip..self.last_viewport.len().min(height) {
                goto(buf, (top + offset) as u16);
                buf.extend_from_slice(ERASE_TO_EOL.as_bytes());
            }
        }

        // The block starts where the live band started; on the very first
        // frame it starts on the bottom row and scrolls rows in. `\r\n`, not
        // `\n`: in raw mode (the shell owns the tty) a bare LF moves down
        // without returning to column 1.
        //
        // `start` may exceed the screen height when the previous block ended
        // flush with the bottom: that means "scroll first, then write", and
        // everything already painted shifts up by the same amount — which is
        // fine, because the live band was just erased.
        let start = self.last_top.unwrap_or(height).max(1) + skip;
        let scrolled = goto_scrolling(buf, start, height);
        for (index, row) in fresh.iter().enumerate() {
            if index > 0 {
                buf.extend_from_slice(CRLF);
            }
            write_row(buf, row, width);
        }
        // The viewport starts on the next row after the last committed row.
        // When every row was already on screen the cursor is *on* that row
        // after the initial move, so no newline is owed.
        if !fresh.is_empty() {
            buf.extend_from_slice(CRLF);
        }
        for (index, line) in viewport.iter().enumerate() {
            if index > 0 {
                buf.extend_from_slice(CRLF);
            }
            write_row(buf, line, width);
        }

        // Physical row of the viewport's first line after the terminal applied
        // any scroll. The block starts on the row `goto_scrolling` settled on
        // (`start` shifted up by the rows it scrolled), the fresh history rows
        // occupy the next `count - skip` rows, and the viewport follows them;
        // rows past the bottom shift the whole block up by the overflow. The
        // skipped rows were already on screen above the block.
        let block = (count - skip) + rows;
        let overflow = (start - scrolled + block).saturating_sub(1 + height);
        let top_after = (start - scrolled + (count - skip)).saturating_sub(overflow);
        // With an empty viewport the anchor must still advance: dropping it
        // would make the next frame bottom-anchor its band and paint over the
        // rows committed here (invariant 4). `height + 1` is the legal "one
        // scroll pending" anchor.
        self.last_top = Some(top_after.max(1));
        self.last_viewport = viewport.to_vec();
        (batch.id, count)
    }

    /// Rewrite the live viewport at its anchor, diffing rows when possible.
    fn place_viewport(&mut self, viewport: &[PreparedLine], buf: &mut Vec<u8>) {
        let height = self.caps.height.max(1) as usize;
        let rows = viewport.len();
        let previous_rows = self.last_viewport.len();
        let previous_top = self.last_top;

        if rows == 0 {
            // Nothing to place. The band left by the previous frame is cleared
            // to the end of the display; no whole line is ever deleted.
            if let Some(top) = previous_top {
                if top <= height {
                    goto(buf, top as u16);
                    buf.extend_from_slice(ERASE_TO_EOD.as_bytes());
                }
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
        if top.saturating_add(rows - 1) > height {
            if let Some(old_top) = previous_top {
                // The old band is live area and is about to be overwritten;
                // clear it first so a short old band cannot leave stale cells
                // below the new one.
                if old_top <= height {
                    goto(buf, old_top as u16);
                    buf.extend_from_slice(ERASE_TO_EOD.as_bytes());
                }
            }
            let scrolled = goto_scrolling(buf, top, height);
            for (index, line) in viewport.iter().enumerate() {
                if index > 0 {
                    buf.extend_from_slice(CRLF);
                }
                write_row(buf, line, self.caps.width);
            }
            let overflow = (top + rows - 1).saturating_sub(height);
            self.last_top = Some(top.saturating_sub(scrolled + overflow).max(1));
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
                goto(buf, (top + index) as u16);
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
            goto(buf, (top + index).min(height) as u16);
            write_row(buf, line, self.caps.width);
        }
        let below = top + rows;
        if below <= height {
            goto(buf, below as u16);
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

/// Move to `row`, scrolling first when it lies past the bottom of the screen.
///
/// Returns the number of rows the whole screen scrolled, so the caller can
/// shift its own recorded geometry by the same amount.
fn goto_scrolling(buf: &mut Vec<u8>, row: usize, height: usize) -> usize {
    let scroll = row.saturating_sub(height);
    goto(buf, row.min(height).max(1) as u16);
    for _ in 0..scroll {
        buf.extend_from_slice(CRLF);
    }
    scroll
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
