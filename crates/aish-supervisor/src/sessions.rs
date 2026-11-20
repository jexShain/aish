//! Snapshot rows over live PTY daemon sessions.

use aish_pty::{discover_sessions, DaemonSessionInfo};

/// Role a session plays in the multi-client controller arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SessionRole {
    /// Session currently holds the input controller role.
    Controller,
    /// Attached client without input control.
    Observer,
    /// Controller disconnected; role transfer is pending.
    Stale,
}

impl std::fmt::Display for SessionRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            SessionRole::Controller => "controller",
            SessionRole::Observer => "observer",
            SessionRole::Stale => "stale",
        };
        f.write_str(name)
    }
}

/// One live PTY session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub socket_path: String,
    pub daemon_pid: u32,
    pub child_pid: Option<u32>,
    pub cwd: String,
    pub model: String,
    pub name: Option<String>,
    pub role: SessionRole,
    /// Session start time as RFC 3339 UTC (`1970-01-01T00:00:00Z` for unknown).
    pub started_at: String,
}

impl SessionRow {
    pub fn from_info(info: &DaemonSessionInfo) -> Self {
        let role = if info.controller_stale {
            SessionRole::Stale
        } else if info.controller.is_some() {
            SessionRole::Controller
        } else {
            SessionRole::Observer
        };
        Self {
            session_id: info.session_id.clone(),
            socket_path: info.socket_path.to_string_lossy().into_owned(),
            daemon_pid: info.daemon_pid,
            // `0` means the daemon did not record a child pid.
            child_pid: (info.child_pid != 0).then_some(info.child_pid),
            cwd: info.cwd.clone(),
            model: info.model.clone().unwrap_or_default(),
            name: info.name.clone(),
            role,
            started_at: format_unix_secs(info.started_at),
        }
    }
}

/// Discover live PTY sessions.
///
/// Returns an empty vector when the session directory is unreadable or no
/// daemon is running.
pub fn snapshot() -> Vec<SessionRow> {
    discover_sessions()
        .iter()
        .map(SessionRow::from_info)
        .collect()
}

/// Format Unix epoch seconds as `YYYY-MM-DDTHH:MM:SSZ` (civil-from-days).
fn format_unix_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Convert days since 1970-01-01 to a proleptic Gregorian `(year, month, day)`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn info(controller: Option<&str>, controller_stale: bool) -> DaemonSessionInfo {
        DaemonSessionInfo {
            session_id: "session-1".into(),
            socket_path: PathBuf::from("/run/user/1000/aish/s1.sock"),
            daemon_pid: 4242,
            child_pid: 4243,
            started_at: 1_700_000_000,
            cwd: "/home/user".into(),
            model: Some("gpt-5".into()),
            api_base: None,
            name: Some("dev".into()),
            controller: controller.map(str::to_string),
            controller_stale,
        }
    }

    #[test]
    fn role_mapping() {
        assert_eq!(
            SessionRow::from_info(&info(Some("client-a"), false)).role,
            SessionRole::Controller
        );
        assert_eq!(
            SessionRow::from_info(&info(None, false)).role,
            SessionRole::Observer
        );
        // Staleness wins over a recorded controller.
        assert_eq!(
            SessionRow::from_info(&info(Some("client-a"), true)).role,
            SessionRole::Stale
        );
    }

    #[test]
    fn row_fields_are_mapped() {
        let row = SessionRow::from_info(&info(Some("client-a"), false));
        assert_eq!(row.session_id, "session-1");
        assert_eq!(row.socket_path, "/run/user/1000/aish/s1.sock");
        assert_eq!(row.daemon_pid, 4242);
        assert_eq!(row.child_pid, Some(4243));
        assert_eq!(row.cwd, "/home/user");
        assert_eq!(row.model, "gpt-5");
        assert_eq!(row.name.as_deref(), Some("dev"));
        assert_eq!(row.started_at, "2023-11-14T22:13:20Z");

        let mut no_child = info(None, false);
        no_child.child_pid = 0;
        no_child.model = None;
        no_child.name = None;
        let row = SessionRow::from_info(&no_child);
        assert_eq!(row.child_pid, None);
        assert_eq!(row.model, "");
        assert_eq!(row.name, None);
    }

    #[test]
    fn snapshot_does_not_panic_with_no_daemon() {
        // Must degrade to an empty (or unrelated) list instead of panicking
        // when no session directory/daemon exists.
        let rows = snapshot();
        for row in rows {
            assert!(!row.session_id.is_empty());
        }
    }

    #[test]
    fn timestamp_formatting_handles_epoch_and_century_boundaries() {
        assert_eq!(format_unix_secs(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_unix_secs(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_unix_secs(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
