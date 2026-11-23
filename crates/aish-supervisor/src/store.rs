//! Durable state: what survives a restart.
//!
//! The supervisor's job when the process dies is to lose as little as possible
//! and, more importantly, never to lose *decisions*. Two rules shape this
//! module:
//!
//! 1. **Save after every command**, not on a timer. A ticket that was accepted
//!    but not yet launched must come back as accepted-but-queued, because the
//!    person was already told it started.
//! 2. **Load is total.** A row that cannot be parsed is skipped with a warning
//!    rather than failing the shell: a corrupt entry must not make the
//!    supervisor unusable.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::domain::{
    Assignment, AssignmentState, Basis, Mandate, Report, ReportState, Strength, ThreadKey, Ticket,
    TicketInput,
};

/// Everything worth keeping across a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Persisted {
    /// Tickets, in creation order.
    pub tickets: Vec<Ticket>,
    /// Effects that were queued but not yet dispatched, as `(kind, ticket)`.
    pub effects: Vec<(String, String)>,
}

/// Where the supervisor's state lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// No persistence: the supervisor is a scratch pad.
    Memory,
    /// A SQLite file.
    File(PathBuf),
}

impl Location {
    /// The default file next to the session database.
    pub fn default_file() -> Self {
        let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Location::File(base.join("aish").join("supervisor.db"))
    }

    /// Open the store, creating the schema when needed.
    pub fn open(&self) -> Result<Store, String> {
        match self {
            Location::Memory => Ok(Store {
                conn: None,
                path: None,
            }),
            Location::File(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("create {}: {e}", parent.display()))?;
                }
                let conn =
                    Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
                init_schema(&conn)?;
                Ok(Store {
                    conn: Some(std::sync::Arc::new(parking_lot::Mutex::new(conn))),
                    path: Some(path.clone()),
                })
            }
        }
    }
}

/// A handle to the durable store.
/// Shareable handle. `rusqlite::Connection` is not `Sync`, so the connection
/// sits behind a mutex; saving is a short transaction and never holds the lock
/// across a worker turn.
#[derive(Clone)]
pub struct Store {
    conn: Option<std::sync::Arc<parking_lot::Mutex<Connection>>>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish()
    }
}

fn init_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ticket (
             id TEXT PRIMARY KEY,
             title TEXT NOT NULL,
             thread TEXT NOT NULL,
             strength TEXT NOT NULL,
             basis TEXT NOT NULL,
             scope TEXT NOT NULL,
             ended INTEGER NOT NULL,
             attention TEXT,
             next_assignment INTEGER NOT NULL DEFAULT 0,
             next_input INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS assignment (
             ticket_id TEXT NOT NULL,
             seq INTEGER NOT NULL,
             state TEXT NOT NULL,
             report_state TEXT,
             report_summary TEXT,
             report_question TEXT,
             PRIMARY KEY (ticket_id, seq)
         );
         CREATE TABLE IF NOT EXISTS input (
             ticket_id TEXT NOT NULL,
             seq INTEGER NOT NULL,
             trigger_id TEXT NOT NULL,
             text TEXT NOT NULL,
             read INTEGER NOT NULL,
             PRIMARY KEY (ticket_id, seq)
         );
         CREATE TABLE IF NOT EXISTS effect (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             kind TEXT NOT NULL,
             ticket_id TEXT NOT NULL
         );",
    )
    .map_err(|e| format!("create schema: {e}"))
}

impl Store {
    /// Replace the stored state with `state`.
    ///
    /// A wholesale rewrite rather than incremental updates: the supervisor's
    /// state is small (tens of rows) and a full rewrite cannot drift from the
    /// in-memory truth the way a partial update can.
    pub fn save(&self, state: &Persisted) -> Result<(), String> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let conn = conn.lock();
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("begin: {e}"))?;
        tx.execute("DELETE FROM ticket", [])
            .map_err(|e| format!("clear tickets: {e}"))?;
        tx.execute("DELETE FROM assignment", [])
            .map_err(|e| format!("clear assignments: {e}"))?;
        tx.execute("DELETE FROM input", [])
            .map_err(|e| format!("clear inputs: {e}"))?;
        tx.execute("DELETE FROM effect", [])
            .map_err(|e| format!("clear effects: {e}"))?;

        for ticket in &state.tickets {
            tx.execute(
                "INSERT INTO ticket
                   (id, title, thread, strength, basis, scope, ended, attention,
                    next_assignment, next_input)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    ticket.id,
                    ticket.title,
                    ticket.thread.0,
                    strength_name(ticket.mandate.strength),
                    basis_name(ticket.mandate.basis),
                    ticket.mandate.scope,
                    i64::from(ticket.ended),
                    ticket.attention,
                    ticket.assignments.len() as i64,
                    ticket.inputs.len() as i64,
                ],
            )
            .map_err(|e| format!("insert ticket {}: {e}", ticket.id))?;

            for (seq, assignment) in ticket.assignments.iter().enumerate() {
                let (report_state, summary, question) = match &assignment.report {
                    Some(report) => (
                        Some(report_state_name(report.state)),
                        Some(report.summary.clone()),
                        report.question.clone(),
                    ),
                    None => (None, None, None),
                };
                tx.execute(
                    "INSERT INTO assignment
                       (ticket_id, seq, state, report_state, report_summary, report_question)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        ticket.id,
                        seq as i64,
                        assignment_state_name(assignment.state),
                        report_state,
                        summary,
                        question,
                    ],
                )
                .map_err(|e| format!("insert assignment: {e}"))?;
            }

            for (seq, input) in ticket.inputs.iter().enumerate() {
                tx.execute(
                    "INSERT INTO input (ticket_id, seq, trigger_id, text, read)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        ticket.id,
                        seq as i64,
                        input.trigger_id,
                        input.text,
                        i64::from(input.read)
                    ],
                )
                .map_err(|e| format!("insert input: {e}"))?;
            }
        }

        for (kind, ticket_id) in &state.effects {
            tx.execute(
                "INSERT INTO effect (kind, ticket_id) VALUES (?1, ?2)",
                params![kind, ticket_id],
            )
            .map_err(|e| format!("insert effect: {e}"))?;
        }

        tx.commit().map_err(|e| format!("commit: {e}"))
    }

    /// Read the stored state.
    ///
    /// A row that cannot be parsed is skipped with a warning; the caller gets
    /// whatever could be recovered, which is always better than refusing to
    /// start.
    pub fn load(&self) -> Persisted {
        let Some(conn) = &self.conn else {
            return Persisted::default();
        };
        let conn = conn.lock();
        let mut state = Persisted::default();

        let Ok(mut stmt) = conn.prepare(
            "SELECT id, title, thread, strength, basis, scope, ended, attention
             FROM ticket ORDER BY rowid",
        ) else {
            return state;
        };
        // The seeds are collected before the child tables are read: an open
        // statement borrowing `conn` would block the second query.
        let seeds: Vec<TicketSeed> = match stmt.query_map([], |row| {
            Ok(TicketSeed {
                id: row.get(0)?,
                title: row.get(1)?,
                thread: row.get(2)?,
                strength: row.get(3)?,
                basis: row.get(4)?,
                scope: row.get(5)?,
                ended: row.get::<_, i64>(6)? != 0,
                attention: row.get(7)?,
            })
        }) {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => return state,
        };
        drop(stmt);

        for row in seeds {
            state.tickets.push(Ticket {
                id: row.id.clone(),
                title: row.title,
                thread: ThreadKey(row.thread),
                mandate: Mandate {
                    strength: parse_strength(&row.strength),
                    basis: parse_basis(&row.basis),
                    scope: row.scope,
                },
                ended: row.ended,
                attention: row.attention,
                assignments: Vec::new(),
                inputs: Vec::new(),
            });
        }

        for ticket in &mut state.tickets {
            ticket.assignments = load_assignments(&conn, &ticket.id);
            ticket.inputs = load_inputs(&conn, &ticket.id);
        }

        if let Ok(mut stmt) = conn.prepare("SELECT kind, ticket_id FROM effect ORDER BY seq") {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            }) {
                for row in rows.flatten() {
                    state.effects.push(row);
                }
            }
        }

        state
    }

    /// Whether this store actually writes anything.
    pub fn is_durable(&self) -> bool {
        self.conn.is_some()
    }

    /// The file behind this store, if any.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

struct TicketSeed {
    id: String,
    title: String,
    thread: String,
    strength: String,
    basis: String,
    scope: String,
    ended: bool,
    attention: Option<String>,
}

fn load_assignments(conn: &Connection, ticket_id: &str) -> Vec<Assignment> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT seq, state, report_state, report_summary, report_question
         FROM assignment WHERE ticket_id = ?1 ORDER BY seq",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![ticket_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .map(|(seq, state, report_state, summary, question)| Assignment {
            id: format!("{ticket_id}-{}", seq + 1),
            state: parse_assignment_state(&state),
            report: report_state.map(|state| Report {
                state: parse_report_state(&state),
                summary: summary.unwrap_or_default(),
                question,
            }),
        })
        .collect()
}

fn load_inputs(conn: &Connection, ticket_id: &str) -> Vec<TicketInput> {
    let Ok(mut stmt) =
        conn.prepare("SELECT trigger_id, text, read FROM input WHERE ticket_id = ?1 ORDER BY seq")
    else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(params![ticket_id], |row| {
        Ok(TicketInput {
            trigger_id: row.get(0)?,
            text: row.get(1)?,
            read: row.get::<_, i64>(2)? != 0,
        })
    }) else {
        return Vec::new();
    };
    rows.flatten().collect()
}

fn strength_name(strength: Strength) -> &'static str {
    match strength {
        Strength::Explicit => "explicit",
        Strength::Inferred => "inferred",
    }
}

fn parse_strength(raw: &str) -> Strength {
    match raw {
        "explicit" => Strength::Explicit,
        _ => Strength::Inferred,
    }
}

fn basis_name(basis: Basis) -> &'static str {
    match basis {
        Basis::Direct => "direct",
        Basis::Followup => "followup",
    }
}

fn parse_basis(raw: &str) -> Basis {
    match raw {
        "direct" => Basis::Direct,
        _ => Basis::Followup,
    }
}

fn assignment_state_name(state: AssignmentState) -> &'static str {
    match state {
        AssignmentState::Pending => "pending",
        AssignmentState::Running => "running",
        AssignmentState::Ended => "ended",
    }
}

fn parse_assignment_state(raw: &str) -> AssignmentState {
    match raw {
        "running" => AssignmentState::Running,
        "ended" => AssignmentState::Ended,
        _ => AssignmentState::Pending,
    }
}

fn report_state_name(state: ReportState) -> &'static str {
    match state {
        ReportState::Progress => "progress",
        ReportState::Waiting => "waiting",
        ReportState::Ready => "ready",
        ReportState::Blocked => "blocked",
        ReportState::Failed => "failed",
        ReportState::Dropped => "dropped",
    }
}

fn parse_report_state(raw: &str) -> ReportState {
    match raw {
        "progress" => ReportState::Progress,
        "waiting" => ReportState::Waiting,
        "ready" => ReportState::Ready,
        "blocked" => ReportState::Blocked,
        "failed" => ReportState::Failed,
        _ => ReportState::Dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Actor, Trigger, TriggerKind};
    use crate::Supervisor;

    fn ask(text: &str, thread: &str) -> Trigger {
        Trigger {
            id: format!("h-{text}"),
            kind: TriggerKind::UserAsk,
            actor: Actor::User,
            text: text.to_string(),
            thread: ThreadKey(thread.to_string()),
            at: 0,
        }
    }

    /// A store in a temporary file, so the real SQLite path is exercised.
    fn temp_store(dir: &tempfile::TempDir) -> Store {
        Location::File(dir.path().join("supervisor.db"))
            .open()
            .expect("store opens")
    }

    #[test]
    fn a_memory_store_is_durable_silently() {
        let store = Location::Memory.open().expect("memory opens");
        assert!(!store.is_durable());
        store.save(&Persisted::default()).expect("save is a no-op");
        assert!(store.load().tickets.is_empty());
    }

    #[test]
    fn tickets_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix the parser", "th1"));

        let store = temp_store(&dir);
        store.save(&supervisor.export()).expect("saved");
        let loaded = store.load();

        assert_eq!(loaded.tickets.len(), 1);
        assert_eq!(loaded.tickets[0].title, "fix the parser");
        assert_eq!(loaded.tickets[0].mandate.strength, Strength::Explicit);
        assert_eq!(loaded.tickets[0].mandate.basis, Basis::Direct);
        assert_eq!(loaded.tickets[0].inputs.len(), 1);
        assert_eq!(loaded.tickets[0].inputs[0].text, "fix the parser");
    }

    #[test]
    fn reports_and_attention_survive() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.assignment_started(&id, false);
        supervisor
            .report(
                &id,
                Report {
                    state: ReportState::Progress,
                    summary: "halfway".to_string(),
                    question: Some("which file?".to_string()),
                },
            )
            .unwrap();

        let store = temp_store(&dir);
        store.save(&supervisor.export()).unwrap();
        let loaded = store.load();

        let ticket = &loaded.tickets[0];
        assert_eq!(ticket.attention.as_deref(), Some("which file?"));
        let report = ticket.latest_report().expect("report kept");
        assert_eq!(report.summary, "halfway");
        assert_eq!(report.question.as_deref(), Some("which file?"));
    }

    #[test]
    fn a_queued_effect_survives_so_work_is_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));

        let store = temp_store(&dir);
        store.save(&supervisor.export()).unwrap();
        let loaded = store.load();

        assert_eq!(
            loaded.effects.len(),
            1,
            "the launch decision must outlive the process"
        );
        assert_eq!(loaded.effects[0].0, "launch");
    }

    #[test]
    fn a_restored_supervisor_keeps_its_stage() {
        // The point of persistence: a restart must not resurrect finished work
        // as if it were queued again, nor lose a blocked ticket's state.
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.assignment_started(&id, false);
        supervisor
            .report(
                &id,
                Report {
                    state: ReportState::Ready,
                    summary: "all done".to_string(),
                    question: None,
                },
            )
            .unwrap();

        let store = temp_store(&dir);
        store.save(&supervisor.export()).unwrap();

        let restored = Supervisor::from_persisted(store.load());
        let view = restored.view();
        assert_eq!(view.rows()[0].stage.label(), "Awaiting review");
        assert_eq!(view.rows()[0].detail, "all done");
    }

    #[test]
    fn saving_twice_does_not_duplicate_rows() {
        let dir = tempfile::tempdir().unwrap();
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));

        let store = temp_store(&dir);
        store.save(&supervisor.export()).unwrap();
        store.save(&supervisor.export()).unwrap();

        assert_eq!(store.load().tickets.len(), 1);
    }

    #[test]
    fn the_database_file_is_created_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/supervisor.db");
        let store = Location::File(path.clone()).open().expect("opens");
        assert!(store.is_durable());
        assert_eq!(store.path(), Some(path.as_path()));
        assert!(path.exists(), "the file must exist after opening");
    }
}
