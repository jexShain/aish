//! Raw stdin → [`InputEvent`] reassembly.
//!
//! The terminal is free to split a key sequence across any byte boundary: one
//! `read()` may end in the middle of a CSI parameter run, inside a bracketed
//! paste, or inside a multi-byte UTF-8 scalar. A parser that decodes each chunk
//! independently has to guess, and guesses are wrong exactly when it matters —
//! a paste arriving in 4 KiB pieces, a CJK character split by a pty buffer
//! boundary. This parser buffers the incomplete tail and resumes on the next
//! [`InputParser::feed`], so every event other than [`InputEvent::Unknown`]
//! corresponds to bytes the terminal sent as one logical unit.
//!
//! # State machine
//!
//! * Bare printable bytes → UTF-8 decoded, re-buffered while incomplete.
//! * C0 control bytes → control keys. They cannot introduce a longer sequence,
//!   so they are never held back.
//! * `ESC` alone is ambiguous: it is both the Escape key and the introducer of
//!   CSI / SS3 / paste markers. It stays buffered until the next feed settles
//!   it; [`InputParser::flush`] (the caller's key-timeout path) emits it as
//!   [`Key::Esc`].
//! * `ESC [ 200 ~ … ESC [ 201 ~` → [`InputEvent::Paste`], payload verbatim
//!   (newlines, tabs and stray ESC included): a paste is data, not keystrokes.
//!
//! # Bounded buffering
//!
//! A lost paste terminator must not buffer forever, so the payload is capped;
//! overrunning the cap drops the paste and swallows the tail up to the next end
//! marker rather than replaying megabytes of blob as keystrokes. A paste that
//! simply stops growing is abandoned by
//! [`InputParser::flush_paste_deadline`] after one second.

use std::time::{Duration, Instant};

/// Opening marker of a bracketed paste.
const PASTE_START: &[u8] = b"\x1b[200~";
/// Closing marker of a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";
/// Maximum buffered paste payload before the paste is dropped.
const MAX_PASTE_BYTES: usize = 1024 * 1024;
/// How long a paste may sit without new bytes before it is abandoned.
pub const PASTE_INACTIVITY: Duration = Duration::from_secs(1);
/// How long a lone ESC may sit before it is committed as the Escape key.
///
/// A terminal sends Alt+<char> as ESC + char, so an ESC cannot be emitted
/// immediately; but waiting forever made the next keystroke (600 ms later, far
/// past any terminal sequence) be swallowed as `Alt(ch)`, which the editor does
/// not bind. 50 ms is below human typing speed and above any real sequence.
pub const ESCAPE_INACTIVITY: Duration = Duration::from_millis(50);
/// A CSI parameter run longer than this is not a key this parser understands.
const MAX_CSI_BYTES: usize = 64;

/// A recognised non-printable key.
///
/// Control chords are folded rather than enumerated: `Ctrl('A')` .. `Ctrl('Z')`
/// name the emacs bindings (a terminal sends them as C0 bytes), and the
/// punctuation chords a terminal can still emit (`Ctrl('\\')`, `Ctrl(']')`,
/// `Ctrl('^')`, `Ctrl('_')`, NUL as `Ctrl('@')`) use the same variant.
///
/// `ShiftEnter` and `AltEnter` exist because they are the multi-line newline
/// bindings: they arrive only from terminals that report modifiers (kitty
/// protocol `ESC [ 13 ; 2 u` or the legacy `ESC CR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Enter,
    /// Shift+Enter (kitty/CSI-u terminals only).
    ShiftEnter,
    /// Alt+Enter (legacy `ESC CR`).
    AltEnter,
    Esc,
    Backspace,
    Tab,
    BackTab,
    Delete,
    Insert,
    Home,
    End,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    /// Ctrl+Up.
    CtrlUp,
    /// Ctrl+Down.
    CtrlDown,
    /// Ctrl+Left (word-wise motion).
    CtrlLeft,
    /// Ctrl+Right (word-wise motion).
    CtrlRight,
    /// Ctrl-<letter>, stored uppercased.
    Ctrl(char),
    /// Alt-<char>.
    Alt(char),
}

/// One logical input unit recovered from the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A printable character, already UTF-8 decoded.
    Char(char),
    /// A recognised key, including control chords.
    Key(Key),
    /// A complete bracketed-paste payload, verbatim.
    Paste(String),
    /// Bytes that form no recognised sequence. Emitted rather than dropped so a
    /// caller can log them; the editor never types them.
    Unknown(Vec<u8>),
}

/// How the head of the buffer should be consumed.
enum Step {
    /// One decoded event and the number of bytes it consumed.
    Emit(InputEvent, usize),
    /// The buffer starts a paste; `Some(end)` is the absolute offset of the
    /// closing marker when the paste is complete.
    Paste(Option<usize>),
    /// Need more bytes before a decision is possible.
    Incomplete,
    /// These bytes are garbage; consume them and continue.
    Junk(usize),
}

/// Reassembles raw stdin chunks into [`InputEvent`]s.
#[derive(Debug)]
pub struct InputParser {
    buffer: Vec<u8>,
    /// When the current paste payload last grew; `None` outside a paste.
    paste_touched: Option<Instant>,
    /// Set after a paste was dropped for exceeding the cap: swallow everything
    /// up to the next end marker without emitting.
    discard_paste_tail: bool,
}

impl Default for InputParser {
    fn default() -> Self {
        Self::new()
    }
}

impl InputParser {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            paste_touched: None,
            discard_paste_tail: false,
        }
    }

    /// Bytes held back because they may head a longer sequence.
    pub fn pending(&self) -> &[u8] {
        &self.buffer
    }

    /// Whether an unterminated bracketed paste is in flight.
    pub fn in_paste(&self) -> bool {
        self.paste_touched.is_some()
    }

    /// Feed one chunk and return every event it completed.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<InputEvent> {
        self.buffer.extend_from_slice(bytes);
        self.parse(Instant::now())
    }

    /// Settle ambiguous trailing bytes as literal input.
    ///
    /// Callers run this on a key timeout, which is what makes a bare Escape key
    /// usable at all. An unterminated paste is discarded for the same reason
    /// [`flush_paste_deadline`](Self::flush_paste_deadline) drops it: truncated
    /// paste data must never reach the editor as if it were typed.
    pub fn flush(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        while !self.buffer.is_empty() {
            match self.step(true) {
                Step::Emit(event, len) => {
                    self.buffer.drain(..len);
                    out.push(event);
                }
                Step::Paste(Some(end)) => out.push(InputEvent::Paste(self.take_paste(end))),
                Step::Paste(None) => {
                    // Truncated paste: drop it (see `flush_paste_deadline`).
                    self.buffer.clear();
                    self.paste_touched = None;
                }
                Step::Incomplete => {
                    out.push(InputEvent::Unknown(std::mem::take(&mut self.buffer)));
                }
                Step::Junk(len) => {
                    out.push(InputEvent::Unknown(self.buffer.drain(..len).collect()));
                }
            }
        }
        out
    }

    /// Drop a paste that stopped growing; returns whether one was discarded.
    ///
    /// A paste whose end marker never arrives is truncated data (the terminal
    /// or a multiplexer lost the tail), so the payload is thrown away rather
    /// than delivered as if it were complete — half a pasted command silently
    /// executed is worse than a paste the user has to repeat.
    pub fn flush_paste_deadline(&mut self, now: Instant) -> bool {
        let Some(touched) = self.paste_touched else {
            return false;
        };
        if now.duration_since(touched) < PASTE_INACTIVITY {
            return false;
        }
        self.buffer.clear();
        self.paste_touched = None;
        true
    }

    fn parse(&mut self, now: Instant) -> Vec<InputEvent> {
        let mut out = Vec::new();
        loop {
            match self.step(false) {
                Step::Emit(event, len) => {
                    self.buffer.drain(..len);
                    out.push(event);
                }
                Step::Paste(Some(end)) => out.push(InputEvent::Paste(self.take_paste(end))),
                Step::Paste(None) => {
                    if self.buffer.len() > MAX_PASTE_BYTES {
                        self.buffer.clear();
                        self.paste_touched = None;
                        self.discard_paste_tail = true;
                    } else {
                        self.paste_touched = Some(now);
                    }
                    break;
                }
                Step::Incomplete => break,
                Step::Junk(len) => {
                    out.push(InputEvent::Unknown(self.buffer.drain(..len).collect()));
                }
            }
        }
        out
    }

    /// Cut a complete paste out of the front of the buffer, leaving anything
    /// after the end marker in place. `payload_end` is the absolute offset of
    /// the closing marker.
    fn take_paste(&mut self, payload_end: usize) -> String {
        let payload =
            String::from_utf8_lossy(&self.buffer[PASTE_START.len()..payload_end]).into_owned();
        self.buffer.drain(..payload_end + PASTE_END.len());
        self.paste_touched = None;
        payload
    }

    /// Classify the head of the buffer.
    ///
    /// `forced` is the key-timeout path: an ambiguous prefix becomes its most
    /// likely literal reading instead of staying pending.
    fn step(&mut self, forced: bool) -> Step {
        if self.buffer.is_empty() {
            return Step::Incomplete;
        }

        // Tail of an abandoned paste: swallow to the end marker so a lost
        // terminator cannot replay payload bytes as input.
        if self.discard_paste_tail {
            match find_subslice(&self.buffer, PASTE_END) {
                Some(pos) => {
                    self.buffer.drain(..pos + PASTE_END.len());
                    self.discard_paste_tail = false;
                    // The bytes after the consumed tail are ordinary input;
                    // classify them in the same call.
                    return self.step(forced);
                }
                None => {
                    self.buffer.clear();
                    return Step::Incomplete;
                }
            }
        }

        let first = self.buffer[0];
        if first == 0x1b {
            return self.step_escape(forced);
        }

        match first {
            0x00 => return Step::Emit(InputEvent::Key(Key::Ctrl('@')), 1),
            b'\r' | b'\n' => return Step::Emit(InputEvent::Key(Key::Enter), 1),
            0x7f => return Step::Emit(InputEvent::Key(Key::Backspace), 1),
            0x09 => return Step::Emit(InputEvent::Key(Key::Tab), 1),
            // 0x08 is BS; Ctrl-H is indistinguishable from Backspace.
            0x08 => return Step::Emit(InputEvent::Key(Key::Backspace), 1),
            0x01..=0x07 | 0x0b..=0x1a => {
                let letter = (b'a' + (first - 1)) as char;
                return Step::Emit(InputEvent::Key(Key::Ctrl(letter.to_ascii_uppercase())), 1);
            }
            0x1c => return Step::Emit(InputEvent::Key(Key::Ctrl('\\')), 1),
            0x1d => return Step::Emit(InputEvent::Key(Key::Ctrl(']')), 1),
            0x1e => return Step::Emit(InputEvent::Key(Key::Ctrl('^')), 1),
            0x1f => return Step::Emit(InputEvent::Key(Key::Ctrl('_')), 1),
            _ => {}
        }

        // UTF-8 scalar. `from_utf8` reports both the character and whether the
        // tail is merely incomplete.
        match std::str::from_utf8(&self.buffer) {
            Ok(text) => {
                let ch = text.chars().next().unwrap_or('\u{fffd}');
                Step::Emit(InputEvent::Char(ch), ch.len_utf8())
            }
            Err(err) => {
                if err.error_len().is_some() {
                    Step::Junk(1)
                } else if err.valid_up_to() == 0 {
                    Step::Incomplete
                } else {
                    let ch = std::str::from_utf8(&self.buffer[..err.valid_up_to()])
                        .ok()
                        .and_then(|t| t.chars().next())
                        .unwrap_or('\u{fffd}');
                    Step::Emit(InputEvent::Char(ch), ch.len_utf8())
                }
            }
        }
    }

    fn step_escape(&mut self, forced: bool) -> Step {
        debug_assert_eq!(self.buffer[0], 0x1b);
        if self.buffer.len() == 1 {
            return if forced {
                Step::Emit(InputEvent::Key(Key::Esc), 1)
            } else {
                Step::Incomplete
            };
        }

        match self.buffer[1] {
            b'[' => self.step_csi(forced),
            b'O' => self.step_ss3(forced),
            // ESC ESC: the first is the Escape key, the second opens a sequence.
            0x1b => Step::Emit(InputEvent::Key(Key::Esc), 1),
            b'\r' | b'\n' => Step::Emit(InputEvent::Key(Key::AltEnter), 2),
            0x20..=0x7e => {
                let ch = self.buffer[1] as char;
                Step::Emit(InputEvent::Key(Key::Alt(ch)), 2)
            }
            _ => Step::Emit(InputEvent::Unknown(vec![0x1b]), 1),
        }
    }

    /// SS3 keypad: `ESC O <final>`.
    fn step_ss3(&mut self, forced: bool) -> Step {
        if self.buffer.len() < 3 {
            return if forced {
                Step::Emit(InputEvent::Unknown(std::mem::take(&mut self.buffer)), 0)
            } else {
                Step::Incomplete
            };
        }
        let key = match self.buffer[2] {
            b'A' => Some(Key::Up),
            b'B' => Some(Key::Down),
            b'C' => Some(Key::Right),
            b'D' => Some(Key::Left),
            b'H' => Some(Key::Home),
            b'F' => Some(Key::End),
            b'M' => Some(Key::Enter),
            _ => None,
        };
        match key {
            Some(key) => Step::Emit(InputEvent::Key(key), 3),
            None => Step::Emit(InputEvent::Unknown(self.buffer[..3].to_vec()), 3),
        }
    }

    fn step_csi(&mut self, forced: bool) -> Step {
        // Bracketed paste open marker may be split; wait until it either matches
        // fully or diverges.
        if let Some(len) = prefix_match(&self.buffer, PASTE_START) {
            if len == PASTE_START.len() {
                let rest = &self.buffer[PASTE_START.len()..];
                return match find_subslice(rest, PASTE_END) {
                    Some(pos) => Step::Paste(Some(PASTE_START.len() + pos)),
                    None => Step::Paste(None),
                };
            }
            return if forced {
                Step::Emit(InputEvent::Unknown(std::mem::take(&mut self.buffer)), 0)
            } else {
                Step::Incomplete
            };
        }
        // A stray end marker (its opener was lost) is not input.
        if let Some(len) = prefix_match(&self.buffer, PASTE_END) {
            if len == PASTE_END.len() {
                return Step::Junk(PASTE_END.len());
            }
            return if forced {
                Step::Emit(InputEvent::Unknown(std::mem::take(&mut self.buffer)), 0)
            } else {
                Step::Incomplete
            };
        }

        // Generic CSI: ESC [ <params 0x30-0x3f> <intermediates 0x20-0x2f>
        // <final 0x40-0x7e>.
        let mut i = 2usize;
        while i < self.buffer.len() {
            let b = self.buffer[i];
            if (0x40..=0x7e).contains(&b) {
                let seq = self.buffer[..=i].to_vec();
                return match decode_csi(&seq) {
                    Some(event) => Step::Emit(event, i + 1),
                    None => Step::Emit(InputEvent::Unknown(seq), i + 1),
                };
            }
            if b < 0x20 || b == 0x7f {
                // A control byte inside a CSI means the sequence is malformed;
                // consume only the ESC so the byte is parsed on its own.
                return Step::Emit(InputEvent::Unknown(vec![0x1b]), 1);
            }
            if i >= MAX_CSI_BYTES {
                return Step::Emit(InputEvent::Unknown(self.buffer[..=i].to_vec()), i + 1);
            }
            i += 1;
        }

        if forced {
            Step::Emit(InputEvent::Unknown(std::mem::take(&mut self.buffer)), 0)
        } else {
            Step::Incomplete
        }
    }
}

/// Decode a complete CSI sequence (`ESC [ … <final>`).
fn decode_csi(seq: &[u8]) -> Option<InputEvent> {
    debug_assert!(seq.len() >= 3 && seq[0] == 0x1b && seq[1] == b'[');
    let params = std::str::from_utf8(&seq[2..seq.len() - 1]).ok()?;
    let final_byte = *seq.last()?;

    // Modifier encoding is `1 + bitmask`: 2 = Shift, 3 = Alt, 5 = Ctrl,
    // 7 = Ctrl+Alt.
    let modifier = modifier_of(params);
    let ctrl = modifier & 0b100 != 0;
    let alt = modifier & 0b010 != 0;
    let shift = modifier & 0b001 != 0;

    match final_byte {
        b'A' => Some(InputEvent::Key(if ctrl { Key::CtrlUp } else { Key::Up })),
        b'B' => Some(InputEvent::Key(if ctrl {
            Key::CtrlDown
        } else {
            Key::Down
        })),
        b'C' => Some(InputEvent::Key(if ctrl {
            Key::CtrlRight
        } else {
            Key::Right
        })),
        b'D' => Some(InputEvent::Key(if ctrl {
            Key::CtrlLeft
        } else {
            Key::Left
        })),
        b'H' => Some(InputEvent::Key(Key::Home)),
        b'F' => Some(InputEvent::Key(Key::End)),
        b'Z' => Some(InputEvent::Key(Key::BackTab)),
        b'M' => Some(InputEvent::Key(Key::Enter)),
        b'~' => decode_csi_tilde(params),
        b'u' => decode_csi_u(params, ctrl, alt, shift),
        _ => None,
    }
}

/// Modifier bitmask from CSI params (`""` → 0, `"1;5"` → 4).
fn modifier_of(params: &str) -> u8 {
    params
        .rsplit(';')
        .next()
        .and_then(|last| last.parse::<u8>().ok())
        .filter(|&m| m > 1)
        .map(|m| m - 1)
        .unwrap_or(0)
}

/// `ESC [ <n> ~` and `ESC [ <n> ; <mod> ~` (xterm keypad / editing keys).
fn decode_csi_tilde(params: &str) -> Option<InputEvent> {
    let code: u32 = params.split(';').next()?.parse().ok()?;
    let key = match code {
        1 | 7 => Key::Home,
        2 => Key::Insert,
        3 => Key::Delete,
        4 | 8 => Key::End,
        5 => Key::PageUp,
        6 => Key::PageDown,
        _ => return None,
    };
    Some(InputEvent::Key(key))
}

/// `ESC [ <codepoint> [; <modifier>] u` (kitty / CSI-u).
fn decode_csi_u(params: &str, ctrl: bool, alt: bool, shift: bool) -> Option<InputEvent> {
    let codepoint: u32 = params.split(';').next()?.parse().ok()?;
    let ch = char::from_u32(codepoint)?;

    if ctrl {
        let upper = ch.to_ascii_uppercase();
        if upper.is_ascii_alphabetic() {
            return Some(InputEvent::Key(Key::Ctrl(upper)));
        }
    }
    match ch {
        '\r' | '\n' if shift => Some(InputEvent::Key(Key::ShiftEnter)),
        '\r' | '\n' if alt => Some(InputEvent::Key(Key::AltEnter)),
        '\r' | '\n' => Some(InputEvent::Key(Key::Enter)),
        '\t' if shift => Some(InputEvent::Key(Key::BackTab)),
        '\t' => Some(InputEvent::Key(Key::Tab)),
        '\x7f' => Some(InputEvent::Key(Key::Backspace)),
        '\x1b' => Some(InputEvent::Key(Key::Esc)),
        _ if alt => Some(InputEvent::Key(Key::Alt(ch))),
        _ => Some(InputEvent::Char(ch)),
    }
}

/// Length of the common prefix when `buf` starts with a prefix of `marker`;
/// `Some(marker.len())` on a full match.
fn prefix_match(buf: &[u8], marker: &[u8]) -> Option<usize> {
    let n = buf.len().min(marker.len());
    (buf[..n] == marker[..n]).then_some(n)
}

/// Offset of the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(parser: &mut InputParser, chunks: &[&[u8]]) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(parser.feed(chunk));
        }
        out
    }

    #[test]
    fn input_parser_reassembles_split_csi() {
        let mut parser = InputParser::new();
        // Up arrow split three ways, then a printable key: nothing may be
        // emitted until the final byte arrives, and the 'x' must not be eaten.
        let events = feed_all(&mut parser, &[b"\x1b", b"[", b"Ax"]);
        assert_eq!(
            events,
            vec![InputEvent::Key(Key::Up), InputEvent::Char('x')]
        );
        assert!(parser.pending().is_empty());

        // Split inside a `~` sequence too.
        let events = feed_all(&mut parser, &[b"\x1b[3", b"~"]);
        assert_eq!(events, vec![InputEvent::Key(Key::Delete)]);
    }

    #[test]
    fn input_parser_bracketed_paste_keeps_newlines() {
        let mut parser = InputParser::new();
        // Payload contains LF, CRLF, a tab and an embedded ESC, all of which
        // must survive byte-for-byte: a paste is data, not keystrokes.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"\x1b[200~line1\nline2\r\n\tcol\x1b[31mred");
        raw.extend_from_slice(b"\x1b[201~");
        let events = feed_all(&mut parser, &[&raw]);
        assert_eq!(
            events,
            vec![InputEvent::Paste(
                "line1\nline2\r\n\tcol\u{1b}[31mred".to_string()
            )]
        );

        // Same paste split across chunks at an arbitrary boundary, with a
        // following keystroke that must not have been swallowed.
        let mut parser = InputParser::new();
        let mut raw = Vec::new();
        raw.extend_from_slice(b"\x1b[200~a\nb\x1b[201~z");
        let (head, tail) = raw.split_at(9);
        let mut events = feed_all(&mut parser, &[head, tail]);
        assert_eq!(events.pop(), Some(InputEvent::Char('z')));
        assert_eq!(events, vec![InputEvent::Paste("a\nb".to_string())]);
    }

    #[test]
    fn input_parser_utf8_split_across_chunks() {
        let mut parser = InputParser::new();
        // '世' is 3 bytes, 'é' is 2; neither may become U+FFFD.
        let events = feed_all(&mut parser, &[b"\xe4", b"\xb8", b"\x96caf\xc3", b"\xa9"]);
        assert_eq!(
            events,
            vec![
                InputEvent::Char('世'),
                InputEvent::Char('c'),
                InputEvent::Char('a'),
                InputEvent::Char('f'),
                InputEvent::Char('é'),
            ]
        );
    }

    #[test]
    fn arrow_keys_parse_from_csi_and_ss3() {
        let mut parser = InputParser::new();
        let events = feed_all(&mut parser, &[b"\x1b[A\x1bOB\x1b[C\x1bOD"]);
        assert_eq!(
            events,
            vec![
                InputEvent::Key(Key::Up),
                InputEvent::Key(Key::Down),
                InputEvent::Key(Key::Right),
                InputEvent::Key(Key::Left),
            ]
        );
    }

    #[test]
    fn control_and_modifier_keys() {
        let mut parser = InputParser::new();
        // Ctrl-A, Ctrl-W, CR, Alt-x, Ctrl+Left, Shift+Enter (CSI-u).
        let events = feed_all(&mut parser, &[b"\x01\x17\r\x1bx\x1b[1;5D\x1b[13;2u"]);
        assert_eq!(
            events,
            vec![
                InputEvent::Key(Key::Ctrl('A')),
                InputEvent::Key(Key::Ctrl('W')),
                InputEvent::Key(Key::Enter),
                InputEvent::Key(Key::Alt('x')),
                InputEvent::Key(Key::CtrlLeft),
                InputEvent::Key(Key::ShiftEnter),
            ]
        );
    }

    #[test]
    fn bare_escape_needs_flush() {
        let mut parser = InputParser::new();
        assert!(parser.feed(b"\x1b").is_empty());
        assert_eq!(parser.pending(), b"\x1b");
        assert_eq!(parser.flush(), vec![InputEvent::Key(Key::Esc)]);
        assert!(parser.pending().is_empty());
    }

    #[test]
    fn runaway_paste_is_bounded_and_swallowed() {
        let mut parser = InputParser::new();
        parser.feed(b"\x1b[200~");
        // 32 x 64 KiB = 2 MiB, past the 1 MiB cap: the paste is dropped rather
        // than buffered forever, and nothing is replayed as keystrokes.
        let chunk = vec![b'a'; 64 * 1024];
        for _ in 0..32 {
            assert!(parser.feed(&chunk).is_empty());
        }
        assert!(!parser.in_paste());
        assert!(parser.pending().len() < MAX_PASTE_BYTES);

        // The tail up to the end marker is swallowed; input after it resumes.
        let events = parser.feed(b"tail\x1b[201~k");
        assert_eq!(events, vec![InputEvent::Char('k')]);
        assert!(parser.pending().is_empty());
    }

    #[test]
    fn stalled_paste_is_abandoned_on_deadline() {
        let mut parser = InputParser::new();
        parser.feed(b"\x1b[200~half");
        assert!(parser.in_paste());
        // Inside the deadline nothing is dropped.
        assert!(!parser.flush_paste_deadline(Instant::now()));
        assert!(parser.in_paste());

        // Past it, the truncated payload is discarded and buffering resumes with
        // the next real keypress.
        let later = Instant::now() + PASTE_INACTIVITY + Duration::from_millis(1);
        assert!(parser.flush_paste_deadline(later));
        assert!(!parser.in_paste());
        assert!(parser.pending().is_empty());
        assert_eq!(parser.feed(b"k"), vec![InputEvent::Char('k')]);

        // A forced flush of an unterminated paste drops it too.
        let mut parser = InputParser::new();
        parser.feed(b"\x1b[200~half");
        assert!(parser.flush().is_empty());
        assert!(!parser.in_paste());
    }
}
