//! Status-line HUD: one dim row summarising supervisor activity.
//!
//! The HUD is the top of the viewport: a single row carrying the product
//! context on the left (model, cwd, ...) and the supervisor's activity on the
//! right (how many agents/tasks are running, a spinner, context usage). It is
//! re-supplied every frame, so [`HudState::render`] must be pure and must never
//! emit a row wider than the width it was given — a row that wraps corrupts
//! every row the renderer paints afterwards.
//!
//! # Fitting order
//!
//! Three things compete for one row, and the order they give way is fixed:
//!
//! 1. the middle gap collapses (padding is decoration, never content),
//! 2. the summary truncates (activity counts are advisory),
//! 3. the left segment truncates (context is what the user is looking at, so it
//!    is the last thing to lose columns).
//!
//! Widths are measured with [`visible_width`], so a CJK model name or an emoji
//! in a skill name costs the columns it actually paints rather than its byte
//! length.
//!
//! # Why the summary is empty when idle
//!
//! The struct has no separate `busy` flag on purpose: the summary *is* the
//! busy signal. A supervisor with nothing running produces an empty summary,
//! which hides the spinner, so the HUD cannot spin at an idle shell.

use aish_supervisor::SupervisorSnapshot;

use crate::highlight::{DIM, RESET};
use crate::render::{prepare_line, visible_width};

/// Spinner frames, advanced one per [`HudState::tick`] while the supervisor is
/// busy.
///
/// Braille cells are used because each frame is exactly one column wide in
/// every terminal and font that can draw them at all, so advancing the spinner
/// cannot change the row's width and cannot make it jump horizontally.
pub const SPINNER_FRAMES: [char; 8] = ['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];

/// Cells in the context-usage bar.
const CTX_CELLS: usize = 10;

/// One-row status line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HudState {
    /// Right-aligned summary text (e.g. `"2 agents · 1 task · 3 sessions"`).
    ///
    /// Empty while the supervisor is idle, which is what hides the spinner.
    pub summary: String,
    /// Spinner frame index for the busy indicator.
    pub spinner: usize,
    /// Left-aligned segment (model name, cwd, etc).
    pub left: String,
    /// Context usage percentage 0..=100, rendered as a bar.
    pub ctx_percent: u8,
}

impl HudState {
    /// Empty HUD: no left segment, no activity, context bar at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update from a supervisor snapshot.
    ///
    /// The summary text is the supervisor's own, so the HUD and any other
    /// consumer cannot disagree on how activity is phrased. Only *running* work
    /// makes the HUD busy: finished, failed and cancelled rows are history, and
    /// a shell whose only activity is old history must not show a spinner.
    pub fn set_snapshot(&mut self, snap: &SupervisorSnapshot) {
        self.summary = snap.hud_summary();
    }

    /// Replace the left segment (model name, working directory, ...).
    pub fn set_left(&mut self, left: impl Into<String>) {
        self.left = left.into();
    }

    /// Set context usage; values above 100 are clamped so the bar cannot
    /// overfill.
    pub fn set_ctx_percent(&mut self, pct: u8) {
        self.ctx_percent = pct.min(100);
    }

    /// Advance the spinner (call once per frame when busy).
    pub fn tick(&mut self) {
        self.spinner = (self.spinner + 1) % SPINNER_FRAMES.len();
    }

    /// Render exactly ONE row at `width`, fitting both segments.
    ///
    /// Layout: left ... summary [spinner] [ctx bar]; middle space collapses
    /// first, then the summary truncates, then the left truncates.
    pub fn render(&self, width: u16) -> String {
        if width == 0 {
            return String::new();
        }

        let bar = self.ctx_bar();
        // The spinner and the bar are one inseparable tail: truncating between
        // them would leave a spinner with nothing to spin about.
        let tail = fit(
            &if self.summary.is_empty() {
                bar
            } else {
                format!("{} {}", self.spinner_frame(), bar)
            },
            width,
        );
        let tail_w = visible_width(&tail);

        let left = self.left.as_str();
        let summary = self.summary.as_str();
        // Every segment is measured *after* fitting, never by adding up raw
        // widths: `fit` expands tabs from the segment's own origin, so the
        // fitted pieces are additively measurable while the raw text is not (a
        // tab costs one column at column 0 but up to eight further right, which
        // is how a row could previously end up wider than its budget).
        let left_w = visible_width(left).min(width);

        // The summary gives way first, and only then the left segment.
        let summary_budget = width.saturating_sub(left_w.saturating_add(tail_w));
        let summary_fit = fit(summary, summary_budget);
        let left_budget = width.saturating_sub(visible_width(&summary_fit).saturating_add(tail_w));
        let left_fit = fit(left, left_budget);

        // Whatever the three segments left over is the middle gap, so it can
        // only ever shrink the row and never grow it.
        let used = visible_width(&left_fit)
            .saturating_add(visible_width(&summary_fit))
            .saturating_add(tail_w);
        let gap = width.saturating_sub(used) as usize;

        let mut row = String::with_capacity(left_fit.len() + gap + summary_fit.len() + tail.len());
        row.push_str(&left_fit);
        row.extend(std::iter::repeat_n(' ', gap));
        row.push_str(&summary_fit);
        row.push_str(&tail);
        if visible_width(&row) > width {
            // Belt and braces for release builds: a row that wraps corrupts
            // every row painted after it, so the budget is enforced, not
            // merely asserted.
            row = prepare_line(&row, width).text;
        }
        format!("{DIM}{row}{RESET}")
    }

    /// The context-usage bar: ten cells plus the percentage.
    fn ctx_bar(&self) -> String {
        let pct = self.ctx_percent.min(100) as usize;
        // Round to the nearest cell so 5% shows one filled cell rather than
        // nothing at all.
        let filled = (pct * CTX_CELLS + 50) / 100;
        let mut bar = String::with_capacity(CTX_CELLS * 3 + 4);
        bar.extend(std::iter::repeat_n('█', filled));
        bar.extend(std::iter::repeat_n('░', CTX_CELLS - filled));
        bar.push_str(&format!(" {pct}%"));
        bar
    }

    /// Current spinner glyph.
    fn spinner_frame(&self) -> char {
        SPINNER_FRAMES[self.spinner % SPINNER_FRAMES.len()]
    }
}

/// `text` normalized into at most `budget` columns.
///
/// [`prepare_line`] is always used, never a byte slice: besides cutting a wide
/// glyph into its half-cell instead of in two, it is the only place that
/// materializes tabs and drops control characters, and the fitted pieces are
/// what the layout adds up. Returning `text` unchanged whenever it *measured*
/// short was the bug: a tab's width depends on the absolute column it starts
/// at, so it measures narrower at the row origin than it paints once the left
/// segment and the gap have shifted it right, and the sum under-counted the
/// row.
fn fit(text: &str, budget: u16) -> String {
    prepare_line(text, budget).text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::strip_escapes;
    use aish_supervisor::{
        AgentRun, AgentState, SessionRole, SessionRow, SupervisorSnapshot, TaskRow, TaskState,
    };

    fn agent(id: &str, state: AgentState) -> AgentRun {
        AgentRun {
            spawn_id: id.to_string(),
            agent_type: "task".to_string(),
            depth: 0,
            skill_name: None,
            description: None,
            state,
            tool_calls: 0,
            last_text: String::new(),
            started_at_ms: 0,
            finished_at_ms: None,
        }
    }

    fn task(id: &str, state: TaskState) -> TaskRow {
        TaskRow {
            task_id: id.to_string(),
            command: "echo hi".to_string(),
            state,
            exit_code: None,
            started_at_ms: 0,
            finished_at_ms: None,
            output_path: String::new(),
        }
    }

    fn session(id: &str) -> SessionRow {
        SessionRow {
            session_id: id.to_string(),
            socket_path: String::new(),
            daemon_pid: 0,
            child_pid: None,
            cwd: "/tmp".to_string(),
            model: "test-model".to_string(),
            name: None,
            role: SessionRole::Controller,
            started_at: String::new(),
        }
    }

    fn snapshot(
        agents: Vec<AgentRun>,
        tasks: Vec<TaskRow>,
        sessions: Vec<SessionRow>,
    ) -> SupervisorSnapshot {
        SupervisorSnapshot {
            agents,
            tasks,
            sessions,
        }
    }

    /// A snapshot with one running agent, one running task and two sessions:
    /// the HUD example from the contract.
    fn busy_snapshot() -> SupervisorSnapshot {
        snapshot(
            vec![agent("spawn-1", AgentState::Running)],
            vec![task("task-1", TaskState::Running)],
            vec![session("s1"), session("s2")],
        )
    }

    #[test]
    fn idle_renders_left_and_ctx_bar() {
        let mut hud = HudState::new();
        hud.set_left("gpt-5");
        hud.set_ctx_percent(42);

        let row = hud.render(40);
        let plain = strip_escapes(&row);

        // `gpt-5` + gap + ten-cell bar (42% rounds to four filled cells).
        assert_eq!(plain, format!("gpt-5{}████░░░░░░ 42%", " ".repeat(21)));
        assert_eq!(visible_width(&plain), 40);
        // A dim row is closed with a reset, or every row after it stays dim.
        assert!(row.starts_with(DIM), "row is not dim: {row:?}");
        assert!(row.ends_with(RESET), "row is not closed: {row:?}");
        // Idle: no spinner.
        for frame in SPINNER_FRAMES {
            assert!(!plain.contains(frame), "idle HUD spun: {plain:?}");
        }
    }

    #[test]
    fn busy_includes_summary() {
        let mut hud = HudState::new();
        hud.set_snapshot(&busy_snapshot());
        hud.set_left("model-x");
        hud.set_ctx_percent(30);

        assert_eq!(hud.summary, "1 agent · 1 task · 2 sessions");

        let plain = strip_escapes(&hud.render(80));
        assert!(plain.contains("1 agent · 1 task · 2 sessions"), "{plain:?}");
        assert!(plain.contains(SPINNER_FRAMES[0]), "{plain:?}");
        assert!(plain.contains("30%"), "{plain:?}");
        // Left segment is still first, and the surplus went into the gap.
        assert!(plain.starts_with("model-x "), "{plain:?}");
    }

    #[test]
    fn spinner_advances() {
        let mut hud = HudState::new();
        hud.set_snapshot(&busy_snapshot());

        let mut seen = Vec::new();
        for _ in 0..SPINNER_FRAMES.len() {
            let plain = strip_escapes(&hud.render(80));
            let frame = SPINNER_FRAMES
                .iter()
                .find(|f| plain.contains(**f))
                .copied()
                .expect("busy HUD shows a spinner frame");
            seen.push(frame);
            hud.tick();
        }

        assert_eq!(seen, SPINNER_FRAMES.to_vec(), "spinner skipped a frame");
        // One full turn wraps back to the first frame.
        assert_eq!(hud.spinner, 0);
        hud.tick();
        assert_eq!(hud.spinner, 1);
    }

    #[test]
    fn middle_space_collapses_first() {
        let mut hud = HudState::new();
        hud.summary = "2 agents".to_string();
        hud.set_left("model-x");
        hud.set_ctx_percent(30);

        // 7 (left) + 8 (summary) + 16 (spinner + bar) = 31 columns minimum, so
        // 31 is the last width that keeps a gap of zero.
        let full = strip_escapes(&hud.render(31));
        assert_eq!(full, "model-x2 agents⣾ ███░░░░░░░ 30%");
        assert_eq!(visible_width(&full), 31);

        // Anything narrower loses the summary before it loses the left segment.
        let tight = strip_escapes(&hud.render(30));
        assert_eq!(tight, "model-x2 agent⣾ ███░░░░░░░ 30%");
        assert!(tight.starts_with("model-x"), "{tight:?}");

        // Wider rows pad the middle, never the right edge.
        let wide = strip_escapes(&hud.render(40));
        assert_eq!(
            wide,
            format!("model-x{}2 agents⣾ ███░░░░░░░ 30%", " ".repeat(9))
        );
        assert_eq!(visible_width(&wide), 40);
    }

    #[test]
    fn summary_truncates_before_left() {
        let mut hud = HudState::new();
        hud.summary = "12 agents · 7 tasks".to_string();
        hud.set_left("model-x");
        hud.set_ctx_percent(0);

        // 7 (left) + 19 (summary) + 15 (spinner + bar) = 41: at 36 the summary
        // has 14 columns and the left segment keeps all 7.
        let plain = strip_escapes(&hud.render(36));
        assert!(
            plain.starts_with("model-x"),
            "left segment was cut before the summary: {plain:?}"
        );
        assert!(
            !plain.contains("7 tasks"),
            "summary should have gone first: {plain:?}"
        );
        assert!(visible_width(&plain) <= 36, "{plain:?}");

        // Ten columns narrower and the summary is gone entirely while the left
        // segment is still intact.
        let narrow = strip_escapes(&hud.render(22));
        assert_eq!(narrow, "model-x⣾ ░░░░░░░░░░ 0%");
        assert!(visible_width(&narrow) <= 22, "{narrow:?}");
    }

    #[test]
    fn never_exceeds_width() {
        let mut hud = HudState::new();
        hud.summary = "12 agents · 7 tasks · 4 sessions".to_string();
        hud.set_left("漢字モデル · /home/ユーザー/aish");
        hud.set_ctx_percent(97);

        for width in 0..=20u16 {
            let row = hud.render(width);
            assert!(
                visible_width(&row) <= width,
                "width {width}: {row:?} painted {} columns",
                visible_width(&row)
            );
        }
        // The fuzz must also hold once the spinner moves: a frame is one column
        // wide by construction, so the layout cannot drift.
        for _ in 0..SPINNER_FRAMES.len() {
            hud.tick();
            for width in [1u16, 2, 7, 13, 19, 20] {
                assert!(visible_width(&hud.render(width)) <= width, "width {width}");
            }
        }
    }

    #[test]
    fn wide_chars_respected() {
        let mut hud = HudState::new();
        hud.set_left("漢字");
        hud.set_ctx_percent(50);

        // 4 columns of left + 12 of gap + 14 of tail = 30: the gap is measured
        // in display columns, not characters.
        let row = hud.render(30);
        let plain = strip_escapes(&row);
        assert_eq!(plain, format!("漢字{}█████░░░░░ 50%", " ".repeat(12)));
        assert_eq!(visible_width(&plain), 30);

        // A budget that lands inside a wide glyph fills the half cell instead of
        // leaving it stale.
        let cut = strip_escapes(&hud.render(5));
        assert_eq!(visible_width(&cut), 5);
        assert_eq!(cut.chars().count(), 5);
    }

    #[test]
    fn tab_in_summary_cannot_overflow() {
        // A tab measures four columns at the row origin but expands to the next
        // eight-column stop once the left segment and the gap shift it right.
        // Adding up raw segment widths therefore under-counted the row: it
        // painted 35 columns for a budget of 33.
        let mut hud = HudState::new();
        hud.set_left("");
        hud.summary = "with\t tab".to_string();
        hud.set_ctx_percent(0);

        let row = hud.render(33);
        assert_eq!(visible_width(&strip_escapes(&row)), 33, "row: {row:?}");
        assert!(
            !row.contains('\t'),
            "tab reached the terminal unexpanded: {row:?}"
        );
    }

    #[test]
    fn tab_fuzz_never_exceeds_width() {
        let mut hud = HudState::new();
        hud.set_ctx_percent(64);

        for (left, summary) in [
            ("", "with\t tab"),
            ("\tleft", "1\tagent"),
            ("a\tb\tc", "漢\t字"),
            ("\t\t", "\t"),
            ("model-x", "2 agents\t·\t1 task"),
        ] {
            hud.set_left(left);
            hud.summary = summary.to_string();
            for width in 1..=200u16 {
                let row = hud.render(width);
                let plain = strip_escapes(&row);
                assert!(
                    visible_width(&plain) <= width,
                    "{left:?}/{summary:?} width {width}: {plain:?} painted {}",
                    visible_width(&plain)
                );
                assert!(!plain.contains('\t'), "width {width}: tab leaked {plain:?}");
            }
        }
    }

    #[test]
    fn control_bytes_in_segments_are_dropped() {
        // Left and summary are product data too. A carriage return would repaint
        // the row from column 0 and a newline would paint a second row, both
        // outside the one-row contract, so neither may reach the terminal.
        let mut hud = HudState::new();
        hud.set_left("mod\rel\nx");
        hud.summary = "1\r\nagent".to_string();
        hud.set_ctx_percent(10);

        for width in 1..=80u16 {
            let row = hud.render(width);
            let plain = strip_escapes(&row);
            assert!(
                !plain.chars().any(|ch| ch.is_control() && ch != '\x1b'),
                "width {width}: control byte leaked {plain:?}"
            );
            assert!(visible_width(&plain) <= width, "width {width}: {plain:?}");
        }

        // Only the row framing escapes survive; the text itself is intact.
        let plain = strip_escapes(&hud.render(40));
        assert!(plain.starts_with("modelx"), "{plain:?}");
        assert!(plain.contains("1agent"), "{plain:?}");
    }
}
