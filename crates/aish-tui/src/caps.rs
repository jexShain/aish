//! Terminal capability detection.
//!
//! v1 deliberately does not probe the terminal with DECRQM (CSI ? 2026 $ p).
//! A probe reply is asynchronous and can be swallowed by tmux/screen or by a
//! program that already owns stdin, so a wrong answer is indistinguishable from
//! a slow one. Instead synchronized output defaults to on — every terminal that
//! does not implement DEC 2026 ignores the private mode harmlessly — and the
//! `AISH_TUI_SYNC=0` / `AISH_TUI_SYNC=1` environment override covers the
//! terminals that do implement it but flicker anyway (Apple Terminal in
//! particular).
//!
//! # Empty geometry
//!
//! A pty reports `0x0` in perfectly ordinary situations: `script -qec`, a
//! detached session, a terminal emulator that has not answered yet, a resize
//! caught mid-flight. That is *not* an anomaly worth a warning — it is the
//! normal startup race of an interactive shell, and logging it on every launch
//! trains users to ignore the log. Zero is resolved silently from
//! `COLUMNS`/`LINES` (which `script` and most terminals export) and then from
//! [`DEFAULT_WIDTH`]`x`[`DEFAULT_HEIGHT`]. Only a report that is non-zero but
//! still unusable (a `1x1` terminal) is worth a warning, because that one
//! really does truncate every row.

use crate::renderer::TerminalCaps;

/// Environment variable overriding synchronized-output support.
pub const SYNC_ENV: &str = "AISH_TUI_SYNC";

/// Fallback width when the terminal cannot be queried (pipes, CI).
pub const DEFAULT_WIDTH: u16 = 80;
/// Fallback height when the terminal cannot be queried (pipes, CI).
pub const DEFAULT_HEIGHT: u16 = 24;

/// Axis value that is reported but cannot be rendered into.
///
/// A single column/row is a real terminal state (a pty resized to nothing yet
/// still attached), so it is kept rather than replaced — but it is anomalous
/// enough to be worth a warning, unlike the routine `0x0` startup report.
const MIN_USABLE_AXIS: u16 = 2;

/// Parse the `AISH_TUI_SYNC` override. `None` means "no override".
pub fn sync_override() -> Option<bool> {
    let raw = std::env::var(SYNC_ENV).ok()?;
    match raw.trim() {
        "1" | "true" | "TRUE" | "yes" | "on" => Some(true),
        "0" | "false" | "FALSE" | "no" | "off" => Some(false),
        other => {
            tracing::warn!(
                value = other,
                "ignoring unrecognized {} (expected 1 or 0)",
                SYNC_ENV
            );
            None
        }
    }
}

/// Detect the terminal geometry and renderer capabilities for the current
/// process' stdout.
pub fn detect_caps() -> TerminalCaps {
    let (width, height) = resolve_geometry_from(crossterm::terminal::size().ok(), env_axis);
    TerminalCaps {
        synchronized_output: sync_override().unwrap_or(true),
        width,
        height,
    }
}

/// Resolve the geometry from what the terminal reported plus the environment.
///
/// `reported` is `None` when the query itself failed. `env` looks up
/// `COLUMNS`/`LINES`, which is where a pty that answers `0x0` usually still
/// publishes its real size.
///
/// A zero axis is resolved silently, per axis: a pty can report zero for width
/// while its height is correct. A non-zero but unusable axis is kept verbatim
/// (clamping it to a normal size would hide a terminal the user has to fix) and
/// warned about, because that is the only case a user can act on.
fn resolve_geometry_from(
    reported: Option<(u16, u16)>,
    env: impl Fn(&str) -> Option<u16>,
) -> (u16, u16) {
    let Some((width, height)) = reported else {
        tracing::debug!("terminal size unavailable; using fallback geometry");
        return (
            env("COLUMNS").unwrap_or(DEFAULT_WIDTH),
            env("LINES").unwrap_or(DEFAULT_HEIGHT),
        );
    };

    if width == 0 || height == 0 {
        // Routine on a pty: silent, and every axis that *is* known is kept.
        let resolved = (
            if width == 0 {
                env("COLUMNS").unwrap_or(DEFAULT_WIDTH)
            } else {
                width
            },
            if height == 0 {
                env("LINES").unwrap_or(DEFAULT_HEIGHT)
            } else {
                height
            },
        );
        tracing::debug!(
            reported_width = width,
            reported_height = height,
            width = resolved.0,
            height = resolved.1,
            "terminal reported empty geometry; using fallback"
        );
        return resolved;
    }

    if width < MIN_USABLE_AXIS || height < MIN_USABLE_AXIS {
        tracing::warn!(
            width,
            height,
            "terminal reported an unusably small geometry; rows will be truncated"
        );
    }
    (width, height)
}

/// Read one axis from the environment, rejecting values that are not a
/// positive integer.
fn env_axis(name: &str) -> Option<u16> {
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No environment fallback: the built-in defaults must apply.
    fn no_env(_: &str) -> Option<u16> {
        None
    }

    #[test]
    fn geometry_zero_is_silent() {
        // `script -qec` and a detached pty both report 0x0 at startup; the
        // resolver must produce a usable geometry without an alarm.
        assert_eq!(
            resolve_geometry_from(Some((0, 0)), no_env),
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        );
    }

    #[test]
    fn geometry_uses_env_fallback() {
        let env = |name: &str| match name {
            "COLUMNS" => Some(132),
            "LINES" => Some(43),
            _ => None,
        };
        assert_eq!(resolve_geometry_from(Some((0, 0)), env), (132, 43));
    }

    #[test]
    fn geometry_resolves_zero_axis_on_its_own() {
        // A pty may answer nothing for width while its height is real; the
        // known axis must survive.
        let env = |name: &str| (name == "COLUMNS").then_some(100);
        assert_eq!(resolve_geometry_from(Some((0, 30)), env), (100, 30));
        assert_eq!(
            resolve_geometry_from(Some((90, 0)), env),
            (90, DEFAULT_HEIGHT)
        );
    }

    #[test]
    fn geometry_keeps_reported_size() {
        assert_eq!(resolve_geometry_from(Some((120, 40)), no_env), (120, 40));
    }

    #[test]
    fn geometry_unavailable_uses_defaults() {
        assert_eq!(
            resolve_geometry_from(None, no_env),
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        );
    }

    #[test]
    fn geometry_small_nonzero_is_kept() {
        // 1x1 is a real (if useless) report: it is kept so the warning names
        // the actual geometry instead of a fabricated one.
        assert_eq!(resolve_geometry_from(Some((1, 1)), no_env), (1, 1));
    }

    #[test]
    fn geometry_is_never_zero() {
        let caps = detect_caps();
        assert!(caps.width > 0);
        assert!(caps.height > 0);
    }

    #[test]
    fn env_axis_rejects_garbage() {
        assert_eq!(env_axis("AISH_TUI_TEST_MISSING_AXIS"), None);
    }
}
