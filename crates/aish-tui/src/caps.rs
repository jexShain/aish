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

use crate::renderer::TerminalCaps;

/// Environment variable overriding synchronized-output support.
pub const SYNC_ENV: &str = "AISH_TUI_SYNC";

/// Fallback geometry when the terminal cannot be queried (pipes, CI).
pub const DEFAULT_WIDTH: u16 = 80;
/// Fallback geometry when the terminal cannot be queried (pipes, CI).
pub const DEFAULT_HEIGHT: u16 = 24;

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
    // A pty can report zero for either axis (detached, mid-resize, emulators
    // that answer nothing). Fall back per axis rather than clamping to 1: a
    // one-column geometry silently truncates every row to a single glyph.
    let (width, height) = match crossterm::terminal::size() {
        Ok((w, h)) => {
            if w == 0 || h == 0 {
                tracing::warn!(width = w, height = h, "terminal reported empty geometry");
            }
            (
                if w == 0 { DEFAULT_WIDTH } else { w },
                if h == 0 { DEFAULT_HEIGHT } else { h },
            )
        }
        Err(err) => {
            tracing::debug!(%err, "terminal size unavailable; using fallback geometry");
            (DEFAULT_WIDTH, DEFAULT_HEIGHT)
        }
    };
    TerminalCaps {
        synchronized_output: sync_override().unwrap_or(true),
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_is_never_zero() {
        let caps = detect_caps();
        assert!(caps.width > 0);
        assert!(caps.height > 0);
    }
}
