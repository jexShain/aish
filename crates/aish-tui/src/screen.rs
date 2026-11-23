//! Cell-grid screen with per-cell diffing, for the fullscreen UI.
//!
//! The inline [`crate::renderer::Renderer`] streams rows downward and lets the
//! terminal scroll finished ones into its own scrollback. A fullscreen UI has
//! the opposite contract: it owns every cell of the alternate screen and may
//! repaint any of them, so it needs an absolute-coordinate model.
//!
//! This module is that model, and nothing more: a rectangular grid of
//! [`Cell`]s plus the byte-level diff that turns "previous grid" into "escape
//! sequences". Layout lives in the caller.
//!
//! # Width
//!
//! A double-width glyph occupies one cell and marks the next as its
//! continuation ([`Cell::ch`] is empty there). The continuation cell is never
//! written on its own: writing it would shift the following glyph left by one
//! column, which is exactly the corruption the wide-glyph handling in
//! [`crate::render`] exists to prevent.

use crate::render::visible_width;

/// One terminal cell.
///
/// `ch` is empty for a blank cell. Colours are `None` when the terminal
/// default applies, so a plain screen writes no SGR at all.
///
/// A double-width glyph claims two cells: its own, and a marked continuation
/// one. That mark is a separate flag rather than "empty `ch`", because a blank
/// cell is also empty — conflating the two drops blank columns from the output
/// and shifts every following glyph left.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cell {
    /// The glyph; empty for a blank cell.
    pub ch: String,
    /// Foreground as a 24-bit value.
    pub fg: Option<(u8, u8, u8)>,
    /// Background as a 24-bit value.
    pub bg: Option<(u8, u8, u8)>,
    /// Bold attribute.
    pub bold: bool,
    /// Whether this cell is the right half of the glyph to its left.
    pub continuation: bool,
}

impl Cell {
    /// A cell holding one glyph with the terminal's default colours.
    pub fn new(ch: impl Into<String>) -> Self {
        Self {
            ch: ch.into(),
            ..Self::default()
        }
    }

    /// The right half of a double-width glyph: never written on its own.
    pub fn continuation(style: &CellStyle) -> Self {
        Self {
            continuation: true,
            ..style.apply(Self::default())
        }
    }

    /// Whether this cell is a continuation of the glyph to its left.
    pub fn is_continuation(&self) -> bool {
        self.continuation
    }

    /// Whether the cell carries no visible content and no styling.
    pub fn is_blank(&self) -> bool {
        self.ch.is_empty()
            && !self.continuation
            && self.fg.is_none()
            && self.bg.is_none()
            && !self.bold
    }
}

/// A rectangular screen of cells, `width * height`, row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl Screen {
    /// A blank screen. Zero on either axis is coerced to one, so every
    /// index below stays in range even on a degenerate terminal.
    pub fn new(width: u16, height: u16) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            width,
            height,
            cells: vec![Cell::default(); width as usize * height as usize],
        }
    }

    /// Screen width in columns.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Screen height in rows.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Read one cell, or `None` outside the grid.
    pub fn cell(&self, x: u16, y: u16) -> Option<&Cell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(&self.cells[y as usize * self.width as usize + x as usize])
    }

    /// Iterate the cells of one row, left to right. An out-of-range row
    /// yields nothing rather than panicking.
    pub fn cells_row(&self, y: u16) -> impl Iterator<Item = &Cell> {
        let start = usize::from(y) * usize::from(self.width);
        let end = start + usize::from(self.width);
        self.cells.get(start..end).unwrap_or(&[]).iter()
    }

    /// Reset every cell to blank without reallocating.
    pub fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::default();
        }
    }

    /// Write `text` at `(x, y)`, clipped to the right edge and to this row.
    ///
    /// Double-width glyphs claim two cells; a glyph that does not fit the
    /// remaining columns is dropped rather than split, because half a glyph
    /// would shift the rest of the row. Styling applies to every cell the call
    /// writes.
    pub fn put(&mut self, x: u16, y: u16, text: &str, style: &CellStyle) {
        if y >= self.height {
            return;
        }
        let mut col = x;
        for ch in text.chars() {
            if col >= self.width {
                break;
            }
            // Tab expands to the next eight-column stop, matching the inline
            // renderer, so a prompt with a tab lines up in both modes.
            if ch == '\t' {
                let stop = crate::render::TAB_WIDTH;
                let advance = stop - (col as usize % stop);
                let end = (col as usize + advance).min(self.width as usize) as u16;
                for fill in col..end {
                    if let Some(cell) = self.cell_mut(fill, y) {
                        *cell = style.apply(Cell::new(" "));
                    }
                }
                col = end;
                continue;
            }
            if ch.is_control() {
                continue;
            }
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width == 0 {
                continue;
            }
            if col as usize + width > self.width as usize {
                break;
            }
            let index = y as usize * self.width as usize + col as usize;
            self.cells[index] = style.apply(Cell::new(ch.to_string()));
            if width == 2 {
                self.cells[index + 1] = Cell::continuation(style);
            }
            col += width as u16;
        }
    }

    /// Write `text` right-aligned so it ends at `right` (exclusive).
    pub fn put_right(&mut self, right: u16, y: u16, text: &str, style: &CellStyle) {
        let width = visible_width(text);
        let x = right.saturating_sub(width);
        self.put(x, y, text, style);
    }

    /// Fill a horizontal span with a repeated glyph.
    pub fn fill_row(&mut self, x: u16, y: u16, count: u16, ch: char, style: &CellStyle) {
        for offset in 0..count {
            if let Some(cell) = self.cell_mut(x.saturating_add(offset), y) {
                *cell = style.apply(Cell::new(ch.to_string()));
            }
        }
    }

    /// Draw a single-line box border with the ASCII/box-drawing corner set.
    pub fn box_frame(&mut self, x: u16, y: u16, width: u16, height: u16, style: &CellStyle) {
        if width < 2 || height < 2 {
            return;
        }
        let right = x + width - 1;
        let bottom = y + height - 1;
        self.put(x, y, "╭", style);
        self.put(right, y, "╮", style);
        self.put(x, bottom, "╰", style);
        self.put(right, bottom, "╯", style);
        self.fill_row(x + 1, y, width - 2, '─', style);
        self.fill_row(x + 1, bottom, width - 2, '─', style);
        for row in (y + 1)..bottom {
            self.put(x, row, "│", style);
            self.put(right, row, "│", style);
        }
    }

    /// Mutable access to one cell, or `None` outside the grid.
    pub fn cell_mut(&mut self, x: u16, y: u16) -> Option<&mut Cell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(&mut self.cells[y as usize * self.width as usize + x as usize])
    }

    /// Escape sequences turning `previous` into `self`.
    ///
    /// Both grids must share the dimensions; a mismatch (a resize) is
    /// reported as [`Diff::full_redraw`] because cell-for-cell comparison
    /// would be meaningless. Only changed cells are addressed, and runs of
    /// changes no more than [`GAP`] cells apart are merged so a styled row
    /// costs one cursor move instead of one per cell.
    pub fn diff(&self, previous: &Screen) -> Diff {
        if self.width != previous.width || self.height != previous.height {
            return Diff::full_redraw(self);
        }

        let mut buf = Vec::new();
        let mut previous_cell: Option<StyleKey> = None;
        let mut last_written: Option<u16> = None;
        let mut row: Option<u16> = None;

        for y in 0..self.height {
            for x in 0..self.width {
                let cell = &self.cells[y as usize * self.width as usize + x as usize];
                let old = &previous.cells[y as usize * self.width as usize + x as usize];
                if cell == old {
                    continue;
                }
                // A continuation cell is written by the wide glyph to its
                // left. Addressing it would either shift the row (a glyph
                // there) or punch a hole in it (a space), so it is always
                // skipped; a changed glyph therefore emits both columns when
                // its own cell is visited.
                if cell.is_continuation() {
                    continue;
                }
                let contiguous = row == Some(y) && last_written == Some(x.wrapping_sub(1));
                if !contiguous {
                    write_goto(&mut buf, x, y);
                }
                write_cell(&mut buf, cell, &mut previous_cell);
                last_written = Some(x);
                row = Some(y);
            }
        }

        if !buf.is_empty() {
            buf.extend_from_slice(RESET.as_bytes());
        }
        Diff {
            bytes: buf,
            full_redraw: false,
        }
    }

    /// Escape sequences drawing the whole grid, cursor at home first.
    pub fn render_full(&self) -> Diff {
        Diff::full_redraw(self)
    }
}

/// Cells addressed within [`Screen::diff`] are merged into one run when the
/// gap between them is at most this many columns.
pub const GAP: u16 = 4;

/// Escape sequences for a frame: either a delta or a whole-screen paint.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Diff {
    /// Bytes to write.
    pub bytes: Vec<u8>,
    /// Whether the bytes redraw the whole screen (cursor home and every row).
    pub full_redraw: bool,
}

impl Diff {
    /// Whether the frame carries nothing to write.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn full_redraw(screen: &Screen) -> Self {
        let mut buf = Vec::with_capacity(screen.cells.len() * 8 + 32);
        buf.extend_from_slice(HOME.as_bytes());
        let mut previous_cell: Option<StyleKey> = None;
        for y in 0..screen.height {
            if y > 0 {
                buf.extend_from_slice(CRLF.as_bytes());
            }
            for x in 0..screen.width {
                let cell = &screen.cells[y as usize * screen.width as usize + x as usize];
                // The wide glyph already advanced the cursor past its second
                // column; writing the continuation would blank it.
                if cell.is_continuation() {
                    continue;
                }
                write_cell(&mut buf, cell, &mut previous_cell);
            }
        }
        buf.extend_from_slice(RESET.as_bytes());
        Self {
            bytes: buf,
            full_redraw: true,
        }
    }
}

/// Colours and weight applied to the cells a [`Screen::put`] writes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellStyle {
    /// Foreground, `None` for the terminal default.
    pub fg: Option<(u8, u8, u8)>,
    /// Background, `None` for the terminal default.
    pub bg: Option<(u8, u8, u8)>,
    /// Bold attribute.
    pub bold: bool,
}

impl CellStyle {
    /// A style with only a foreground colour.
    pub fn fg(rgb: (u8, u8, u8)) -> Self {
        Self {
            fg: Some(rgb),
            ..Self::default()
        }
    }

    /// A style with foreground and background.
    pub fn both(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> Self {
        Self {
            fg: Some(fg),
            bg: Some(bg),
            bold: false,
        }
    }

    /// The same style, bolded.
    pub fn bolded(mut self) -> Self {
        self.bold = true;
        self
    }

    fn apply(&self, mut cell: Cell) -> Cell {
        cell.fg = self.fg;
        cell.bg = self.bg;
        cell.bold = self.bold;
        cell
    }
}

const HOME: &str = "\x1b[H";
const RESET: &str = "\x1b[0m";
const CRLF: &str = "\r\n";

fn write_goto(buf: &mut Vec<u8>, x: u16, y: u16) {
    // One-based, as every ANSI cursor address is.
    buf.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
}

/// The attributes [`write_cell`] has to re-emit when they change.
///
/// Held by value rather than as a `&Cell` so the diff loop owns no borrow of
/// the previous grid beyond the one it is reading.
type StyleKey = (Option<(u8, u8, u8)>, Option<(u8, u8, u8)>, bool);

fn write_cell(buf: &mut Vec<u8>, cell: &Cell, previous: &mut Option<StyleKey>) {
    let key: StyleKey = (cell.fg, cell.bg, cell.bold);
    let style_changed = *previous != Some(key);
    if style_changed {
        buf.extend_from_slice(RESET.as_bytes());
        if cell.bold {
            buf.extend_from_slice(b"\x1b[1m");
        }
        if let Some((r, g, b)) = cell.fg {
            buf.extend_from_slice(format!("\x1b[38;2;{r};{g};{b}m").as_bytes());
        }
        if let Some((r, g, b)) = cell.bg {
            buf.extend_from_slice(format!("\x1b[48;2;{r};{g};{b}m").as_bytes());
        }
        *previous = Some(key);
    }
    if cell.ch.is_empty() {
        // A blank cell still emits a space: the grid is dense and the cursor
        // has to advance past it.
        buf.push(b' ');
    } else {
        buf.extend_from_slice(cell.ch.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> CellStyle {
        CellStyle::default()
    }

    #[test]
    fn put_writes_cells_left_to_right() {
        let mut screen = Screen::new(10, 2);
        screen.put(2, 1, "abc", &plain());
        assert_eq!(screen.cell(2, 1).unwrap().ch, "a");
        assert_eq!(screen.cell(3, 1).unwrap().ch, "b");
        assert_eq!(screen.cell(4, 1).unwrap().ch, "c");
        assert!(screen.cell(5, 1).unwrap().is_blank());
    }

    #[test]
    fn put_clips_at_the_right_edge() {
        let mut screen = Screen::new(4, 1);
        screen.put(2, 0, "abcdef", &plain());
        assert_eq!(screen.cell(3, 0).unwrap().ch, "b");
        // No wrapping: the rest is dropped, never spilled onto the next row.
        assert_eq!(screen.height(), 1);
    }

    #[test]
    fn wide_glyph_claims_two_cells_and_is_never_split() {
        let mut screen = Screen::new(5, 1);
        screen.put(0, 0, "中x", &plain());
        assert_eq!(screen.cell(0, 0).unwrap().ch, "中");
        assert!(screen.cell(1, 0).unwrap().is_continuation());
        assert_eq!(screen.cell(2, 0).unwrap().ch, "x");

        // A wide glyph with only one column left is dropped, not split.
        let mut tight = Screen::new(3, 1);
        tight.put(2, 0, "中", &plain());
        assert!(tight.cell(2, 0).unwrap().is_blank());
    }

    #[test]
    fn tab_advances_to_the_next_eight_column_stop() {
        let mut screen = Screen::new(20, 1);
        screen.put(0, 0, "a\tb", &plain());
        assert_eq!(screen.cell(0, 0).unwrap().ch, "a");
        assert_eq!(screen.cell(8, 0).unwrap().ch, "b");
    }

    #[test]
    fn control_bytes_never_reach_a_cell() {
        let mut screen = Screen::new(8, 1);
        screen.put(0, 0, "a\u{1b}[31mb", &plain());
        let text: String = (0..8)
            .map(|x| screen.cell(x, 0).unwrap().ch.clone())
            .collect();
        assert!(
            !text.contains('\u{1b}'),
            "escape reached the grid: {text:?}"
        );
        assert!(text.starts_with("a[31mb"), "{text:?}");
    }

    #[test]
    fn put_right_ends_at_the_given_column() {
        let mut screen = Screen::new(10, 1);
        screen.put_right(10, 0, "abc", &plain());
        assert_eq!(screen.cell(7, 0).unwrap().ch, "a");
        assert_eq!(screen.cell(9, 0).unwrap().ch, "c");
    }

    #[test]
    fn diff_only_addresses_changed_cells() {
        let mut previous = Screen::new(10, 2);
        previous.put(0, 0, "old", &plain());
        let mut next = Screen::new(10, 2);
        next.put(0, 0, "new", &plain());

        let diff = next.diff(&previous);
        assert!(!diff.full_redraw);
        let text = String::from_utf8_lossy(&diff.bytes).into_owned();
        // "n" differs, "e" and "w" match "e"/"l"? — only the differing columns
        // are addressed, but the run is contiguous so one goto covers it.
        assert!(text.contains("\x1b[1;1H"), "{text:?}");
        assert!(text.ends_with("\x1b[0m"), "{text:?}");
        // The second row is untouched and must not be addressed.
        assert!(!text.contains("\x1b[2;"), "{text:?}");
    }

    #[test]
    fn identical_screens_produce_no_bytes() {
        let mut screen = Screen::new(6, 2);
        screen.put(0, 0, "stable", &plain());
        let diff = screen.diff(&screen.clone());
        assert!(
            diff.is_empty(),
            "{:?}",
            String::from_utf8_lossy(&diff.bytes)
        );
    }

    #[test]
    fn resize_forces_a_full_redraw() {
        let previous = Screen::new(4, 2);
        let next = Screen::new(6, 3);
        let diff = next.diff(&previous);
        assert!(diff.full_redraw);
        assert!(!diff.is_empty());
    }

    #[test]
    fn style_change_on_identical_text_is_still_a_diff() {
        let mut previous = Screen::new(4, 1);
        previous.put(0, 0, "same", &plain());
        let mut next = Screen::new(4, 1);
        next.put(0, 0, "same", &CellStyle::fg((1, 2, 3)));

        let diff = next.diff(&previous);
        assert!(!diff.is_empty(), "a recolour must repaint");
        let text = String::from_utf8_lossy(&diff.bytes).into_owned();
        assert!(text.contains("38;2;1;2;3"), "{text:?}");
    }

    #[test]
    fn continuation_cells_are_not_written_on_their_own() {
        let previous = Screen::new(6, 1);
        let mut next = Screen::new(6, 1);
        next.put(0, 0, "中", &plain());
        let diff = next.diff(&previous);
        let text = String::from_utf8_lossy(&diff.bytes).into_owned();
        // One goto for the glyph; the continuation column must not be
        // addressed separately.
        assert_eq!(text.matches("\x1b[1;").count(), 1, "{text:?}");
    }

    #[test]
    fn full_render_keeps_blank_columns() {
        // A gap between two words is a run of blank cells; dropping them
        // concatenates the words ("statewaits"), which is exactly the bug an
        // empty-`ch` continuation marker caused.
        let mut screen = Screen::new(12, 1);
        screen.put(0, 0, "state", &plain());
        screen.put(8, 0, "waits", &plain());
        let text = String::from_utf8_lossy(&screen.render_full().bytes).into_owned();
        assert!(text.contains("state"), "{text:?}");
        assert!(!text.contains("statewaits"), "blanks dropped: {text:?}");
        assert_eq!(text.matches(' ').count(), 3, "{text:?}");
    }

    #[test]
    fn diff_keeps_blank_columns_between_changed_runs() {
        let previous = Screen::new(12, 1);
        let mut next = Screen::new(12, 1);
        next.put(0, 0, "state", &plain());
        next.put(8, 0, "waits", &plain());
        let text = String::from_utf8_lossy(&next.diff(&previous).bytes).into_owned();
        assert!(!text.contains("statewaits"), "blanks dropped: {text:?}");
    }

    #[test]
    fn full_render_does_not_blank_a_wide_glyph() {
        let mut screen = Screen::new(6, 1);
        screen.put(0, 0, "中x", &plain());
        let text = String::from_utf8_lossy(&screen.render_full().bytes).into_owned();
        // The continuation column must not be written as a space, which would
        // leave a hole where the glyph's right half is.
        assert!(text.contains("中x"), "wide glyph was split: {text:?}");
    }

    #[test]
    fn diff_does_not_blank_a_wide_glyph() {
        let previous = Screen::new(6, 1);
        let mut next = Screen::new(6, 1);
        next.put(0, 0, "中x", &plain());
        let text = String::from_utf8_lossy(&next.diff(&previous).bytes).into_owned();
        // The glyph is written once, then the cursor jumps to the column
        // *after* its two cells: "x" lands on column 3, never on column 2
        // (which would overwrite the glyph's right half).
        assert!(text.contains("中"), "wide glyph missing: {text:?}");
        assert!(
            text.contains("\x1b[1;3Hx"),
            "cursor not past the glyph: {text:?}"
        );
        assert!(
            !text.contains("\x1b[1;2H"),
            "continuation was addressed: {text:?}"
        );
    }

    #[test]
    fn box_frame_draws_four_corners_and_rails() {
        let mut screen = Screen::new(10, 4);
        screen.box_frame(1, 0, 6, 4, &plain());
        assert_eq!(screen.cell(1, 0).unwrap().ch, "╭");
        assert_eq!(screen.cell(6, 0).unwrap().ch, "╮");
        assert_eq!(screen.cell(1, 3).unwrap().ch, "╰");
        assert_eq!(screen.cell(6, 3).unwrap().ch, "╯");
        assert_eq!(screen.cell(1, 1).unwrap().ch, "│");
        assert_eq!(screen.cell(3, 0).unwrap().ch, "─");
    }

    #[test]
    fn zero_geometry_stays_indexable() {
        let mut screen = Screen::new(0, 0);
        assert_eq!(screen.width(), 1);
        assert_eq!(screen.height(), 1);
        screen.put(0, 0, "x", &plain());
        assert_eq!(screen.cell(0, 0).unwrap().ch, "x");
    }
}
