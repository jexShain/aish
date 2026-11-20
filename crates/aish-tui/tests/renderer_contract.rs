//! Renderer contract tests: history acking, viewport diffing, paint framing.
//!
//! Every test drives a real [`Renderer`] over a shared byte sink, so the
//! assertions are about the exact bytes a terminal would receive.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use aish_tui::render::prepare_lines;
use aish_tui::{Frame, HistoryBatch, PreparedLine, Renderer, TerminalCaps};

/// Byte sink whose contents the test can inspect while the renderer owns it.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Sink {
    /// Bytes written so far, without consuming them.
    fn snapshot(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).expect("utf-8 output")
    }

    fn take(&self) -> String {
        let mut buf = self.0.borrow_mut();
        let out = String::from_utf8(buf.clone()).expect("utf-8 output");
        buf.clear();
        out
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

const WIDTH: u16 = 20;
const HEIGHT: u16 = 6;

fn caps(synchronized_output: bool) -> TerminalCaps {
    TerminalCaps {
        synchronized_output,
        width: WIDTH,
        height: HEIGHT,
    }
}

fn renderer() -> (Renderer<Sink>, Sink) {
    let sink = Sink::default();
    (Renderer::new(sink.clone(), caps(true)), sink)
}

fn lines(rows: &[&str]) -> Vec<PreparedLine> {
    prepare_lines(rows.iter().copied(), WIDTH)
}

fn rows(rows: &[&str]) -> Frame {
    Frame::viewport(lines(rows))
}

/// Rows painted by a `\x1b[{row};1H` cursor move, in order of appearance.
fn addressed_rows(stream: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for piece in stream.split("\x1b[").skip(1) {
        let Some((params, _)) = piece.split_once('H') else {
            continue;
        };
        let Some((row, col)) = params.split_once(';') else {
            continue;
        };
        if col != "1" {
            continue;
        }
        // A cursor move must be the whole directive: private modes such as
        // `?25l` are not row addresses.
        if let Ok(row) = row.parse() {
            out.push(row);
        }
    }
    out
}

#[test]
fn vec_sink_captures_the_frame_verbatim() {
    // The generic sink makes the byte stream observable: a plain `Vec<u8>` is
    // enough to assert the exact sequences a terminal would receive.
    let mut renderer = Renderer::new(Vec::new(), caps(true));
    renderer.draw(rows(&["hello"])).unwrap();
    let out = String::from_utf8(renderer.into_inner()).expect("utf-8 output");
    assert_eq!(
        out,
        "\x1b[?25l\x1b[?2026h\x1b[?7l\x1b[6;1Hhello\x1b[K\x1b[?2026l\x1b[?7h\x1b[?25h"
    );
}

#[test]
fn history_ack_is_monotonic_and_one_shot() {
    let (mut renderer, sink) = renderer();
    let viewport = lines(&["prompt"]);

    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(1, lines(&["done one"]))),
            viewport: viewport.clone(),
        })
        .unwrap();
    assert_eq!(renderer.acked_history_id(), 1);
    assert_eq!(renderer.history_rows(), 1);
    let first = sink.take();
    assert!(
        first.contains("done one"),
        "history row not written: {first:?}"
    );

    // Same id again: no history bytes at all.
    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(1, lines(&["done one"]))),
            viewport: viewport.clone(),
        })
        .unwrap();
    let replay = sink.take();
    assert!(
        !replay.contains("done one"),
        "replayed batch was written twice: {replay:?}"
    );
    assert_eq!(renderer.acked_history_id(), 1);
    assert_eq!(renderer.history_rows(), 1);

    // Older id: ignored, watermark does not move backwards.
    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(0, lines(&["stale"]))),
            viewport,
        })
        .unwrap();
    let stale = sink.take();
    assert!(!stale.contains("stale"), "stale batch written: {stale:?}");
    assert_eq!(renderer.acked_history_id(), 1);

    // A newer id advances the watermark exactly once.
    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(2, lines(&["done two"]))),
            viewport: lines(&["prompt"]),
        })
        .unwrap();
    assert_eq!(renderer.acked_history_id(), 2);
    assert_eq!(renderer.history_rows(), 2);
}

#[test]
fn viewport_diff_only_writes_changed_rows() {
    let (mut renderer, sink) = renderer();
    renderer
        .draw(rows(&["header", "spinner 1", "footer"]))
        .unwrap();
    let initial = sink.take();
    assert_eq!(
        addressed_rows(&initial),
        vec![4, 5, 6],
        "first paint must place every row"
    );

    renderer
        .draw(rows(&["header", "spinner 2", "footer"]))
        .unwrap();
    let diff = sink.take();
    assert_eq!(
        addressed_rows(&diff),
        vec![5],
        "only the changed row may be addressed: {diff:?}"
    );
    assert!(diff.contains("spinner 2"));
    assert_eq!(
        diff.matches("header").count(),
        0,
        "unchanged row rewritten: {diff:?}"
    );
}

#[test]
fn viewport_shrink_emits_clear_to_end() {
    let (mut renderer, sink) = renderer();
    renderer
        .draw(rows(&["one", "two", "three", "four"]))
        .unwrap();
    sink.take();

    renderer.draw(rows(&["one", "two"])).unwrap();
    let shrink = sink.take();
    assert!(
        shrink.contains("\x1b[J"),
        "shrink must clear the tail: {shrink:?}"
    );
    // Rows of the old band above the new one are cleared per line, not deleted.
    assert!(
        !shrink.contains("\x1b[2J") && !shrink.contains("\x1b[M"),
        "no whole-screen or line delete may be emitted: {shrink:?}"
    );
    let addresses = addressed_rows(&shrink);
    assert!(
        addresses.contains(&5),
        "old top row not cleared: {addresses:?}"
    );
    assert_eq!(
        renderer.caps(),
        caps(true),
        "geometry must not change on shrink"
    );
}

#[test]
fn paint_sequence_order() {
    let (mut renderer, sink) = renderer();
    renderer.draw(rows(&["hi"])).unwrap();
    let out = sink.take();

    let hide = out.find("\x1b[?25l").expect("missing hide-cursor");
    let sync_on = out.find("\x1b[?2026h").expect("missing sync begin");
    let wrap_off = out.find("\x1b[?7l").expect("missing autowrap off");
    let body = out.find("hi").expect("missing row text");
    let sync_off = out.find("\x1b[?2026l").expect("missing sync end");
    let wrap_on = out.find("\x1b[?7h").expect("missing autowrap on");
    let show = out.find("\x1b[?25h").expect("missing show-cursor");

    assert!(hide < sync_on, "cursor must hide before sync begin");
    assert!(sync_on < wrap_off, "autowrap off inside the sync block");
    assert!(
        wrap_off < body && body < sync_off,
        "row text must be inside the frame"
    );
    assert!(sync_off < wrap_on, "autowrap restored after sync end");
    assert!(wrap_on < show, "cursor restored last");
    assert_eq!(out.matches("\x1b[?25l").count(), 1);
    assert_eq!(out.matches("\x1b[?25h").count(), 1);
}

#[test]
fn no_sync_caps_skips_dec2026() {
    let sink = Sink::default();
    let mut renderer = Renderer::new(sink.clone(), caps(false));
    renderer.draw(rows(&["hi"])).unwrap();
    let out = sink.take();

    assert!(!out.contains("\x1b[?2026h"), "sync begin leaked: {out:?}");
    assert!(!out.contains("\x1b[?2026l"), "sync end leaked: {out:?}");
    assert!(out.contains("\x1b[?25l") && out.contains("\x1b[?25h"));
    assert!(out.contains("\x1b[?7l") && out.contains("\x1b[?7h"));
}

#[test]
fn redraw_of_identical_frame_writes_no_rows() {
    let (mut renderer, sink) = renderer();
    renderer.draw(rows(&["static", "text"])).unwrap();
    sink.take();
    renderer.draw(rows(&["static", "text"])).unwrap();
    let again = sink.take();
    assert!(
        addressed_rows(&again).is_empty(),
        "identical frame repainted rows: {again:?}"
    );
    // The framing is still emitted so the terminal sees a well-formed paint.
    assert!(again.contains("\x1b[?25l") && again.contains("\x1b[?25h"));
}

#[test]
fn invalidate_forces_full_repaint_after_resize() {
    let (mut renderer, sink) = renderer();
    renderer.draw(rows(&["a", "b"])).unwrap();
    sink.take();

    renderer.resize(30, 8);
    renderer.draw(rows(&["a", "b"])).unwrap();
    let repaint = sink.take();
    // The band is bottom-anchored in the new geometry: rows 7 and 8.
    assert_eq!(
        addressed_rows(&repaint),
        vec![7, 8],
        "resize must repaint all rows"
    );
}

#[test]
fn history_write_erases_live_band_first() {
    let (mut renderer, sink) = renderer();
    renderer.draw(rows(&["live one", "live two"])).unwrap();
    sink.take();

    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(1, lines(&["committed"]))),
            viewport: lines(&["live one", "live two"]),
        })
        .unwrap();
    let out = sink.take();

    let erase = out.find("\x1b[K").expect("no erase");
    let commit = out.find("committed").expect("committed row missing");
    assert!(
        erase < commit,
        "live band must be erased before history is written: {out:?}"
    );
    // Erasure happens per line from column 1 — never a line delete.
    assert!(!out.contains("\x1b[M") && !out.contains("\x1b[1J"));
    assert_eq!(renderer.acked_history_id(), 1);
}

#[test]
fn full_width_row_is_not_erased_under_the_cursor() {
    // A row that exactly fills the terminal ends with the cursor on the last
    // column; an `EL` there would delete the character just written.
    let (mut wide, wide_sink) = renderer();
    let full = "x".repeat(WIDTH as usize);
    wide.draw(Frame::viewport(lines(&[&full]))).unwrap();
    let out = wide_sink.take();
    let text = out.find(&full).expect("full-width row missing");
    let after = &out[text + full.len()..];
    assert!(
        !after.starts_with("\x1b[K"),
        "erase-to-eol eaten at full width: {:?}",
        &out[text..]
    );

    // A short row does erase its tail.
    let (mut renderer, sink) = renderer();
    renderer.draw(Frame::viewport(lines(&["short"]))).unwrap();
    let out = sink.take();
    assert!(
        out.contains("short\x1b[K"),
        "short row must erase its tail: {out:?}"
    );
}

#[test]
fn history_rows_advance_with_crlf() {
    // Raw mode keeps the tty out of the kernel's ONLCR translation, so the
    // renderer must emit CRLF itself or rows would start mid-line.
    let (mut renderer, sink) = renderer();
    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(1, lines(&["one", "two"]))),
            viewport: lines(&["live"]),
        })
        .unwrap();
    let out = sink.take();
    let one = out.find("one").expect("first row missing");
    let two = out.find("two").expect("second row missing");
    assert!(
        out[one..two].contains("\r\n"),
        "history rows not CRLF separated: {:?}",
        &out[one..two]
    );
    assert!(!out[one..two].contains("\none"), "bare LF separator used");
}

/// Minimal VT screen: enough of CUP / EL / ED / CR / LF / scroll to assert
/// what a real terminal would show, which is the only way to catch a paint that
/// looks plausible in bytes but overwrites committed rows.
struct Screen {
    cells: Vec<Vec<char>>,
    row: usize,
    col: usize,
    width: usize,
    height: usize,
    /// Rows pushed off the top, exactly as a terminal archives them.
    scrollback: Vec<String>,
}

impl Screen {
    fn new(width: usize, height: usize) -> Self {
        Self {
            cells: vec![vec![' '; width]; height],
            row: 0,
            col: 0,
            width,
            height,
            scrollback: Vec::new(),
        }
    }

    fn feed(&mut self, stream: &str) {
        let bytes = stream.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                0x1b if bytes.get(i + 1) == Some(&b'[') => {
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    if j >= bytes.len() {
                        return;
                    }
                    let final_byte = bytes[j] as char;
                    let params = &stream[i + 2..j];
                    self.csi(params, final_byte);
                    i = j + 1;
                }
                b'\r' => {
                    self.col = 0;
                    i += 1;
                }
                b'\n' => {
                    self.linefeed();
                    i += 1;
                }
                0x1b => {
                    i += 2;
                }
                _ => {
                    let ch = stream[i..].chars().next().unwrap();
                    i += ch.len_utf8();
                    self.put(ch);
                }
            }
        }
    }

    fn csi(&mut self, params: &str, final_byte: char) {
        let numbers: Vec<usize> = params
            .trim_start_matches('?')
            .split(';')
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect();
        match final_byte {
            'H' => {
                self.row = numbers
                    .first()
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.height - 1);
                self.col = numbers
                    .get(1)
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(self.width - 1);
            }
            'K' => {
                // Erase to end of line, and no further: the cursor's own cell
                // is included exactly as a terminal would include it.
                for col in self.col..self.width {
                    self.cells[self.row][col] = ' ';
                }
            }
            'J' => {
                for col in self.col..self.width {
                    self.cells[self.row][col] = ' ';
                }
                for row in self.row + 1..self.height {
                    self.cells[row].fill(' ');
                }
            }
            _ => {}
        }
    }

    fn linefeed(&mut self) {
        if self.row + 1 >= self.height {
            let top: String = self.cells.remove(0).iter().collect();
            self.scrollback.push(top.trim_end().to_string());
            self.cells.push(vec![' '; self.width]);
        } else {
            self.row += 1;
        }
    }

    fn put(&mut self, ch: char) {
        if self.col >= self.width {
            // Autowrap is disabled during a paint; a full row clamps instead.
            self.col = self.width - 1;
            self.cells[self.row][self.col] = ch;
            return;
        }
        self.cells[self.row][self.col] = ch;
        self.col += 1;
    }

    fn line(&self, row: usize) -> String {
        self.cells[row]
            .iter()
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn lines(&self) -> Vec<String> {
        (0..self.height).map(|row| self.line(row)).collect()
    }
}

#[test]
fn committed_history_is_never_overwritten_by_the_viewport() {
    // 6 rows: commit 2 rows of history while a 3-row viewport is live. The
    // viewport must flow below the history, never be absolute-positioned over
    // it — that would silently rewrite a committed row.
    let (mut renderer, sink) = renderer();
    let mut screen = Screen::new(WIDTH as usize, HEIGHT as usize);

    let viewport = lines(&["vp1", "vp2", "vp3"]);
    draw_into(
        &mut renderer,
        &sink,
        &mut screen,
        Frame::viewport(viewport.clone()),
    );

    let mut seen = Vec::new();
    for id in 1..=4u64 {
        let row = format!("commit {id}");
        let rows = lines(&[&row]);
        draw_into(
            &mut renderer,
            &sink,
            &mut screen,
            Frame {
                history: Some(HistoryBatch::new(id, rows)),
                viewport: viewport.clone(),
            },
        );
        seen.push(row);
        let screen_lines = screen.lines();
        // A committed row is on screen or archived in scrollback, once and only
        // once, with its exact text: that is history finality.
        let mut visible: Vec<String> = screen.scrollback.clone();
        visible.extend(screen_lines.iter().cloned());
        for committed in &seen {
            assert_eq!(
                visible.iter().filter(|line| *line == committed).count(),
                1,
                "committed row {committed:?} not committed exactly once after batch {id}: {visible:?}"
            );
        }
        // The viewport is always the bottom block, in order.
        assert_eq!(
            &screen_lines[HEIGHT as usize - 3..],
            &["vp1".to_string(), "vp2".to_string(), "vp3".to_string()],
            "viewport not flush with the bottom after batch {id}: {screen_lines:?}"
        );
    }
}

/// Draw a frame and replay the newly written bytes onto the emulated screen.
fn draw_into(renderer: &mut Renderer<Sink>, sink: &Sink, screen: &mut Screen, frame: Frame) {
    let before = sink.snapshot().len();
    renderer.draw(frame).expect("draw must not fail");
    let all = sink.snapshot();
    screen.feed(&all[before..]);
}

#[test]
fn stale_empty_batch_does_not_move_the_watermark() {
    let (mut renderer, sink) = renderer();
    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(5, lines(&["five"]))),
            viewport: lines(&["vp"]),
        })
        .unwrap();
    assert_eq!(renderer.acked_history_id(), 5);
    sink.take();

    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(2, Vec::new())),
            viewport: lines(&["vp"]),
        })
        .unwrap();
    assert_eq!(
        renderer.acked_history_id(),
        5,
        "an empty stale batch moved the watermark backwards"
    );
    assert_eq!(renderer.history_rows(), 1);
}

#[test]
fn viewport_stays_under_history_after_a_short_commit() {
    // A commit that does not fill the screen leaves the viewport mid-screen.
    // The following viewport-only frames must keep it there: jumping back to
    // the bottom would detach it from the history it sits under.
    let (mut renderer, sink) = renderer();
    let mut screen = Screen::new(WIDTH as usize, HEIGHT as usize);
    let viewport = lines(&["vp1", "vp2"]);

    draw_into(
        &mut renderer,
        &sink,
        &mut screen,
        Frame::viewport(viewport.clone()),
    );
    draw_into(
        &mut renderer,
        &sink,
        &mut screen,
        Frame {
            history: Some(HistoryBatch::new(1, lines(&["committed"]))),
            viewport: viewport.clone(),
        },
    );

    let after_commit = screen.lines();
    let committed_row = after_commit
        .iter()
        .position(|line| line == "committed")
        .expect("committed row missing");
    assert_eq!(
        &after_commit[committed_row + 1..committed_row + 3],
        &["vp1".to_string(), "vp2".to_string()],
        "viewport not directly under history: {after_commit:?}"
    );

    // A viewport-only refresh must rewrite those same rows in place.
    draw_into(
        &mut renderer,
        &sink,
        &mut screen,
        Frame::viewport(lines(&["vp1", "vp2b"])),
    );
    let after = screen.lines();
    assert_eq!(
        &after[committed_row + 1..committed_row + 3],
        &["vp1".to_string(), "vp2b".to_string()],
        "viewport moved or went stale after an in-place refresh: {after:?}"
    );
    assert!(
        after.iter().any(|line| line == "committed"),
        "committed row lost after a viewport refresh: {after:?}"
    );
}

#[test]
fn growing_past_the_bottom_never_scrolls_live_rows() {
    // Anchor the viewport mid-screen with a short commit, then grow it until it
    // no longer fits. It must re-anchor at the bottom instead of writing past
    // it, which would scroll a live row into scrollback.
    let (mut renderer, sink) = renderer();
    let mut screen = Screen::new(WIDTH as usize, HEIGHT as usize);

    draw_into(
        &mut renderer,
        &sink,
        &mut screen,
        Frame::viewport(lines(&["vp1"])),
    );
    draw_into(
        &mut renderer,
        &sink,
        &mut screen,
        Frame {
            history: Some(HistoryBatch::new(1, lines(&["committed"]))),
            viewport: lines(&["vp1"]),
        },
    );
    // Five viewport rows starting just below the committed row run past the
    // last screen row.
    draw_into(
        &mut renderer,
        &sink,
        &mut screen,
        Frame::viewport(lines(&["vpA", "vpB", "vpC", "vpD", "vpE"])),
    );

    // Scrolling past the bottom is allowed — but only finished rows may leave
    // the screen, so the committed row must be the one archived.
    let lines_now = screen.lines();
    // The committed row survives on screen or in scrollback; either way it was
    // not painted over by a live row.
    let combined: Vec<&String> = screen.scrollback.iter().chain(lines_now.iter()).collect();
    assert!(
        combined.iter().any(|line| *line == "committed"),
        "committed row lost: screen={lines_now:?} scrollback={:?}",
        screen.scrollback
    );
    assert!(
        screen.scrollback.iter().all(|line| !line.starts_with("vp")),
        "a live row scrolled into scrollback: {:?}",
        screen.scrollback
    );
    assert!(
        lines_now.iter().all(|line| line != "vp1"),
        "stale live row survived the grow: {lines_now:?}"
    );
    assert_eq!(
        &lines_now[HEIGHT as usize - 5..],
        &["vpA", "vpB", "vpC", "vpD", "vpE"].map(str::to_string),
        "grown viewport not flush with the bottom: {lines_now:?}"
    );
}

#[test]
fn empty_history_batch_advances_watermark_only() {
    let (mut renderer, sink) = renderer();
    renderer.draw(rows(&["view"])).unwrap();
    sink.take();

    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(7, Vec::new())),
            viewport: lines(&["view"]),
        })
        .unwrap();
    assert_eq!(renderer.acked_history_id(), 7);
    assert_eq!(renderer.history_rows(), 0);
    assert!(
        addressed_rows(&sink.take()).is_empty(),
        "empty batch must not repaint the viewport"
    );
}

// ---------------------------------------------------------------------------
// Regression: defects found by the omp review (2026-11-20)
// ---------------------------------------------------------------------------

#[test]
fn empty_viewport_batch_keeps_the_anchor_so_history_survives() {
    // Bug: committing with an empty viewport cleared `last_top`, so the next
    // frame bottom-anchored its band and painted over the committed rows.
    let (mut renderer, sink) = renderer();
    let mut stream = String::new();
    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(1, lines(&["keepA", "keepB"]))),
            viewport: Vec::new(),
        })
        .unwrap();
    stream.push_str(&sink.take());

    renderer
        .draw(Frame {
            history: None,
            viewport: lines(&["vp1", "vp2", "vp3"]),
        })
        .unwrap();
    stream.push_str(&sink.take());

    let mut screen = Screen::new(WIDTH as usize, HEIGHT as usize);
    screen.feed(&stream);
    let visible: Vec<String> = screen
        .cells
        .iter()
        .map(|row| row.iter().collect::<String>().trim_end().to_string())
        .collect();
    let mut combined = screen.scrollback.clone();
    combined.extend(visible);
    assert!(
        combined.iter().any(|line| line == "keepA") && combined.iter().any(|line| line == "keepB"),
        "committed rows were overwritten by the live band: {combined:?}"
    );
}

#[test]
fn empty_batch_still_places_a_changed_viewport() {
    // Bug: an empty history batch returned early without placing the
    // viewport, losing that frame's viewport update entirely.
    let (mut renderer, sink) = renderer();
    renderer.draw(rows(&["a"])).unwrap();
    sink.take();

    renderer
        .draw(Frame {
            history: Some(HistoryBatch::new(1, Vec::new())),
            viewport: lines(&["b"]),
        })
        .unwrap();

    let addressed = addressed_rows(&sink.take());
    assert_eq!(addressed, vec![HEIGHT], "viewport update was dropped");
}

#[test]
fn failed_flush_rolls_back_the_viewport_memo() {
    // Bug: last_viewport/last_top were updated before the write, so a failed
    // flush left the memo claiming rows that never reached the screen; the
    // retry then repainted history a second time.
    struct FailingSink {
        inner: Sink,
        fail_next: Rc<RefCell<bool>>,
    }
    impl Write for FailingSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if *self.fail_next.borrow() {
                *self.fail_next.borrow_mut() = false;
                return Err(io::Error::other("flush failed"));
            }
            self.inner.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let inner = Sink::default();
    let fail_next = Rc::new(RefCell::new(true));
    let mut renderer = Renderer::new(
        FailingSink {
            inner: inner.clone(),
            fail_next: fail_next.clone(),
        },
        caps(true),
    );

    let frame = Frame {
        history: Some(HistoryBatch::new(1, lines(&["done"]))),
        viewport: lines(&["vp"]),
    };
    assert!(renderer.draw(frame.clone()).is_err());
    assert_eq!(renderer.acked_history_id(), 0, "ack advanced on failure");
    renderer.draw(frame).unwrap();

    let written = inner.snapshot();
    assert_eq!(
        written.matches("done").count(),
        1,
        "history row written twice: {written:?}"
    );
    assert_eq!(renderer.history_rows(), 1);
}

#[test]
fn oversized_viewport_keeps_the_tail() {
    // Bug: `[..min(height)]` kept the head, so the prompt row (last) was
    // never painted on a viewport taller than the screen.
    let (mut renderer, sink) = renderer();
    let tall: Vec<&str> = vec!["r1", "r2", "r3", "r4", "r5", "r6", "PROMPT", "tail"];
    renderer.draw(rows(&tall)).unwrap();
    let written = sink.take();
    let mut screen = Screen::new(WIDTH as usize, HEIGHT as usize);
    screen.feed(&written);
    let visible: Vec<String> = screen
        .cells
        .iter()
        .map(|row| row.iter().collect::<String>().trim_end().to_string())
        .collect();
    assert!(
        visible.iter().any(|line| line == "PROMPT"),
        "prompt row dropped: {visible:?}"
    );
    assert!(
        visible.iter().any(|line| line == "tail"),
        "last row dropped: {visible:?}"
    );
}
