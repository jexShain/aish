//! Cursor overlay for already-rendered editor rows.
//!
//! The editor produces rows that may contain SGR colour spans (a dim ghost
//! completion, a coloured prompt). Painting the cursor into such a row by byte
//! offset would be wrong twice over: the offset counts escape bytes as visible
//! columns, and a cut inside a sequence leaves the terminal in a corrupted
//! rendition state for every row afterwards. [`render_with_cursor`] therefore
//! walks the row as a small state machine — escape spans are copied verbatim,
//! printable characters advance a display-column counter — and only the
//! character that occupies the cursor column is wrapped in reverse video.
//!
//! Reverse video is entered with `ESC[7m` and left with `ESC[27m` rather than a
//! full `ESC[0m` reset, so a cursor inside a dim or coloured span keeps that
//! span's attributes on both sides. A zero-width character (combining mark,
//! variation selector) never consumes a column here, so it cannot swallow the
//! cursor.

use unicode_width::UnicodeWidthChar;

/// Enter reverse video.
pub const CURSOR_ON: &str = "\x1b[7m";
/// Leave reverse video only (keeps foreground/background/dim).
pub const CURSOR_OFF: &str = "\x1b[27m";
/// Dim rendition, used for ghost completion.
pub const DIM: &str = "\x1b[2m";
/// Reset all rendition (closes a dim span).
pub const RESET: &str = "\x1b[0m";

/// Paint a cursor cell into `lines[cursor_line]` at display column `cursor_col`.
///
/// If the column lies at or past the row's content (including on an empty row)
/// a reverse-video space is appended — unless that would take the row past
/// `width` columns, in which case the last glyph is reversed instead (the cell
/// a terminal holding a pending wrap shows the caret in). If `cursor_line` is
/// out of range, the cell is painted on the last line instead — the cursor is
/// always visible. Rows other than the target are returned unchanged.
pub fn render_with_cursor(
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: u16,
    width: u16,
) -> Vec<String> {
    if lines.is_empty() {
        return lines;
    }
    let in_range = cursor_line < lines.len();
    let target = if in_range {
        cursor_line
    } else {
        lines.len() - 1
    };
    let col = if in_range {
        cursor_col as usize
    } else {
        usize::MAX
    };

    let mut out = lines;
    out[target] = paint_line(&out[target], col, usize::from(width.max(1)));
    out
}

/// What a row scan found: the byte range of the glyph covering the caret
/// column (if the caret landed on one), the range of the last glyph that owns a
/// cell, and the row's total display width.
struct CursorScan {
    host: Option<(usize, usize)>,
    last_glyph: Option<(usize, usize)>,
    width: usize,
}

/// Walk `line`, recording the glyph covering display column `col`, the last
/// glyph that owns a cell, and the row's display width. Escape spans own no
/// columns.
fn scan_line(line: &str, col: usize) -> CursorScan {
    let mut display = 0usize;
    let mut host = None;
    let mut last_glyph = None;
    let mut idx = 0usize;
    while idx < line.len() {
        let ch = line[idx..].chars().next().expect("idx is a boundary");
        if ch == '\x1b' {
            idx += escape_len(&line[idx..]);
            continue;
        }
        let end = idx + ch.len_utf8();
        // A zero-width character (combining mark, control byte) owns no cell, so
        // it can neither host the cursor nor advance the column — reversing it
        // would dim nothing and could strand a control byte inside the caret.
        let width = if ch.is_control() {
            0
        } else {
            ch.width().unwrap_or(0)
        };
        if width > 0 {
            // The glyph covers [display, display + width). A cursor landing on a
            // wide glyph's trailing cell reverses the whole glyph.
            if host.is_none() && display <= col && col < display + width {
                host = Some((idx, end));
            }
            last_glyph = Some((idx, end));
        }
        display += width;
        idx = end;
    }
    CursorScan {
        host,
        last_glyph,
        width: display,
    }
}

/// Wrap the character at display column `col` in reverse video.
///
/// Past the last glyph a reverse-video space is appended, but only while the
/// row is narrower than `max_cells`: the caret must never take a row past the
/// terminal width, because the terminal would soft-wrap it (and, with autowrap
/// restored between frames, scroll the frame under the renderer). When the row
/// is already full, the final glyph is reversed instead — the cell a terminal
/// holding a pending wrap shows the caret in.
fn paint_line(line: &str, col: usize, max_cells: usize) -> String {
    let scan = scan_line(line, col);
    let range = match scan.host {
        Some(host) => Some(host),
        // Past the text: a caret cell only while the row has a free column;
        // otherwise the row is full and its last glyph takes the caret.
        None if scan.width < max_cells => None,
        None => scan.last_glyph,
    };

    let mut out = String::with_capacity(line.len() + CURSOR_ON.len() + CURSOR_OFF.len() + 1);
    match range {
        Some((start, end)) => {
            out.push_str(&line[..start]);
            out.push_str(CURSOR_ON);
            out.push_str(&line[start..end]);
            out.push_str(CURSOR_OFF);
            out.push_str(&line[end..]);
        }
        None => {
            out.push_str(line);
            out.push_str(CURSOR_ON);
            out.push(' ');
            out.push_str(CURSOR_OFF);
        }
    }
    out
}

/// Byte length of the escape span starting at `s` (which begins with ESC).
fn escape_len(s: &str) -> usize {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return bytes.len();
    }
    match bytes[1] {
        b'[' => {
            // CSI: parameter/intermediate bytes, then one final byte 0x40-0x7e.
            for (i, &b) in bytes.iter().enumerate().skip(2) {
                if (0x40..=0x7e).contains(&b) {
                    return i + 1;
                }
            }
            bytes.len()
        }
        b']' => {
            // OSC: terminated by BEL or ESC \.
            let mut i = 2;
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
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_escapes(s: &str) -> String {
        let mut out = String::new();
        let mut i = 0;
        while i < s.len() {
            let ch = s[i..].chars().next().unwrap();
            if ch == '\x1b' {
                i += escape_len(&s[i..]);
            } else {
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out
    }

    #[test]
    fn wraps_character_at_cursor() {
        let rows = render_with_cursor(vec!["abc".to_string()], 0, 1, 80);
        assert_eq!(rows[0], "a\x1b[7mb\x1b[27mc");
    }

    #[test]
    fn appends_space_past_end_of_line() {
        let rows = render_with_cursor(vec![String::new(), "hi".to_string()], 1, 2, 80);
        assert_eq!(rows[1], "hi\x1b[7m \x1b[27m");
    }

    #[test]
    fn preserves_ansi_spans_and_offsets_columns() {
        // "a" + red "bc": display columns are 0,1,2 — the cursor at column 2
        // must land on 'c', not on an escape byte.
        let line = "a\x1b[31mbc\x1b[0m";
        let rows = render_with_cursor(vec![line.to_string()], 0, 2, 80);
        assert_eq!(rows[0], "a\x1b[31mb\x1b[7mc\x1b[27m\x1b[0m");
        assert_eq!(strip_escapes(&rows[0]), "abc");
    }

    #[test]
    fn wide_char_is_reversed_whole() {
        // '世' occupies columns 0..2; a cursor on column 1 still selects it.
        let rows = render_with_cursor(vec!["世界".to_string()], 0, 1, 80);
        assert_eq!(rows[0], "\x1b[7m世\x1b[27m界");
    }

    #[test]
    fn cursor_line_out_of_range_uses_last_line() {
        let rows = render_with_cursor(vec!["one".to_string(), "two".to_string()], 9, 0, 80);
        assert_eq!(rows[1], "two\x1b[7m \x1b[27m");
    }

    #[test]
    fn full_row_reverses_last_glyph_instead_of_growing() {
        // "abcd" exactly fills a 4-column row: appending a reverse-video space
        // would put the row at 5 columns and the terminal would soft-wrap it.
        let rows = render_with_cursor(vec!["abcd".to_string()], 0, 4, 4);
        assert_eq!(rows[0], "abc\x1b[7md\x1b[27m");
        assert_eq!(strip_escapes(&rows[0]), "abcd");

        // A row one column short still gets its own caret cell.
        let rows = render_with_cursor(vec!["abc".to_string()], 0, 3, 4);
        assert_eq!(rows[0], "abc\x1b[7m \x1b[27m");
    }

    #[test]
    fn empty_row_keeps_the_caret_inside_the_width() {
        // An empty row at width 1 cannot host both the caret space and the
        // padding, so the row stays exactly one column wide.
        let rows = render_with_cursor(vec![String::new()], 0, 0, 1);
        assert_eq!(rows[0], "\x1b[7m \x1b[27m");
        assert_eq!(strip_escapes(&rows[0]).chars().count(), 1);
    }
}
