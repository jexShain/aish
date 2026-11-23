//! The fullscreen workspace: layout for the alternate-screen UI.
//!
//! Layout is a pure function of `(snapshot, ui, width, height)`: it fills a
//! [`Screen`] and returns the hotspot table for mouse/selection hit-testing.
//! Nothing here reads the clock, the filesystem or the shell — the caller
//! supplies a [`Workspace`] and gets cells back, which is what makes a
//! golden-file test possible.
//!
//! The shape follows the reference panel (`troupe`), reduced to what aish
//! actually has: one process, one supervisor snapshot, four tabs.

use crate::render::{visible_width, TAB_WIDTH};
use crate::screen::{CellStyle, Screen};

/// Palette, as 24-bit values. Semantics rather than names, so a theme can
/// re-point them without touching the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Body text.
    pub bright: (u8, u8, u8),
    /// Selection and links.
    pub accent: (u8, u8, u8),
    /// Secondary chrome: rules, footers, borders.
    pub chrome: (u8, u8, u8),
    /// Waiting on the user.
    pub warn: (u8, u8, u8),
    /// Finished successfully.
    pub ok: (u8, u8, u8),
    /// Failed.
    pub error: (u8, u8, u8),
    /// De-emphasised text.
    pub dim: (u8, u8, u8),
    /// The orchestrator.
    pub violet: (u8, u8, u8),
}

impl Palette {
    /// The default palette, matching the reference panel's tones.
    pub const fn default_palette() -> Self {
        Self {
            bright: (0xe8, 0xea, 0xf0),
            accent: (0x6f, 0xa8, 0xff),
            chrome: (0x4e, 0x69, 0x91),
            warn: (0xf4, 0xbe, 0x5c),
            ok: (0x98, 0xc3, 0x79),
            error: (0xe0, 0x6c, 0x75),
            dim: (0x9a, 0xa3, 0xad),
            violet: (0xb9, 0x94, 0xf0),
        }
    }

    fn style(&self, rgb: (u8, u8, u8)) -> CellStyle {
        CellStyle::fg(rgb)
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::default_palette()
    }
}

/// Tabs of the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// Roles, stages, activity, flow.
    #[default]
    Mission,
    /// Flat agent list.
    Agents,
    /// Background tasks.
    Tasks,
    /// Event stream.
    Log,
}

impl Tab {
    /// Every tab, in display order.
    pub const ALL: [Tab; 4] = [Tab::Mission, Tab::Agents, Tab::Tasks, Tab::Log];

    /// The tab's label.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Mission => "MISSION",
            Tab::Agents => "AGENTS",
            Tab::Tasks => "TASKS",
            Tab::Log => "LOG",
        }
    }

    /// Next tab, wrapping.
    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Previous tab, wrapping.
    pub fn previous(self) -> Self {
        let index = Self::ALL.iter().position(|t| *t == self).unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// Tab for a `1`-based key press, if it names one.
    pub fn from_digit(digit: u8) -> Option<Self> {
        let index = usize::from(digit).checked_sub(1)?;
        Self::ALL.get(index).copied()
    }
}

/// One agent row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentLine {
    /// Display name.
    pub name: String,
    /// Lifecycle state, driving the glyph and the tone.
    pub state: AgentState,
    /// Short activity string, e.g. the current tool.
    pub activity: String,
    /// Elapsed time, already formatted.
    pub elapsed: String,
}

/// Agent lifecycle, in the order the reference panel resolves them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentState {
    /// Planned but not dispatched.
    #[default]
    Planned,
    /// Running.
    Running,
    /// Blocked on the user.
    Waits,
    /// Finished.
    Done,
    /// Failed.
    Failed,
}

impl AgentState {
    /// The single-cell glyph for this state.
    pub fn glyph(self) -> char {
        match self {
            AgentState::Planned => '○',
            AgentState::Running => '●',
            AgentState::Waits => '◆',
            AgentState::Done => '✓',
            AgentState::Failed => '✗',
        }
    }

    /// Lower-case label.
    pub fn label(self) -> &'static str {
        match self {
            AgentState::Planned => "planned",
            AgentState::Running => "running",
            AgentState::Waits => "waits",
            AgentState::Done => "done",
            AgentState::Failed => "failed",
        }
    }

    fn colour(self, palette: &Palette) -> (u8, u8, u8) {
        match self {
            AgentState::Planned => palette.dim,
            AgentState::Running => palette.accent,
            AgentState::Waits => palette.warn,
            AgentState::Done => palette.ok,
            AgentState::Failed => palette.error,
        }
    }
}

/// One stage of the current plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageLine {
    /// Stage name.
    pub name: String,
    /// Whether it is finished.
    pub done: bool,
    /// Whether it is the current one.
    pub current: bool,
    /// Progress label, e.g. `2/3`.
    pub progress: String,
}

/// One background task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLine {
    /// Task id, as shown to the user.
    pub id: String,
    /// Command or one-line summary.
    pub summary: String,
    /// Elapsed time, already formatted.
    pub elapsed: String,
    /// Whether it has finished.
    pub finished: bool,
}

/// One event-log row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Timestamp, `HH:MM`.
    pub at: String,
    /// Event text.
    pub text: String,
}

/// Everything the layout paints, as plain data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    /// Session title, shown in the title bar.
    pub title: String,
    /// Right-aligned goal line.
    pub goal: String,
    /// Agents.
    pub agents: Vec<AgentLine>,
    /// Stages.
    pub stages: Vec<StageLine>,
    /// Background tasks.
    pub tasks: Vec<TaskLine>,
    /// Event log.
    pub log: Vec<LogLine>,
    /// Clock, `HH:MM`.
    pub clock: String,
    /// Context-window usage, 0..=100.
    pub context_percent: u8,
    /// Selected row in the active tab.
    pub selected: usize,
    /// Active tab.
    pub tab: Tab,
}

/// Minimum usable geometry; below it the screen is a single message.
pub const MIN_WIDTH: u16 = 40;
/// Minimum usable height.
pub const MIN_HEIGHT: u16 = 10;

/// Fixed left column width on the MISSION tab.
pub const LEFT_COLUMN: u16 = 27;

/// Columns between two tab labels.
const TAB_GAP: u16 = 4;

/// Column (relative to the detail header) where a field's value starts.
const DETAIL_VALUE_COLUMN: u16 = 10;

/// What the layout produced besides the cells.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drawn {
    /// Rows the body overflowed by; the caller scrolls by this much.
    pub overflow: u16,
}

/// Paint `workspace` into `screen`.
///
/// Rows are `title / tabs / goal / rule / body... / input / hints`; the body
/// takes whatever height is left, and the two footer rows are never
/// compressed (a clipped footer hides the key that gets the user out).
pub fn draw(workspace: &Workspace, palette: &Palette, screen: &mut Screen) -> Drawn {
    screen.clear();
    let width = screen.width();
    let height = screen.height();

    if width < MIN_WIDTH || height < MIN_HEIGHT {
        let msg = format!("aish fullscreen needs {MIN_WIDTH} columns and {MIN_HEIGHT} rows");
        screen.put(0, 0, &truncate(&msg, width), &palette.style(palette.error));
        return Drawn::default();
    }

    draw_title_bar(workspace, palette, screen, width);
    draw_tabs(workspace, palette, screen, width);
    draw_goal(workspace, palette, screen, width);
    screen.fill_row(0, 3, width, '─', &palette.style(palette.chrome));

    // Two footer rows: the input line and the key hints.
    let input_row = height - 2;
    let hint_row = height - 1;
    let body_top = 4;
    let body_height = input_row.saturating_sub(body_top);

    let drawn = match workspace.tab {
        Tab::Mission => draw_mission(workspace, palette, screen, body_top, body_height),
        Tab::Agents => draw_agents(workspace, palette, screen, body_top, body_height),
        Tab::Tasks => draw_tasks(workspace, palette, screen, body_top, body_height),
        Tab::Log => draw_log(workspace, palette, screen, body_top, body_height),
    };

    draw_input(workspace, palette, screen, input_row);
    draw_hints(workspace, palette, screen, width, hint_row);
    drawn
}

fn draw_title_bar(workspace: &Workspace, palette: &Palette, screen: &mut Screen, width: u16) {
    let waiting = workspace
        .agents
        .iter()
        .filter(|a| a.state == AgentState::Waits)
        .count();
    let mut right = String::new();
    if waiting > 0 {
        right.push_str(&format!("◆ {waiting} needs you  ·  "));
    }
    if !workspace.clock.is_empty() {
        right.push_str(&workspace.clock);
    }

    // The title owns whatever the right segment leaves, minus one column of
    // air: writing the two independently is how "auth-refresh◆ 1 needs you"
    // happens.
    // `put_right` ends at the column before `right`, and the title needs one
    // blank column of its own: two columns of air between the two segments.
    let right_width = if right.is_empty() {
        0
    } else {
        visible_width(&right) + 2
    };
    let title_budget = width.saturating_sub(right_width);
    let title = if workspace.title.is_empty() {
        "aish".to_string()
    } else {
        format!("aish  {}", workspace.title)
    };
    screen.put(
        0,
        0,
        &truncate(&title, title_budget),
        &palette.style(palette.bright).bolded(),
    );

    if !right.is_empty() {
        let style = if waiting > 0 {
            palette.style(palette.warn)
        } else {
            palette.style(palette.dim)
        };
        screen.put_right(width.saturating_sub(1), 0, &right, &style);
    }
}

fn draw_tabs(workspace: &Workspace, palette: &Palette, screen: &mut Screen, width: u16) {
    let mut col = 1;
    for (index, tab) in Tab::ALL.iter().enumerate() {
        let label = format!("{} {}", index + 1, tab.label());
        // A tab stop after each label: the gap is what keeps two labels from
        // reading as one when the row is copied out of the terminal.
        let step = visible_width(&label) + TAB_GAP;
        if usize::from(col) + usize::from(step) > usize::from(width) {
            break;
        }

        let style = if *tab == workspace.tab {
            palette.style(palette.accent).bolded()
        } else {
            palette.style(palette.chrome)
        };
        screen.put(col, 1, &label, &style);
        col += step;
    }
}

fn draw_goal(workspace: &Workspace, palette: &Palette, screen: &mut Screen, width: u16) {
    if workspace.goal.is_empty() {
        return;
    }
    screen.put_right(
        width.saturating_sub(1),
        2,
        &workspace.goal,
        &palette.style(palette.dim),
    );
}

fn draw_mission(
    workspace: &Workspace,
    palette: &Palette,
    screen: &mut Screen,
    top: u16,
    height: u16,
) -> Drawn {
    let width = screen.width();
    let split = LEFT_COLUMN.min(width.saturating_sub(10));
    for row in top..top + height {
        screen.put(split, row, "│", &palette.style(palette.chrome));
    }

    draw_section("ROLES", palette, screen, 0, top, split);
    let mut row = top + 1;
    for (index, agent) in workspace.agents.iter().enumerate() {
        if row >= top + height {
            break;
        }
        let selected = index == workspace.selected;
        let marker = if selected { "▸ " } else { "  " };
        let glyph = agent.state.glyph();
        let line = format!(
            "{marker}{glyph} {}",
            truncate(&agent.name, split.saturating_sub(6))
        );
        let style = if selected {
            palette.style(palette.accent).bolded()
        } else {
            palette.style(agent.state.colour(palette))
        };
        screen.put(0, row, &line, &style);
        row += 1;
    }

    row += 1;
    if row < top + height {
        draw_section("STAGES", palette, screen, 0, row, split);
        row += 1;
        for stage in &workspace.stages {
            if row >= top + height {
                break;
            }
            let glyph = if stage.done {
                '✓'
            } else if stage.current {
                '●'
            } else {
                '○'
            };
            let tail = if !stage.progress.is_empty() {
                format!("  {}", stage.progress)
            } else {
                String::new()
            };
            let label = format!(
                "  {glyph} {}{tail}",
                truncate(&stage.name, split.saturating_sub(10))
            );
            let style = if stage.done {
                palette.style(palette.ok)
            } else if stage.current {
                palette.style(palette.accent)
            } else {
                palette.style(palette.dim)
            };
            screen.put(0, row, &label, &style);
            row += 1;
        }
    }

    row += 1;
    if row < top + height {
        draw_section("ACTIVITY", palette, screen, 0, row, split);
        row += 1;
        for entry in &workspace.log {
            if row >= top + height {
                break;
            }
            let line = format!("  {} {}", entry.at, entry.text);
            screen.put(0, row, &truncate(&line, split), &palette.style(palette.dim));
            row += 1;
        }
    }

    // Right column: flow placeholder plus the selected agent's detail.
    let right = split + 2;
    let right_width = width.saturating_sub(right);
    let mut detail_row = top;
    if let Some(agent) = workspace.agents.get(workspace.selected) {
        let header = format!("─ ○ {} · {} ", agent.name, agent.state.label());
        let rule = "─".repeat(right_width.saturating_sub(visible_width(&header) as u16) as usize);
        screen.put(
            right,
            detail_row,
            &format!("{header}{rule}"),
            &palette.style(palette.chrome),
        );
        detail_row += 1;
        let fields = [
            ("state", agent.state.label().to_string()),
            ("activity", agent.activity.clone()),
            ("elapsed", agent.elapsed.clone()),
        ];
        for (name, value) in fields {
            if detail_row >= top + height {
                break;
            }
            screen.put(right, detail_row, name, &palette.style(palette.dim));
            // One blank column between the label column and the value, so a
            // value can never run into its own label.
            screen.put(
                right + DETAIL_VALUE_COLUMN,
                detail_row,
                &truncate(&value, right_width.saturating_sub(DETAIL_VALUE_COLUMN)),
                &palette.style(palette.bright),
            );
            detail_row += 1;
        }
    } else {
        screen.put(
            right,
            detail_row,
            "no agent selected",
            &palette.style(palette.dim),
        );
    }

    Drawn {
        overflow: (row.saturating_sub(top)).saturating_sub(height),
    }
}

fn draw_agents(
    workspace: &Workspace,
    palette: &Palette,
    screen: &mut Screen,
    top: u16,
    height: u16,
) -> Drawn {
    let width = screen.width();
    // Column starts, not widths: the widest state is "▸✗ planned" (10), so the
    // name column starts past it. Hard-coding a narrower gap is what made long
    // states run into the name ("plannereview").
    const STATE_COL: u16 = 1;
    const NAME_COL: u16 = 12;
    const ACTIVITY_COL: u16 = 32;
    screen.put(STATE_COL, top, "STATE", &palette.style(palette.chrome));
    screen.put(NAME_COL, top, "NAME", &palette.style(palette.chrome));
    screen.put(
        ACTIVITY_COL,
        top,
        "ACTIVITY",
        &palette.style(palette.chrome),
    );
    screen.put_right(
        width.saturating_sub(2),
        top,
        "ELAPSED",
        &palette.style(palette.chrome),
    );

    let mut row = top + 1;
    for (index, agent) in workspace.agents.iter().enumerate() {
        if row >= top + height {
            break;
        }
        let selected = index == workspace.selected;
        let marker = if selected { "▸" } else { " " };
        let state = format!("{marker}{} {}", agent.state.glyph(), agent.state.label());
        let style = if selected {
            palette.style(palette.accent).bolded()
        } else {
            palette.style(palette.bright)
        };
        screen.put(STATE_COL, row, &state, &style);
        screen.put(
            NAME_COL,
            row,
            &truncate(&agent.name, ACTIVITY_COL - NAME_COL - 1),
            &style,
        );
        screen.put(
            ACTIVITY_COL,
            row,
            &truncate(&agent.activity, width.saturating_sub(ACTIVITY_COL + 12)),
            &palette.style(palette.dim),
        );
        screen.put_right(
            width.saturating_sub(2),
            row,
            &agent.elapsed,
            &palette.style(palette.dim),
        );
        row += 1;
    }
    Drawn {
        overflow: row.saturating_sub(top + height),
    }
}

fn draw_tasks(
    workspace: &Workspace,
    palette: &Palette,
    screen: &mut Screen,
    top: u16,
    height: u16,
) -> Drawn {
    let width = screen.width();
    const ID_COL: u16 = 1;
    const COMMAND_COL: u16 = 18;
    screen.put(ID_COL, top, "ID", &palette.style(palette.chrome));
    screen.put(COMMAND_COL, top, "COMMAND", &palette.style(palette.chrome));
    screen.put_right(
        width.saturating_sub(2),
        top,
        "ELAPSED",
        &palette.style(palette.chrome),
    );

    let mut row = top + 1;
    for (index, task) in workspace.tasks.iter().enumerate() {
        if row >= top + height {
            break;
        }
        let glyph = if task.finished { '✓' } else { '●' };
        let colour = if task.finished {
            palette.ok
        } else {
            palette.accent
        };
        let selected = index == workspace.selected;
        let marker = if selected { "▸" } else { " " };
        let style = if selected {
            palette.style(palette.accent).bolded()
        } else {
            palette.style(colour)
        };
        screen.put(
            ID_COL,
            row,
            &format!(
                "{marker}{glyph} {}",
                truncate(&task.id, COMMAND_COL - ID_COL - 4)
            ),
            &style,
        );
        screen.put(
            COMMAND_COL,
            row,
            &truncate(&task.summary, width.saturating_sub(COMMAND_COL + 12)),
            &palette.style(palette.bright),
        );
        screen.put_right(
            width.saturating_sub(2),
            row,
            &task.elapsed,
            &palette.style(palette.dim),
        );
        row += 1;
    }
    Drawn {
        overflow: row.saturating_sub(top + height),
    }
}

fn draw_log(
    workspace: &Workspace,
    palette: &Palette,
    screen: &mut Screen,
    top: u16,
    height: u16,
) -> Drawn {
    let width = screen.width();
    let mut row = top;
    for entry in &workspace.log {
        if row >= top + height {
            break;
        }
        screen.put(1, row, &entry.at, &palette.style(palette.chrome));
        screen.put(
            8,
            row,
            &truncate(&entry.text, width.saturating_sub(9)),
            &palette.style(palette.bright),
        );
        row += 1;
    }
    Drawn {
        overflow: row.saturating_sub(top + height),
    }
}

fn draw_input(workspace: &Workspace, palette: &Palette, screen: &mut Screen, row: u16) {
    let _ = workspace;
    screen.put(0, row, "❯", &palette.style(palette.accent));
    let ctx = format!("ctx {}%", workspace.context_percent);
    screen.put_right(
        screen.width().saturating_sub(1),
        row,
        &ctx,
        &palette.style(palette.dim),
    );
}

fn draw_hints(workspace: &Workspace, palette: &Palette, screen: &mut Screen, width: u16, row: u16) {
    let hints = match workspace.tab {
        Tab::Mission => {
            "↑↓ role   ⏎ detail   Tab tabs   1-4 tab   o open   s send   r stop   ? help   q close"
        }
        _ => "Tab tabs   1-4 tab   ↑↓ select   r stop   ? help   q close",
    };
    screen.put(
        1,
        row,
        &truncate(hints, width.saturating_sub(2)),
        &palette.style(palette.chrome),
    );
}

fn draw_section(label: &str, palette: &Palette, screen: &mut Screen, x: u16, row: u16, width: u16) {
    let head = format!("─ {label} ");
    let rest = "─".repeat(width.saturating_sub(visible_width(&head) as u16) as usize);
    screen.put(
        x,
        row,
        &format!("{head}{rest}"),
        &palette.style(palette.chrome),
    );
}

/// Clip `text` to `width` display columns, never splitting a wide glyph.
fn truncate(text: &str, width: u16) -> String {
    let limit = width as usize;
    if usize::from(visible_width(text)) <= limit {
        return text.to_string();
    }
    let mut used = 0usize;
    let mut out = String::new();
    for ch in text.chars() {
        let advance = if ch == '\t' {
            TAB_WIDTH - (used % TAB_WIDTH)
        } else {
            unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)
        };
        if used + advance > limit {
            break;
        }
        used += advance;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette::default()
    }

    fn sample() -> Workspace {
        Workspace {
            title: "auth-refresh".to_string(),
            goal: "Rotate tokens on every use".to_string(),
            agents: vec![
                AgentLine {
                    name: "orchestrator".to_string(),
                    state: AgentState::Running,
                    activity: "planning".to_string(),
                    elapsed: "0:42".to_string(),
                },
                AgentLine {
                    name: "build".to_string(),
                    state: AgentState::Waits,
                    activity: "Trust this folder?".to_string(),
                    elapsed: "1:03".to_string(),
                },
                AgentLine {
                    name: "review".to_string(),
                    state: AgentState::Planned,
                    activity: String::new(),
                    elapsed: "—".to_string(),
                },
            ],
            stages: vec![
                StageLine {
                    name: "Plan".to_string(),
                    done: true,
                    current: false,
                    progress: String::new(),
                },
                StageLine {
                    name: "Build".to_string(),
                    done: false,
                    current: true,
                    progress: "0/3".to_string(),
                },
            ],
            tasks: vec![TaskLine {
                id: "t1a2060".to_string(),
                summary: "cargo test --workspace".to_string(),
                elapsed: "0:12".to_string(),
                finished: false,
            }],
            log: vec![LogLine {
                at: "12:07".to_string(),
                text: "dispatched build".to_string(),
            }],
            clock: "12:07".to_string(),
            context_percent: 4,
            selected: 1,
            tab: Tab::Mission,
        }
    }

    /// One row as text, with a space standing in for every empty cell so
    /// columns keep their real offsets.
    fn row_text(screen: &Screen, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| match screen.cell(x, y) {
                Some(cell) if !cell.ch.is_empty() => cell.ch.clone(),
                _ => " ".to_string(),
            })
            .collect()
    }

    #[test]
    fn too_small_a_screen_draws_one_message() {
        let mut screen = Screen::new(30, 8);
        draw(&sample(), &palette(), &mut screen);
        let text = row_text(&screen, 0, 30);
        assert!(text.contains("40 colum"), "message clipped away: {text:?}");
        // Everything below the message stays blank rather than half-drawn.
        assert!(row_text(&screen, 1, 30).trim().is_empty());
    }

    #[test]
    fn title_bar_carries_the_name_the_clock_and_the_waiting_count() {
        let mut screen = Screen::new(100, 30);
        draw(&sample(), &palette(), &mut screen);
        let title = row_text(&screen, 0, 100);
        assert!(title.contains("aish"), "{title:?}");
        assert!(title.contains("auth-refresh"), "{title:?}");
        assert!(title.contains("1 needs you"), "{title:?}");
        assert!(title.contains("12:07"), "{title:?}");
    }

    #[test]
    fn title_and_right_segment_never_collide() {
        let mut screen = Screen::new(100, 30);
        draw(&sample(), &palette(), &mut screen);
        let title = row_text(&screen, 0, 100);
        assert!(!title.contains("auth-refresh◆"), "no air: {title:?}");
        // The right segment still ends at the last column.
        assert!(title.trim_end().ends_with("12:07"), "{title:?}");
    }

    #[test]
    fn detail_values_are_separated_from_their_labels() {
        let mut screen = Screen::new(100, 30);
        draw(&sample(), &palette(), &mut screen);
        let body: Vec<String> = (4..20).map(|y| row_text(&screen, y, 100)).collect();
        let joined = body.join("\n");
        assert!(
            !joined.contains("statewaits"),
            "label ran into value: {joined}"
        );
        assert!(joined.contains("state"), "{joined}");
        assert!(joined.contains("waits"), "{joined}");
    }

    #[test]
    fn tabs_highlight_the_active_one() {
        let mut workspace = sample();
        workspace.tab = Tab::Tasks;
        let mut screen = Screen::new(100, 30);
        draw(&workspace, &palette(), &mut screen);
        let tabs = row_text(&screen, 1, 100);
        assert!(tabs.contains("3 TASKS"), "{tabs:?}");
        assert!(!tabs.contains("MISSION2"), "tabs ran together: {tabs:?}");
        // Every label is followed by at least one blank column.
        for label in ["1 MISSION", "2 AGENTS", "3 TASKS", "4 LOG"] {
            let at = tabs.find(label).expect(label) + label.len();
            assert_eq!(
                &tabs[at..at + 1],
                " ",
                "{label} not followed by air: {tabs:?}"
            );
        }
        // The active tab is bold, an inactive one is not.
        // The label starts one column after its tab number.
        let number_x = screen
            .cells_row(1)
            .position(|c| c.ch == "3")
            .expect("tasks number painted") as u16;
        assert!(screen.cell(number_x, 1).unwrap().bold);
        assert!(screen.cell(number_x + 2, 1).unwrap().bold, "label not bold");
    }

    #[test]
    fn mission_splits_at_the_fixed_left_column() {
        let mut screen = Screen::new(100, 30);
        draw(&sample(), &palette(), &mut screen);
        for y in 4..28 {
            assert_eq!(
                screen.cell(LEFT_COLUMN, y).unwrap().ch,
                "│",
                "missing split at row {y}"
            );
        }
    }

    #[test]
    fn selected_agent_is_marked_and_detailed() {
        let mut screen = Screen::new(100, 30);
        draw(&sample(), &palette(), &mut screen);
        let body: Vec<String> = (4..20).map(|y| row_text(&screen, y, 100)).collect();
        let joined = body.join("\n");
        assert!(joined.contains("▸"), "no selection marker: {joined}");
        // The detail column describes the same agent the marker points at.
        assert!(joined.contains("waits"), "{joined}");
        assert!(joined.contains("Trust this folder?"), "{joined}");
    }

    #[test]
    fn stages_show_their_state_glyphs() {
        let mut screen = Screen::new(100, 30);
        draw(&sample(), &palette(), &mut screen);
        let body: Vec<String> = (4..28).map(|y| row_text(&screen, y, 100)).collect();
        let joined = body.join("\n");
        assert!(joined.contains("✓ Plan"), "{joined}");
        assert!(joined.contains("● Build"), "{joined}");
        assert!(joined.contains("0/3"), "{joined}");
    }

    #[test]
    fn agents_tab_lists_every_agent() {
        let mut workspace = sample();
        workspace.tab = Tab::Agents;
        let mut screen = Screen::new(100, 30);
        draw(&workspace, &palette(), &mut screen);
        let body: Vec<String> = (4..28).map(|y| row_text(&screen, y, 100)).collect();
        let joined = body.join("\n");
        for name in ["orchestrator", "build", "review"] {
            assert!(joined.contains(name), "{name} missing: {joined}");
        }
    }

    #[test]
    fn agents_tab_columns_never_run_together() {
        // "planned" and "running" are the long states; with a narrow state
        // column they used to spill into the name ("plannereview").
        let mut workspace = sample();
        workspace.tab = Tab::Agents;
        workspace.agents[2].state = AgentState::Running;
        let mut screen = Screen::new(100, 30);
        draw(&workspace, &palette(), &mut screen);
        // Rows: header at 4, then one row per agent from 5.
        let running = row_text(&screen, 5, 100);
        assert!(!running.contains("runninorchestrator"), "{running:?}");
        assert!(running.contains("orchestrator"), "{running:?}");
        let planned = row_text(&screen, 7, 100);
        assert!(!planned.contains("plannereview"), "{planned:?}");
        assert!(planned.contains("review"), "{planned:?}");
    }

    #[test]
    fn tasks_tab_shows_id_and_command() {
        let mut workspace = sample();
        workspace.tab = Tab::Tasks;
        let mut screen = Screen::new(100, 30);
        draw(&workspace, &palette(), &mut screen);
        let body: Vec<String> = (4..28).map(|y| row_text(&screen, y, 100)).collect();
        let joined = body.join("\n");
        assert!(joined.contains("t1a2060"), "{joined}");
        assert!(joined.contains("cargo test"), "{joined}");
    }

    #[test]
    fn log_tab_keeps_the_hint_row_intact() {
        let mut workspace = sample();
        workspace.tab = Tab::Log;
        let mut screen = Screen::new(100, 60);
        draw(&workspace, &palette(), &mut screen);
        let hints = row_text(&screen, 59, 100);
        assert!(hints.contains("q close"), "footer compressed: {hints:?}");
    }

    #[test]
    fn body_overflow_is_reported_not_painted_over_the_footer() {
        let mut workspace = sample();
        workspace.stages = (0..80)
            .map(|i| StageLine {
                name: format!("stage-{i}"),
                done: false,
                current: false,
                progress: String::new(),
            })
            .collect();
        let mut screen = Screen::new(100, 24);
        let drawn = draw(&workspace, &palette(), &mut screen);
        assert!(drawn.overflow > 0, "overflow not reported");
        // The footer still holds the key hints, and the last row is not body.
        let hints = row_text(&screen, 23, 100);
        assert!(hints.contains("q close"), "{hints:?}");
    }

    #[test]
    fn wide_glyph_names_do_not_shift_the_split() {
        let mut workspace = sample();
        workspace.agents[0].name = "编排者超长名字".to_string();
        let mut screen = Screen::new(100, 30);
        draw(&workspace, &palette(), &mut screen);
        for y in 4..28 {
            assert_eq!(screen.cell(LEFT_COLUMN, y).unwrap().ch, "│", "row {y}");
        }
    }

    #[test]
    fn tab_rotation_wraps_both_ways() {
        assert_eq!(Tab::Log.next(), Tab::Mission);
        assert_eq!(Tab::Mission.previous(), Tab::Log);
        assert_eq!(Tab::from_digit(3), Some(Tab::Tasks));
        assert_eq!(Tab::from_digit(9), None);
        assert_eq!(Tab::from_digit(0), None);
    }

    #[test]
    fn truncate_never_splits_a_wide_glyph() {
        // Three columns fit exactly one CJK glyph, not one and a half.
        assert_eq!(truncate("中文字", 3), "中");
        assert_eq!(truncate("abc", 2), "ab");
        assert_eq!(truncate("abc", 10), "abc");
    }
}
