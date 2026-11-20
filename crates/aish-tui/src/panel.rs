//! Full-width supervision panel: agents / tasks / sessions.
//!
//! The panel is the "who is working right now" screen: three tabs over the
//! three independence domains a shell supervises — sub-agent runs spawned by
//! the model, background tasks started from the prompt, and the PTY sessions
//! attached to the daemon.
//!
//! # Rendering is parameterized
//!
//! [`SupervisorPanel::render`] takes `(width, height)` and reads no terminal
//! size of its own. That is what makes the panel testable byte-for-byte and
//! what lets a caller put it beside the prompt instead of over it. The panel
//! also never mutates *selection* state from `render`: the only writes are the
//! remembered page height and the scroll offset, which exist so selection
//! movement can keep the selected row on screen — and so a resize can scroll a
//! stranded selection back into view — without being told the height again.
//! Both are atomics, which is what lets `render` stay `&self`.
//!
//! # Actions, not effects
//!
//! The panel does not stop tasks, cancel agents or attach sessions — it says
//! what the user asked for ([`PanelAction`]) and the caller performs it. The
//! action is latched on Enter and collected with
//! [`SupervisorPanel::take_action`], so a caller that ignores it cannot
//! accidentally stop something.

use std::sync::atomic::{AtomicUsize, Ordering};

use aish_supervisor::{AgentState, SessionRole, SupervisorSnapshot, TaskState};
use crossterm::event::{Event, KeyCode, KeyEventKind};

use crate::component::{Component, InputHandled, Interactive};
use crate::highlight::{CURSOR_OFF, CURSOR_ON, DIM, RESET};
use crate::render::{prepare_line, visible_width};

/// Green: a run that finished successfully.
const GREEN: &str = "\x1b[32m";
/// Red: a run that failed.
const RED: &str = "\x1b[31m";

/// Selection marker painted before the selected row (and its column twin
/// before every other row, so labels stay aligned).
const MARKER: &str = "▸ ";
/// Blank marker of the same width.
const MARKER_BLANK: &str = "  ";

/// Which list the panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelTab {
    /// Sub-agent runs.
    #[default]
    Agents,
    /// Background tasks.
    Tasks,
    /// Live PTY sessions.
    Sessions,
}

impl PanelTab {
    /// All tabs in display order.
    pub const ALL: [PanelTab; 3] = [PanelTab::Agents, PanelTab::Tasks, PanelTab::Sessions];

    /// Index into [`SupervisorPanel`]'s entry arrays.
    fn index(self) -> usize {
        match self {
            PanelTab::Agents => 0,
            PanelTab::Tasks => 1,
            PanelTab::Sessions => 2,
        }
    }

    /// Tab title as drawn in the header.
    fn title(self) -> &'static str {
        match self {
            PanelTab::Agents => "Agents",
            PanelTab::Tasks => "Tasks",
            PanelTab::Sessions => "Sessions",
        }
    }

    /// Placeholder shown by an empty tab.
    fn empty_message(self) -> &'static str {
        match self {
            PanelTab::Agents => "(no agents)",
            PanelTab::Tasks => "(no tasks)",
            PanelTab::Sessions => "(no sessions)",
        }
    }
}

/// One selectable row in the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelEntry {
    /// `spawn_id` / `task_id` / `session_id`.
    pub id: String,
    /// Primary text.
    pub label: String,
    /// Right-aligned detail (state, exit code, ...).
    pub detail: String,
    /// Colour the row is painted in.
    pub tone: EntryTone,
}

/// Colour class of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryTone {
    /// Still running: default foreground, no decoration.
    Running,
    /// Finished successfully.
    Ok,
    /// Failed.
    Failed,
    /// Not running and not interesting (cancelled, stopped, observer, ...).
    Muted,
}

impl EntryTone {
    /// Escape sequence entering this tone, or `""` for the default foreground.
    fn start(self) -> &'static str {
        match self {
            EntryTone::Running => "",
            EntryTone::Ok => GREEN,
            EntryTone::Failed => RED,
            EntryTone::Muted => DIM,
        }
    }

    /// Escape sequence leaving this tone.
    fn end(self) -> &'static str {
        match self {
            EntryTone::Running => "",
            _ => RESET,
        }
    }
}

/// Action the user asked for; the caller performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelAction {
    /// Nothing to do (no selection).
    None,
    /// Stop the background task with this id.
    StopTask(String),
    /// Cancel the sub-agent run with this spawn id.
    CancelAgent(String),
    /// Attach the terminal to this session.
    AttachSession(String),
    /// Close the panel.
    Close,
}

/// Three-tab supervision panel.
#[derive(Debug, Default)]
pub struct SupervisorPanel {
    tab: PanelTab,
    selected: usize,
    /// First painted list row.
    ///
    /// An atomic for the same reason as [`SupervisorPanel::page`]: `render`
    /// takes `&self`, and it is the only place that learns the window height,
    /// so it is also the only place that can notice a resize pushed the
    /// selection off screen. Leaving the correction in a local made `Enter`
    /// act on a row the user could not see.
    scroll: AtomicUsize,
    entries: [Vec<PanelEntry>; 3],
    /// List rows the last render was given.
    ///
    /// Selection movement needs it to scroll, and the alternative — asking the
    /// caller for the height again — would make `handle_input` depend on state
    /// only `render` has. Rendering itself stays a pure function of its
    /// arguments, so the value is an atomic rather than a `&mut` field.
    page: AtomicUsize,
    /// Action latched by Enter/Esc, collected by [`SupervisorPanel::take_action`].
    pending: Option<PanelAction>,
}

impl SupervisorPanel {
    /// Empty panel on the agents tab.
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild all three lists from a supervisor snapshot.
    pub fn set_snapshot(&mut self, snap: &SupervisorSnapshot) {
        self.entries[PanelTab::Agents.index()] = snap.agents.iter().map(agent_entry).collect();
        self.entries[PanelTab::Tasks.index()] = snap.tasks.iter().map(task_entry).collect();
        self.entries[PanelTab::Sessions.index()] =
            snap.sessions.iter().map(session_entry).collect();
        self.clamp_selection();
        // A list that shrank under the window still has to show its selection;
        // `ensure_visible` cannot scroll down past the new length because it
        // only ever moves the window towards the selected row.
        self.ensure_visible();
    }

    /// Tab currently shown.
    pub fn active_tab(&self) -> PanelTab {
        self.tab
    }

    /// Switch tabs, keeping the selection inside the new list.
    pub fn set_tab(&mut self, tab: PanelTab) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.selected = 0;
        self.scroll.store(0, Ordering::Relaxed);
        self.clamp_selection();
    }

    /// Entry under the cursor, if the active tab is not empty.
    pub fn selected_entry(&self) -> Option<&PanelEntry> {
        self.entries[self.tab.index()].get(self.selected)
    }

    /// Index of the selected row (for callers that scroll their own view).
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// First visible row.
    pub fn scroll_offset(&self) -> usize {
        self.scroll.load(Ordering::Relaxed)
    }

    /// Collect the latched action, if any.
    pub fn take_action(&mut self) -> Option<PanelAction> {
        self.pending.take()
    }

    /// Render `height` rows at `width` (header + list, no frame chrome).
    pub fn render(&self, width: u16, height: u16) -> Vec<String> {
        if height == 0 || width == 0 {
            return Vec::new();
        }
        let capacity = height as usize;
        let mut rows = Vec::with_capacity(capacity);

        rows.push(self.render_header(width));

        // One row of the budget is the header; the rest is list.
        let list_height = capacity - 1;
        let entries = &self.entries[self.tab.index()];
        self.page.store(list_height, Ordering::Relaxed);

        if entries.is_empty() {
            self.scroll.store(0, Ordering::Relaxed);
            rows.push(fit(
                &format!("{DIM}{}{RESET}", self.tab.empty_message()),
                width,
            ));
        } else {
            // The window never starts below the last row, and the selection is
            // never left outside it: `selected` is a plain field and the height
            // is only known here, so a resize (or a shorter list) can strand the
            // selection off screen. Both corrections are written back because
            // the caller acts on `selected_index()`: Enter must not stop a task
            // the user cannot see.
            let max_scroll = entries.len().saturating_sub(list_height);
            let scroll = self.scroll.load(Ordering::Relaxed).min(max_scroll);
            self.scroll.store(scroll, Ordering::Relaxed);
            self.ensure_visible();
            let scroll = self.scroll.load(Ordering::Relaxed);
            for (index, entry) in entries.iter().enumerate().skip(scroll).take(list_height) {
                rows.push(self.render_entry(index, entry, width));
            }
        }

        // Pad short lists so the panel owns exactly `height` rows: a caller
        // clearing the region first must not see stale content below it.
        while rows.len() < capacity {
            rows.push(String::new());
        }
        rows
    }

    /// `Agents(2) Tasks(1) Sessions(3)` with the active tab reverse-videoed.
    fn render_header(&self, width: u16) -> String {
        let mut parts = Vec::with_capacity(PanelTab::ALL.len() * 2);
        for tab in PanelTab::ALL {
            let title = format!("{}({})", tab.title(), self.entries[tab.index()].len());
            if tab == self.tab {
                parts.push(format!("{CURSOR_ON}{title}{CURSOR_OFF}"));
            } else {
                parts.push(format!("{DIM}{title}{RESET}"));
            }
        }
        fit(&parts.join(" "), width)
    }

    /// One list row: marker, label, padding, right-aligned detail.
    fn render_entry(&self, index: usize, entry: &PanelEntry, width: u16) -> String {
        // The marker is a trusted constant, so it is not fitted: `prepare_line`
        // trims trailing spaces and the marker *is* a glyph plus padding. Both
        // forms are two columns wide so labels stay aligned down the column; on
        // a single-column row only the glyph itself fits.
        let (marker, marker_w) = match width {
            0 => ("", 0),
            1 => (if index == self.selected { "▸" } else { " " }, 1),
            _ => (
                if index == self.selected {
                    MARKER
                } else {
                    MARKER_BLANK
                },
                2,
            ),
        };
        let avail = width - marker_w;

        // The detail is the row's state ("running", "exit 2"), so it keeps its
        // columns; the label, which is often a whole command line, is what
        // gives way. Capping the detail at half the row keeps the two from
        // starving each other on a narrow terminal.
        let detail_text = sanitize(&entry.detail);
        let detail_budget = visible_width(&detail_text).min(avail / 2);
        let detail = fit(&detail_text, detail_budget);
        let detail_w = visible_width(&detail);
        let reserve = u16::from(detail_w > 0);
        let label = fit(
            &sanitize(&entry.label),
            avail.saturating_sub(detail_w + reserve),
        );
        let label_w = visible_width(&label);
        let gap = avail.saturating_sub(label_w + detail_w);

        let start = entry.tone.start();
        let end = entry.tone.end();
        let mut row = String::with_capacity(
            marker.len() + label.len() + gap as usize + detail.len() + start.len() + end.len(),
        );
        row.push_str(marker);
        row.push_str(start);
        row.push_str(&label);
        row.extend(std::iter::repeat_n(' ', gap as usize));
        row.push_str(&detail);
        row.push_str(end);
        row
    }

    /// Keep the selection inside the active list.
    fn clamp_selection(&mut self) {
        let len = self.entries[self.tab.index()].len();
        self.selected = self.selected.min(len.saturating_sub(1));
        // A window never starts below the selected row: `ensure_visible` relies
        // on it to know which direction to scroll.
        let scroll = self.scroll.load(Ordering::Relaxed).min(self.selected);
        self.scroll.store(scroll, Ordering::Relaxed);
    }

    /// Move the selection by `delta`, clamping at both ends.
    fn move_selection(&mut self, delta: isize) {
        let len = self.entries[self.tab.index()].len();
        if len == 0 {
            self.selected = 0;
            self.scroll.store(0, Ordering::Relaxed);
            return;
        }
        let last = len - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(last)
        };
        self.ensure_visible();
    }

    /// Scroll the minimum amount that puts the selected row on screen.
    ///
    /// Takes `&self` because `render` — the only caller that knows the window
    /// height — has no `&mut` to work with; the offset lives in an atomic for
    /// exactly this reason.
    fn ensure_visible(&self) {
        let page = self.page.load(Ordering::Relaxed).max(1);
        let scroll = self.scroll.load(Ordering::Relaxed);
        let next = if self.selected < scroll {
            self.selected
        } else if self.selected >= scroll + page {
            self.selected + 1 - page
        } else {
            scroll
        };
        self.scroll.store(next, Ordering::Relaxed);
    }

    /// Action the current selection would trigger.
    fn action_for_selection(&self) -> PanelAction {
        let Some(entry) = self.selected_entry() else {
            return PanelAction::None;
        };
        match self.tab {
            PanelTab::Agents => PanelAction::CancelAgent(entry.id.clone()),
            PanelTab::Tasks => PanelAction::StopTask(entry.id.clone()),
            PanelTab::Sessions => PanelAction::AttachSession(entry.id.clone()),
        }
    }
}

impl Component for SupervisorPanel {
    /// Natural height: header plus every row of the active tab.
    fn render(&self, width: u16) -> Vec<String> {
        let rows = 1 + self.entries[self.tab.index()].len();
        self.render(width, rows.min(u16::MAX as usize) as u16)
    }
}

impl Interactive for SupervisorPanel {
    fn handle_input(&mut self, ev: Event) -> InputHandled {
        let Event::Key(key) = ev else {
            return InputHandled::Ignored;
        };
        // Terminals with the kitty protocol report press and release; acting on
        // both would move the cursor twice per keystroke.
        if key.kind == KeyEventKind::Release {
            return InputHandled::Ignored;
        }

        match key.code {
            KeyCode::Tab => {
                self.set_tab(match self.tab {
                    PanelTab::Agents => PanelTab::Tasks,
                    PanelTab::Tasks => PanelTab::Sessions,
                    PanelTab::Sessions => PanelTab::Agents,
                });
                InputHandled::RequestRender
            }
            KeyCode::BackTab => {
                self.set_tab(match self.tab {
                    PanelTab::Agents => PanelTab::Sessions,
                    PanelTab::Tasks => PanelTab::Agents,
                    PanelTab::Sessions => PanelTab::Tasks,
                });
                InputHandled::RequestRender
            }
            KeyCode::Up => {
                self.move_selection(-1);
                InputHandled::RequestRender
            }
            KeyCode::Down => {
                self.move_selection(1);
                InputHandled::RequestRender
            }
            KeyCode::Enter => {
                let action = self.action_for_selection();
                if action == PanelAction::None {
                    return InputHandled::Consumed;
                }
                self.pending = Some(action);
                InputHandled::Submit
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pending = Some(PanelAction::Close);
                InputHandled::Cancel
            }
            _ => InputHandled::Ignored,
        }
    }
}

/// `text` normalized into at most `budget` columns.
///
/// [`prepare_line`] is always used, never a byte slice and never a short
/// circuit: it is the only place that expands tabs, drops the control
/// characters that would move the cursor and keeps escape sequences from being
/// cut in half. A row's text is product data — a task's command line is raw
/// user or model input — so returning it unchanged whenever it *measured* short
/// let `\x1b[31m`, `\x1b]8;;...` and embedded newlines reach the terminal
/// verbatim.
fn fit(text: &str, budget: u16) -> String {
    prepare_line(text, budget).text
}

/// Reduce `text` to the printable, single-line content a row may paint.
///
/// Row text is product data — a task's command line is raw user or model
/// input — and [`prepare_line`] deliberately keeps escape sequences verbatim
/// because the renderer *builds* styled rows out of them. Passing that text
/// through unchanged would let `\x1b[31m` override the row's tone,
/// `\x1b]8;;url` open a hyperlink `RESET` cannot close (so every following row
/// stays clickable) and `\n` add rows the viewport budget never accounted for.
/// Tabs survive: [`fit`] materializes them as spaces, so the painted row is
/// exactly as wide as the sum of its fitted pieces.
fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|ch| *ch == '\t' || !ch.is_control())
        .collect()
}

fn agent_entry(run: &aish_supervisor::AgentRun) -> PanelEntry {
    let label = run
        .description
        .clone()
        .or_else(|| run.skill_name.clone())
        .unwrap_or_else(|| run.agent_type.clone());
    let label = if label.is_empty() {
        run.spawn_id.clone()
    } else {
        label
    };
    let tone = match run.state {
        AgentState::Running => EntryTone::Running,
        AgentState::Completed => EntryTone::Ok,
        AgentState::Failed => EntryTone::Failed,
        AgentState::Cancelled => EntryTone::Muted,
    };
    PanelEntry {
        id: run.spawn_id.clone(),
        label,
        detail: format!("{} · {} tools", run.state, run.tool_calls),
        tone,
    }
}

fn task_entry(row: &aish_supervisor::TaskRow) -> PanelEntry {
    let tone = match row.state {
        TaskState::Running => EntryTone::Running,
        TaskState::Completed => EntryTone::Ok,
        TaskState::Failed => EntryTone::Failed,
        TaskState::Stopped => EntryTone::Muted,
    };
    let detail = match row.exit_code {
        Some(code) => format!("{} · exit {code}", row.state),
        None => row.state.to_string(),
    };
    PanelEntry {
        id: row.task_id.clone(),
        label: row.command.clone(),
        detail,
        tone,
    }
}

fn session_entry(row: &aish_supervisor::SessionRow) -> PanelEntry {
    let tone = match row.role {
        SessionRole::Controller => EntryTone::Ok,
        SessionRole::Observer => EntryTone::Muted,
        SessionRole::Stale => EntryTone::Failed,
    };
    PanelEntry {
        id: row.session_id.clone(),
        label: row.name.clone().unwrap_or_else(|| row.cwd.clone()),
        detail: format!("{} · {}", row.role, row.model),
        tone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::strip_escapes;
    use aish_supervisor::{AgentRun, SessionRow, TaskRow};
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn agent(id: &str, state: AgentState) -> AgentRun {
        AgentRun {
            spawn_id: id.to_string(),
            agent_type: "task".to_string(),
            depth: 0,
            skill_name: None,
            description: Some(format!("description {id}")),
            state,
            tool_calls: 3,
            last_text: String::new(),
            started_at_ms: 0,
            finished_at_ms: None,
        }
    }

    fn task(id: &str, state: TaskState) -> TaskRow {
        TaskRow {
            task_id: id.to_string(),
            command: format!("cargo test {id}"),
            state,
            exit_code: None,
            started_at_ms: 0,
            finished_at_ms: None,
            output_path: String::new(),
        }
    }

    fn session(id: &str, role: SessionRole) -> SessionRow {
        SessionRow {
            session_id: id.to_string(),
            socket_path: String::new(),
            daemon_pid: 1,
            child_pid: None,
            cwd: format!("/work/{id}"),
            model: "gpt-5".to_string(),
            name: None,
            role,
            started_at: String::new(),
        }
    }

    fn panel() -> SupervisorPanel {
        let mut panel = SupervisorPanel::new();
        panel.set_snapshot(&SupervisorSnapshot {
            agents: vec![
                agent("spawn-a", AgentState::Running),
                agent("spawn-b", AgentState::Completed),
            ],
            tasks: vec![task("t1", TaskState::Running)],
            sessions: vec![
                session("s1", SessionRole::Controller),
                session("s2", SessionRole::Observer),
                session("s3", SessionRole::Stale),
            ],
        });
        panel
    }

    /// Panel with `n` running agents, for scrolling tests.
    fn panel_with_agents(n: usize) -> SupervisorPanel {
        let mut panel = SupervisorPanel::new();
        panel.set_snapshot(&SupervisorSnapshot {
            agents: (0..n)
                .map(|i| agent(&format!("spawn-{i:02}"), AgentState::Running))
                .collect(),
            tasks: Vec::new(),
            sessions: Vec::new(),
        });
        panel
    }

    #[test]
    fn tab_switches() {
        let mut panel = panel();
        assert_eq!(panel.active_tab(), PanelTab::Agents);

        assert_eq!(
            panel.handle_input(key(KeyCode::Tab)),
            InputHandled::RequestRender
        );
        assert_eq!(panel.active_tab(), PanelTab::Tasks);

        assert_eq!(
            panel.handle_input(key(KeyCode::Tab)),
            InputHandled::RequestRender
        );
        assert_eq!(panel.active_tab(), PanelTab::Sessions);

        // Tab wraps forward, BackTab wraps backward.
        panel.handle_input(key(KeyCode::Tab));
        assert_eq!(panel.active_tab(), PanelTab::Agents);
        panel.handle_input(key(KeyCode::BackTab));
        assert_eq!(panel.active_tab(), PanelTab::Sessions);

        // The selected entry follows the tab.
        panel.set_tab(PanelTab::Tasks);
        assert_eq!(
            panel.selected_entry().map(|e| e.id.as_str()),
            Some("t1"),
            "selection did not move to the new tab's list"
        );
    }

    #[test]
    fn selection_clamps() {
        let mut panel = panel();
        // Up at the top stays at the top.
        panel.handle_input(key(KeyCode::Up));
        assert_eq!(panel.selected_index(), 0);

        panel.handle_input(key(KeyCode::Down));
        assert_eq!(panel.selected_index(), 1);

        // Down at the bottom stays at the bottom (two agents).
        panel.handle_input(key(KeyCode::Down));
        assert_eq!(panel.selected_index(), 1);
        panel.handle_input(key(KeyCode::Down));
        assert_eq!(panel.selected_index(), 1);

        // An empty tab has no selection at all.
        let mut empty = SupervisorPanel::new();
        empty.set_snapshot(&SupervisorSnapshot::default());
        empty.handle_input(key(KeyCode::Down));
        assert_eq!(empty.selected_index(), 0);
        assert!(empty.selected_entry().is_none());
    }

    #[test]
    fn selection_produces_action() {
        let mut panel = panel();

        panel.handle_input(key(KeyCode::Down));
        panel.handle_input(key(KeyCode::Down));
        assert_eq!(
            panel.handle_input(key(KeyCode::Enter)),
            InputHandled::Submit
        );
        assert_eq!(
            panel.take_action(),
            Some(PanelAction::CancelAgent("spawn-b".to_string()))
        );
        // Collected once.
        assert_eq!(panel.take_action(), None);

        panel.set_tab(PanelTab::Tasks);
        panel.handle_input(key(KeyCode::Enter));
        assert_eq!(
            panel.take_action(),
            Some(PanelAction::StopTask("t1".to_string()))
        );

        panel.set_tab(PanelTab::Sessions);
        panel.handle_input(key(KeyCode::Down));
        panel.handle_input(key(KeyCode::Down));
        panel.handle_input(key(KeyCode::Enter));
        assert_eq!(
            panel.take_action(),
            Some(PanelAction::AttachSession("s3".to_string()))
        );

        // Enter on an empty tab latches nothing.
        let mut empty = SupervisorPanel::new();
        empty.set_snapshot(&SupervisorSnapshot::default());
        assert_eq!(
            empty.handle_input(key(KeyCode::Enter)),
            InputHandled::Consumed
        );
        assert_eq!(empty.take_action(), None);
    }

    #[test]
    fn esc_cancels() {
        let mut panel = panel();
        assert_eq!(panel.handle_input(key(KeyCode::Esc)), InputHandled::Cancel);
        assert_eq!(panel.take_action(), Some(PanelAction::Close));

        assert_eq!(
            panel.handle_input(key(KeyCode::Char('q'))),
            InputHandled::Cancel
        );
        assert_eq!(panel.take_action(), Some(PanelAction::Close));

        // Anything else is not the panel's.
        assert_eq!(
            panel.handle_input(key(KeyCode::Char('x'))),
            InputHandled::Ignored
        );
        assert_eq!(panel.take_action(), None);
    }

    #[test]
    fn scrolls_to_keep_selection_visible() {
        let mut panel = panel_with_agents(10);
        // Four list rows: the selection must walk the window down.
        panel.render(40, 5);
        assert_eq!(panel.scroll_offset(), 0);

        for _ in 0..3 {
            panel.handle_input(key(KeyCode::Down));
        }
        assert_eq!(panel.selected_index(), 3);
        assert_eq!(
            panel.scroll_offset(),
            0,
            "scrolled before the window filled"
        );

        panel.handle_input(key(KeyCode::Down));
        assert_eq!(panel.selected_index(), 4);
        assert_eq!(panel.scroll_offset(), 1, "selection left the window");

        let rows = panel.render(40, 5);
        let plain: Vec<String> = rows.iter().map(|r| strip_escapes(r)).collect();
        assert!(
            plain.iter().any(|r| r.contains("description spawn-04")),
            "selected row is not on screen: {plain:?}"
        );
        assert!(
            !plain.iter().any(|r| r.contains("description spawn-00")),
            "window did not scroll: {plain:?}"
        );

        // Walking back up scrolls the window up again.
        for _ in 0..4 {
            panel.handle_input(key(KeyCode::Up));
        }
        assert_eq!(panel.selected_index(), 0);
        assert_eq!(panel.scroll_offset(), 0);
    }

    #[test]
    fn renders_counts_in_header() {
        let panel = panel();
        let header = strip_escapes(&panel.render(60, 5)[0]);
        assert_eq!(header, "Agents(2) Tasks(1) Sessions(3)");

        // The active tab is the only reverse-videoed one, and every title is
        // closed so the attribute cannot bleed into the list.
        let raw = &panel.render(60, 5)[0];
        assert_eq!(raw.matches(CURSOR_ON).count(), 1, "{raw:?}");
        assert!(raw.contains(&format!("{CURSOR_ON}Agents(2){CURSOR_OFF}")));

        let mut panel = panel;
        panel.set_tab(PanelTab::Sessions);
        let raw = &panel.render(60, 5)[0];
        assert!(
            raw.contains(&format!("{CURSOR_ON}Sessions(3){CURSOR_OFF}")),
            "{raw:?}"
        );
    }

    #[test]
    fn detail_right_aligned() {
        let panel = panel();
        let rows = panel.render(50, 4);

        // Row 1 is the first agent (selected), row 2 the completed one.
        let selected = strip_escapes(&rows[1]);
        assert!(selected.starts_with(MARKER), "{selected:?}");
        assert!(selected.ends_with("running · 3 tools"), "{selected:?}");
        assert_eq!(visible_width(&selected), 50, "detail is not flush right");

        let unselected = strip_escapes(&rows[2]);
        assert!(unselected.starts_with(MARKER_BLANK), "{unselected:?}");
        assert!(
            unselected.ends_with("completed · 3 tools"),
            "{unselected:?}"
        );
        assert_eq!(visible_width(&unselected), 50, "detail is not flush right");

        // Tones: green for a completed agent, nothing for a running one.
        assert!(
            !rows[1].contains(GREEN),
            "running row was toned: {:?}",
            rows[1]
        );
        assert!(rows[2].contains(GREEN), "completed agent was not green");

        let mut panel = panel;
        panel.set_tab(PanelTab::Sessions);
        let rows = panel.render(50, 4);
        assert!(
            rows[2].contains(DIM),
            "observer session was not dim: {:?}",
            rows[2]
        );
        assert!(
            rows[3].contains(RED),
            "stale session was not red: {:?}",
            rows[3]
        );

        // A detail wider than half the row is capped so a long state string
        // cannot starve the label.
        let mut wide = SupervisorPanel::new();
        wide.set_snapshot(&SupervisorSnapshot {
            agents: Vec::new(),
            tasks: vec![TaskRow {
                command: "make".to_string(),
                state: TaskState::Failed,
                exit_code: Some(101),
                ..task("t9", TaskState::Failed)
            }],
            sessions: Vec::new(),
        });
        wide.set_tab(PanelTab::Tasks);
        let row = strip_escapes(&wide.render(20, 2)[1]);
        assert_eq!(visible_width(&row), 20, "{row:?}");
        assert!(row.contains("make"), "label starved: {row:?}");
    }

    #[test]
    fn empty_state_message() {
        let mut panel = SupervisorPanel::new();
        panel.set_snapshot(&SupervisorSnapshot::default());
        for (tab, message) in [
            (PanelTab::Agents, "(no agents)"),
            (PanelTab::Tasks, "(no tasks)"),
            (PanelTab::Sessions, "(no sessions)"),
        ] {
            panel.set_tab(tab);
            let rows = panel.render(40, 4);
            assert_eq!(rows.len(), 4, "panel did not fill its height");
            assert_eq!(strip_escapes(&rows[1]), message);
            assert!(
                rows[1].contains(DIM),
                "placeholder is not dim: {:?}",
                rows[1]
            );
            // Padding rows are empty, not spaces: a cleared region must not
            // leave a ruled line behind.
            assert!(rows[2].is_empty() && rows[3].is_empty(), "{rows:?}");
        }
    }

    #[test]
    fn respects_height_and_width() {
        let panel = panel_with_agents(6);

        // Height: exactly `height` rows, whatever the list length.
        assert_eq!(panel.render(40, 1).len(), 1, "header alone");
        assert_eq!(panel.render(40, 3).len(), 3);
        assert_eq!(panel.render(40, 20).len(), 20, "short list padded");
        assert!(panel.render(40, 0).is_empty());
        assert!(panel.render(0, 4).is_empty());

        // Width: no row may exceed the budget, including with a CJK label.
        let mut wide = SupervisorPanel::new();
        wide.set_snapshot(&SupervisorSnapshot {
            agents: vec![AgentRun {
                description: Some("漢字の説明文がとても長い場合".to_string()),
                ..agent("spawn", AgentState::Failed)
            }],
            tasks: Vec::new(),
            sessions: Vec::new(),
        });
        for width in 0..=12u16 {
            for row in wide.render(width, 4) {
                assert!(
                    visible_width(&row) <= width,
                    "width {width}: {row:?} painted {} columns",
                    visible_width(&row)
                );
            }
        }
    }

    /// Panel whose task commands are the given raw strings.
    fn panel_with_commands(commands: &[&str]) -> SupervisorPanel {
        let mut panel = SupervisorPanel::new();
        panel.set_snapshot(&SupervisorSnapshot {
            agents: Vec::new(),
            tasks: commands
                .iter()
                .enumerate()
                .map(|(i, command)| TaskRow {
                    command: (*command).to_string(),
                    ..task(&format!("t{i}"), TaskState::Running)
                })
                .collect(),
            sessions: Vec::new(),
        });
        panel.set_tab(PanelTab::Tasks);
        panel
    }

    /// Every escape sequence in `row` must be one the panel itself emitted.
    fn assert_no_injected_escape(row: &str, allowed: &[&str]) {
        let mut rest = row;
        while let Some(start) = rest.find('\x1b') {
            let tail = &rest[start..];
            let known = allowed.iter().find(|seq| tail.starts_with(**seq));
            let seq = known.unwrap_or_else(|| panic!("injected escape in {row:?}"));
            rest = &tail[seq.len()..];
        }
    }

    #[test]
    fn command_text_cannot_inject_escape_sequences() {
        // A task's command line is raw user or model input. Rendering it
        // verbatim let it override the row's tone (SGR) and, worse, open an
        // OSC 8 hyperlink that `RESET` does not close, so every following row
        // stayed clickable to the attacker's URL.
        let panel = panel_with_commands(&[
            "echo '\x1b[31mRED'",
            "echo '\x1b]8;;https://evil.example\x07click\x1b]8;;\x07'",
        ]);
        let rows = panel.render(80, 4);
        assert_eq!(rows.len(), 4);

        let allowed = [DIM, RESET, GREEN, RED, CURSOR_ON, CURSOR_OFF];
        for row in &rows[1..3] {
            assert_no_injected_escape(row, &allowed);
            assert!(!row.contains('\x07'), "BEL leaked: {row:?}");
        }
        // The text itself survives; only the escapes are gone.
        assert!(
            strip_escapes(&rows[1]).contains("echo '[31mRED'"),
            "{:?}",
            strip_escapes(&rows[1])
        );
        assert!(
            strip_escapes(&rows[2]).contains("click"),
            "{:?}",
            strip_escapes(&rows[2])
        );
    }

    #[test]
    fn command_text_cannot_inject_newlines() {
        // A newline in a command would add rows the viewport budget never
        // accounted for, corrupting every row painted below the panel.
        let panel = panel_with_commands(&["echo a\nb\r\nc"]);
        let rows = panel.render(40, 3);
        assert_eq!(rows.len(), 3, "panel painted extra rows: {rows:?}");
        for row in &rows {
            assert!(
                !row.contains('\n') && !row.contains('\r'),
                "line break leaked: {row:?}"
            );
        }
        assert!(
            strip_escapes(&rows[1]).contains("echo ab c")
                || strip_escapes(&rows[1]).contains("echo abc"),
            "{:?}",
            strip_escapes(&rows[1])
        );
    }

    #[test]
    fn control_bytes_never_reach_the_terminal() {
        // A fuzz over the control range: every row must still be exactly one
        // line and stay inside its column budget.
        let raw: String = (0u8..0x20).map(|b| b as char).collect();
        let panel = panel_with_commands(&[&format!("a{raw}b")]);
        for width in 1..=40u16 {
            let rows = panel.render(width, 4);
            assert_eq!(rows.len(), 4, "width {width}");
            for row in &rows {
                assert!(
                    !row.chars().any(|ch| ch.is_control() && ch != '\x1b'),
                    "control byte leaked at width {width}: {row:?}"
                );
                assert!(
                    visible_width(row) <= width,
                    "width {width}: {row:?} painted {}",
                    visible_width(row)
                );
            }
        }
    }

    #[test]
    fn resize_keeps_selection_visible() {
        // Twenty rows, tall window, selection at the bottom: the window never
        // scrolled, so the selection was visible. Shrinking the terminal to
        // four rows must scroll it back into view — otherwise Enter stops a
        // task that is not on screen.
        let mut panel = panel_with_agents(20);
        for _ in 0..19 {
            panel.handle_input(key(KeyCode::Down));
        }
        assert_eq!(panel.selected_index(), 19);
        panel.render(40, 21);
        assert_eq!(panel.scroll_offset(), 0, "tall window should not scroll");

        let rows = panel.render(40, 4);
        assert_eq!(rows.len(), 4);
        assert!(
            panel.scroll_offset() > 0,
            "resize did not scroll: offset {}",
            panel.scroll_offset()
        );
        let plain: Vec<String> = rows.iter().map(|r| strip_escapes(r)).collect();
        assert!(
            plain.iter().any(|r| r.contains("description spawn-19")),
            "selection scrolled out of view: {plain:?}"
        );
        // And the marker is really painted, not just inside the window.
        assert!(
            plain.iter().any(|r| r.starts_with(MARKER)),
            "no selection marker painted: {plain:?}"
        );

        // Growing the window back re-clamps the offset so the list is not
        // scrolled past its end.
        panel.render(40, 25);
        assert_eq!(panel.scroll_offset(), 0, "offset not clamped on grow");
    }

    #[test]
    fn shrinking_list_keeps_selection_in_bounds() {
        let mut panel = panel_with_agents(20);
        for _ in 0..19 {
            panel.handle_input(key(KeyCode::Down));
        }
        panel.render(40, 5);
        assert!(panel.scroll_offset() > 0);

        // A snapshot that shrinks under the window must leave the offset inside
        // the new list, or the panel would paint blank rows above an empty
        // region; the selected row has to stay painted as well.
        panel.set_snapshot(&SupervisorSnapshot {
            agents: (0..3)
                .map(|i| agent(&format!("spawn-{i:02}"), AgentState::Running))
                .collect(),
            tasks: Vec::new(),
            sessions: Vec::new(),
        });
        assert_eq!(panel.selected_index(), 2);
        assert!(
            panel.scroll_offset() <= 2,
            "offset {} is past the shortened list",
            panel.scroll_offset()
        );
        let plain: Vec<String> = panel
            .render(40, 5)
            .iter()
            .map(|r| strip_escapes(r))
            .collect();
        assert!(
            plain.iter().any(|r| r.contains("description spawn-02")),
            "selection lost after the list shrank: {plain:?}"
        );
        assert_eq!(
            panel.scroll_offset(),
            0,
            "offset did not collapse to the top of the short list"
        );
    }
}
