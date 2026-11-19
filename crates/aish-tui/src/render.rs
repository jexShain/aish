//! Prepared-line construction: the only place that turns arbitrary product
//! strings into terminal-safe rows.
//!
//! A [`PreparedLine`] is a row that is *exactly* `width` columns or less after
//! normalization, so the renderer may overwrite a whole row without erasing and
//! without risking a wrap. Normalization is:
//!
//! 1. expand tabs to spaces (8-column stops relative to the row origin),
//! 2. drop C0 controls other than tab (they would move the cursor),
//! 3. keep ANSI escape sequences verbatim (they are zero-width),
//! 4. truncate on a display-column budget measured with `unicode-width`
//!    (CJK / emoji count as two columns; a wide glyph that does not fit in the
//!    last column is replaced by a space so the half cell is not left behind),
//! 5. trim trailing spaces (a trimmed row never repaints stale background).
//!
//! Scanning is a hand-written byte state machine, not a regex: a cut made on a
//! byte boundary would leave half an escape sequence on screen, which corrupts
//! every row painted afterwards.

use unicode_width::UnicodeWidthChar;

/// Column a tab advances to (multiples of this, counted from the row origin).
pub const TAB_WIDTH: usize = 8;

/// Reset all graphic rendition attributes.
const SGR_RESET: &str = "\x1b[0m";

/// One normalized row ready for the terminal.
///
/// `width` records the budget the row was prepared with, not its measured
/// content width: two rows with equal text but different budgets are still
/// worth repainting, because the budget decides where the next truncation
/// would land. The renderer compares [`PreparedLine::text`] for dirtiness and
/// uses `width` to notice a geometry change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreparedLine {
    /// Normalized text, escape sequences included, no trailing spaces.
    pub text: String,
    /// Column budget this row was fitted to.
    pub width: u16,
}

impl PreparedLine {
    /// Row whose text is already normalized and known to fit `width`.
    pub fn verbatim(text: impl Into<String>, width: u16) -> Self {
        Self {
            text: text.into(),
            width,
        }
    }

    /// Blank row of the given budget.
    pub fn blank(width: u16) -> Self {
        Self {
            text: String::new(),
            width,
        }
    }

    /// Text without escape sequences (tests and debugging).
    pub fn plain(&self) -> String {
        strip_escapes(&self.text)
    }
}

/// Visible column count of `s`, ignoring escape sequences, with tabs expanded
/// to [`TAB_WIDTH`] stops. Wide glyphs count their terminal width.
pub fn visible_width(s: &str) -> u16 {
    let mut scanner = Scanner::new(usize::MAX);
    scanner.run(s);
    scanner.column.min(u16::MAX as usize) as u16
}

/// Remove escape sequences, keeping visible characters.
pub fn strip_escapes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = escape_end(bytes, i);
            continue;
        }
        let ch = s[i..].chars().next().expect("valid utf-8 boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Normalize `raw` into a row occupying at most `width` columns.
pub fn prepare_line(raw: &str, width: u16) -> PreparedLine {
    let mut scanner = Scanner::new(width as usize);
    scanner.run(raw);
    PreparedLine {
        text: scanner.finish(),
        width,
    }
}

/// Prepare a whole frame channel.
pub fn prepare_lines<'a, I, S>(raw: I, width: u16) -> Vec<PreparedLine>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str> + 'a,
{
    raw.into_iter()
        .map(|line| prepare_line(line.as_ref(), width))
        .collect()
}

/// Byte index just past the escape sequence starting at `start` (which must
/// point at ESC). Malformed input degrades to consuming the lone ESC byte.
fn escape_end(bytes: &[u8], start: usize) -> usize {
    debug_assert_eq!(bytes[start], 0x1b);
    let Some(&kind) = bytes.get(start + 1) else {
        return start + 1;
    };
    match kind {
        b'[' => {
            // CSI: parameter bytes, optional intermediates, one final byte.
            let mut i = start + 2;
            while i < bytes.len() && (0x20..=0x3f).contains(&bytes[i]) {
                i += 1;
            }
            while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() && (0x40..=0x7e).contains(&bytes[i]) {
                i + 1
            } else {
                i
            }
        }
        b']' | b'P' | b'_' | b'^' | b'X' => {
            // OSC / DCS / APC / PM / SOS: string payload terminated by BEL or ST.
            let mut i = start + 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            bytes.len()
        }
        _ => {
            // Two-byte escape (ESC 7, ESC =, ...) after optional intermediates.
            let mut i = start + 1;
            while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                i += 1;
            }
            (i + 1).min(bytes.len())
        }
    }
}

/// A contiguous run of output: printable columns, or zero-width escapes.
enum Seg {
    Text(String),
    Spaces(usize),
}

/// Whether `text` paints at least one column (it may be nothing but escapes).
fn has_glyph(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i = escape_end(bytes, i);
            continue;
        }
        return true;
    }
    false
}

/// Fitting state machine shared by [`prepare_line`] and [`visible_width`].
struct Scanner {
    limit: usize,
    segs: Vec<Seg>,
    column: usize,
    truncated: bool,
    saw_escape: bool,
}

impl Scanner {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            segs: Vec::new(),
            column: 0,
            truncated: false,
            saw_escape: false,
        }
    }

    fn run(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b {
                let end = escape_end(bytes, i);
                self.saw_escape = true;
                if !self.truncated {
                    self.push_escape(&s[i..end]);
                }
                i = end;
                continue;
            }
            let ch = s[i..].chars().next().expect("valid utf-8 boundary");
            i += ch.len_utf8();
            match ch {
                '\t' => {
                    let stop = (self.column / TAB_WIDTH + 1) * TAB_WIDTH;
                    self.push_spaces(stop - self.column);
                }
                c if c.is_control() => {}
                c => {
                    let w = c.width().unwrap_or(0);
                    self.push_glyph(c, w);
                }
            }
            if self.truncated {
                // Everything after the cut is invisible: neither glyphs nor
                // escapes can still reach the output.
                break;
            }
        }
    }

    fn push_escape(&mut self, esc: &str) {
        match self.segs.last_mut() {
            Some(Seg::Text(text)) => text.push_str(esc),
            _ => self.segs.push(Seg::Text(esc.to_string())),
        }
    }

    fn push_spaces(&mut self, count: usize) {
        let n = count.min(self.limit - self.column);
        if n > 0 {
            match self.segs.last_mut() {
                Some(Seg::Spaces(prev)) => *prev += n,
                _ => self.segs.push(Seg::Spaces(n)),
            }
            self.column += n;
        }
        if n < count {
            self.truncated = true;
        }
    }

    fn push_glyph(&mut self, ch: char, w: usize) {
        if ch == ' ' {
            // Spaces are their own segment so a trailing run can be dropped
            // from the joined output in one step.
            self.push_spaces(1);
            return;
        }
        if self.column + w > self.limit {
            self.truncated = true;
            // A wide glyph cut in half leaves a stale cell (and some terminals
            // paint it anyway, pushing the row past the budget). Fill the
            // remaining column with a space instead; it is load-bearing, so it
            // must survive trailing-space trimming.
            if self.column < self.limit {
                self.segs.push(Seg::Text(" ".to_string()));
                self.column += 1;
            }
            return;
        }
        match self.segs.last_mut() {
            Some(Seg::Text(text)) => text.push(ch),
            _ => self.segs.push(Seg::Text(ch.to_string())),
        }
        self.column += w;
    }

    fn finish(mut self) -> String {
        // Trailing space runs are dropped, but escape sequences after them are
        // kept: a row that ends with a reset must still close its attributes.
        // Space between the last visible glyph and a trailing escape goes too,
        // so `"\x1b[32mok  \x1b[0m"` becomes `"\x1b[32mok\x1b[0m"`.
        let last_visible = self
            .segs
            .iter()
            .rposition(|seg| matches!(seg, Seg::Text(text) if has_glyph(text)));
        let mut kept = Vec::with_capacity(self.segs.len());
        for (index, seg) in self.segs.drain(..).enumerate() {
            let drop_space =
                matches!(seg, Seg::Spaces(_)) && last_visible.is_none_or(|last| index > last);
            if !drop_space {
                kept.push(seg);
            }
        }
        self.segs = kept;

        let mut out = String::with_capacity(self.column + 8);
        for seg in self.segs {
            match seg {
                Seg::Text(text) => out.push_str(&text),
                Seg::Spaces(n) => out.extend(std::iter::repeat_n(' ', n)),
            }
        }
        if self.truncated && self.saw_escape {
            // The cut may have landed inside a colored run; make sure the
            // attributes cannot bleed into the next row.
            out.push_str(SGR_RESET);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_glyph_truncation_keeps_budget() {
        // "ab" + 2-wide CJK + 2-wide CJK = 6 columns; budget 5 cuts the second
        // glyph and fills its half cell with a space.
        let line = prepare_line("ab漢字", 5);
        assert_eq!(line.text, "ab漢 ");
        assert_eq!(line.width, 5);
        assert_eq!(visible_width(&line.text), 5);
    }

    #[test]
    fn prepare_line_respects_cjk_width() {
        // Four 2-wide glyphs fit a 8-column budget exactly; a 7-column budget
        // cuts the last one and leaves a load-bearing space in its half cell.
        assert_eq!(prepare_line("漢字漢字", 8).text, "漢字漢字");
        assert_eq!(visible_width(&prepare_line("漢字漢字", 8).text), 8);
        assert_eq!(prepare_line("漢字漢字", 7).text, "漢字漢 ");
        assert_eq!(visible_width(&prepare_line("漢字漢字", 7).text), 7);
    }

    #[test]
    fn prepare_line_keeps_ansi_intact() {
        // A styled row truncated mid-run must keep both sequences byte-exact
        // and close with a reset, so the attributes cannot bleed into the next
        // row.
        let line = prepare_line("[1;31mabcdef[0m", 4);
        assert_eq!(line.text, "[1;31mabcd[0m");
        assert_eq!(strip_escapes(&line.text), "abcd");

        // A cut inside the colored run adds the reset itself.
        let cut = prepare_line("[1;31mabcdef", 4);
        assert_eq!(cut.text, "[1;31mabcd[0m");
    }

    #[test]
    fn wide_glyph_at_exact_boundary_is_kept() {
        assert_eq!(prepare_line("漢字", 4).text, "漢字");
    }

    #[test]
    fn ansi_sequences_survive_truncation() {
        let line = prepare_line("\x1b[31mabcdef\x1b[0m", 3);
        assert_eq!(line.text, "\x1b[31mabc\x1b[0m");
        assert_eq!(strip_escapes(&line.text), "abc");
    }

    #[test]
    fn ansi_is_never_split_by_a_cut() {
        // A 2-wide glyph sits exactly at the cut point, followed by an OSC 8
        // hyperlink pair: no partial sequence may reach the output.
        let raw = "a漢\x1b]8;;https://example.com\x07link\x1b]8;;\x07";
        let line = prepare_line(raw, 2);
        assert_eq!(line.text, "a ");
        assert!(
            !line.text.contains('\x1b'),
            "escape leaked: {:?}",
            line.text
        );
    }

    #[test]
    fn trailing_spaces_are_trimmed() {
        assert_eq!(prepare_line("abc   ", 20).text, "abc");
        assert_eq!(visible_width("abc   "), 6);
        // Trimming keeps a trailing reset so attributes still close.
        let styled = prepare_line("\x1b[32mok  \x1b[0m", 20);
        assert_eq!(styled.text, "\x1b[32mok\x1b[0m");
    }

    #[test]
    fn tabs_expand_to_eight_column_stops() {
        assert_eq!(prepare_line("\tx", 20).text, "        x");
        assert_eq!(prepare_line("ab\tcd", 20).text, "ab      cd");
        // Expansion stops at the budget: the four padding spaces fill the row,
        // `x` is cut, and the blank tail is trimmed away.
        assert_eq!(prepare_line("\tx", 4).text, "");
        assert_eq!(prepare_line("\tx", 9).text, "        x");
    }

    #[test]
    fn controls_are_dropped() {
        assert_eq!(prepare_line("a\x07b\x08c\rd", 20).text, "abcd");
    }

    #[test]
    fn visible_width_ignores_escapes() {
        assert_eq!(visible_width("\x1b[1;31mred\x1b[0m"), 3);
        assert_eq!(visible_width("漢字"), 4);
        assert_eq!(visible_width(""), 0);
    }
}
