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
/// a reverse-video space is appended. If `cursor_line` is out of range, the
/// space is appended to the last line instead — the cursor is always visible.
/// Rows other than the target are returned unchanged.
pub fn render_with_cursor(lines: Vec<String>, cursor_line: usize, cursor_col: u16) -> Vec<String> {
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
    out[target] = paint_line(&out[target], col);
    out
}

/// Wrap the character at display column `col` in reverse video, or append a
/// reverse-video space when `col` is at/past the end.
fn paint_line(line: &str, col: usize) -> String {
    let mut out = String::with_capacity(line.len() + CURSOR_ON.len() + CURSOR_OFF.len());
    let mut display = 0usize;
    let mut idx = 0usize;

    while idx < line.len() {
        let ch = line[idx..].chars().next().expect("idx is a boundary");
        if ch == '\x1b' {
            let len = escape_len(&line[idx..]);
            out.push_str(&line[idx..idx + len]);
            idx += len;
            continue;
        }

        let cw = if ch.is_control() {
            0
        } else {
            ch.width().unwrap_or(0)
        };
        // A zero-width character (combining mark, control byte) owns no cell, so
        // it can neither host the cursor nor advance the column — reversing it
        // would dim nothing and could strand a control byte inside the caret.
        if cw == 0 {
            out.push(ch);
            idx += ch.len_utf8();
            continue;
        }
        // The character covers [display, display + cw). A cursor landing on a
        // wide glyph's trailing cell reverses the whole glyph.
        if display <= col && col < display + cw {
            out.push_str(CURSOR_ON);
            out.push(ch);
            out.push_str(CURSOR_OFF);
            out.push_str(&line[idx + ch.len_utf8()..]);
            return out;
        }
        out.push(ch);
        display += cw;
        idx += ch.len_utf8();
    }

    out.push_str(CURSOR_ON);
    out.push(' ');
    out.push_str(CURSOR_OFF);
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
        let rows = render_with_cursor(vec!["abc".to_string()], 0, 1);
        assert_eq!(rows[0], "a\x1b[7mb\x1b[27mc");
    }

    #[test]
    fn appends_space_past_end_of_line() {
        let rows = render_with_cursor(vec![String::new(), "hi".to_string()], 1, 2);
        assert_eq!(rows[1], "hi\x1b[7m \x1b[27m");
    }

    #[test]
    fn preserves_ansi_spans_and_offsets_columns() {
        // "a" + red "bc": display columns are 0,1,2 — the cursor at column 2
        // must land on 'c', not on an escape byte.
        let line = "a\x1b[31mbc\x1b[0m";
        let rows = render_with_cursor(vec![line.to_string()], 0, 2);
        assert_eq!(rows[0], "a\x1b[31mb\x1b[7mc\x1b[27m\x1b[0m");
        assert_eq!(strip_escapes(&rows[0]), "abc");
    }

    #[test]
    fn wide_char_is_reversed_whole() {
        // '世' occupies columns 0..2; a cursor on column 1 still selects it.
        let rows = render_with_cursor(vec!["世界".to_string()], 0, 1);
        assert_eq!(rows[0], "\x1b[7m世\x1b[27m界");
    }

    #[test]
    fn cursor_line_out_of_range_uses_last_line() {
        let rows = render_with_cursor(vec!["one".to_string(), "two".to_string()], 9, 0);
        assert_eq!(rows[1], "two\x1b[7m \x1b[27m");
    }
}
