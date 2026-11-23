//! Derived state: the pure functions every front end agrees on.
//!
//! A ticket's stage is computed from its facts, never stored. That is what
//! makes the panel honest: there is exactly one definition of "waiting on a
//! person", and the store cannot hold a stale copy of it.

use crate::domain::{AssignmentState, ReportState, Ticket};

/// Where a ticket is, from the panel's point of view.
///
/// Read the variants top to bottom: the first one whose condition holds is the
/// stage, which is why this is an enum of *outcomes* rather than a stored
/// field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Over.
    Ended,
    /// Blocked on a person: it cannot continue without one.
    Person(PersonAction),
    /// Queued for a worker slot, or running.
    Working {
        /// Whether it is waiting for a free slot.
        queued: bool,
    },
    /// Between turns; a trigger on its thread can continue it.
    Idle,
}

/// What a person has to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersonAction {
    /// Answer a question.
    Answer,
    /// Review finished work.
    Review,
    /// Unblock it after a failure.
    Blocked,
    /// Retry after a failure.
    Failed,
}

impl PersonAction {
    /// The label the panel shows.
    pub fn label(self) -> &'static str {
        match self {
            PersonAction::Answer => "Awaiting answer",
            PersonAction::Review => "Awaiting review",
            PersonAction::Blocked => "Blocked",
            PersonAction::Failed => "Failed",
        }
    }
}

impl Stage {
    /// Whether this ticket needs a person right now.
    pub fn needs_person(self) -> bool {
        matches!(self, Stage::Person(_))
    }

    /// The panel label for this stage.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Ended => "Ended",
            Stage::Person(action) => action.label(),
            Stage::Working { queued: true } => "Queued",
            Stage::Working { queued: false } => "Working",
            Stage::Idle => "Waiting for a trigger",
        }
    }
}

/// Classify a ticket.
///
/// The order is the whole point: a `ready` report must read as "awaiting
/// review" rather than "working", and a blocked report must only reach a person
/// while nobody has spoken since.
pub fn ticket_stage(ticket: &Ticket) -> Stage {
    if ticket.ended {
        return Stage::Ended;
    }
    if ticket.attention.is_some() {
        return Stage::Person(PersonAction::Answer);
    }
    let assignment = ticket.latest_assignment();
    let report = assignment.and_then(|a| a.report.as_ref());
    match report.map(|r| r.state) {
        Some(ReportState::Ready) => return Stage::Person(PersonAction::Review),
        Some(ReportState::Blocked) if ticket.waits_for_person() => {
            return Stage::Person(PersonAction::Blocked)
        }
        Some(ReportState::Failed) if ticket.waits_for_person() => {
            return Stage::Person(PersonAction::Failed)
        }
        _ => {}
    }
    match assignment.map(|a| a.state) {
        None | Some(AssignmentState::Pending) => Stage::Working { queued: true },
        Some(AssignmentState::Ended) => Stage::Idle,
        Some(AssignmentState::Running) => {
            if report.map(|r| r.state) == Some(ReportState::Waiting) {
                Stage::Idle
            } else {
                Stage::Working { queued: false }
            }
        }
    }
}

/// Whether a retry is available.
///
/// Only a ticket that is stuck on a person can be retried: retrying a working
/// ticket would launch a second worker into the same directory.
pub fn can_retry(ticket: &Ticket) -> bool {
    !ticket.ended
        && matches!(
            ticket_stage(ticket),
            Stage::Person(PersonAction::Blocked | PersonAction::Failed)
        )
}

/// Whether any assignment is live.
pub fn is_live(ticket: &Ticket) -> bool {
    !ticket.ended
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Actor, Assignment, Basis, Mandate, Report, Strength, ThreadKey, TicketInput, TriggerKind,
    };

    fn mandate() -> Mandate {
        Mandate {
            strength: Strength::Explicit,
            basis: Basis::Direct,
            scope: "one file".to_string(),
        }
    }

    /// A ticket shaped like one the supervisor creates: `inputs[0]` is the ask
    /// that created it, which is a task rather than a follow-up.
    fn ticket() -> Ticket {
        Ticket {
            id: "t1".to_string(),
            title: "fix the thing".to_string(),
            mandate: mandate(),
            thread: ThreadKey("th".to_string()),
            ended: false,
            attention: None,
            assignments: Vec::new(),
            inputs: vec![TicketInput {
                trigger_id: "creating-ask".to_string(),
                text: "fix the thing".to_string(),
                read: true,
            }],
        }
    }

    fn with_report(state: ReportState) -> Ticket {
        let mut t = ticket();
        t.assignments.push(Assignment {
            id: "a1".to_string(),
            state: AssignmentState::Running,
            report: Some(Report {
                state,
                summary: "s".to_string(),
                question: None,
            }),
        });
        t
    }

    #[test]
    fn an_ended_ticket_wins_over_everything() {
        let mut t = with_report(ReportState::Ready);
        t.ended = true;
        assert_eq!(ticket_stage(&t), Stage::Ended);
    }

    #[test]
    fn an_attention_reads_as_awaiting_answer() {
        let mut t = with_report(ReportState::Progress);
        t.attention = Some("which file?".to_string());
        assert_eq!(ticket_stage(&t), Stage::Person(PersonAction::Answer));
    }

    #[test]
    fn a_ready_report_reads_as_awaiting_review_not_working() {
        let t = with_report(ReportState::Ready);
        assert_eq!(ticket_stage(&t), Stage::Person(PersonAction::Review));
    }

    #[test]
    fn a_blocked_report_reaches_a_person_only_while_nobody_spoke() {
        let t = with_report(ReportState::Blocked);
        assert_eq!(ticket_stage(&t), Stage::Person(PersonAction::Blocked));

        // An unread person input is a newer follow-up: the ticket goes back to
        // the worker rather than staying stuck on a person.
        let mut resumed = t.clone();
        resumed.inputs.push(TicketInput {
            trigger_id: "tr1".to_string(),
            text: "here is more".to_string(),
            read: false,
        });
        assert_ne!(
            ticket_stage(&resumed),
            Stage::Person(PersonAction::Blocked),
            "a follow-up must release the ticket"
        );
    }

    #[test]
    fn a_failed_report_reads_as_failed() {
        let t = with_report(ReportState::Failed);
        assert_eq!(ticket_stage(&t), Stage::Person(PersonAction::Failed));
    }

    #[test]
    fn a_ticket_without_assignments_is_queued() {
        let t = ticket();
        assert_eq!(ticket_stage(&t), Stage::Working { queued: true });
    }

    #[test]
    fn a_pending_assignment_is_queued() {
        let mut t = ticket();
        t.assignments.push(Assignment {
            id: "a1".to_string(),
            state: AssignmentState::Pending,
            report: None,
        });
        assert_eq!(ticket_stage(&t), Stage::Working { queued: true });
    }

    #[test]
    fn a_waiting_report_is_idle_not_working() {
        // The worker ended its turn without an outstanding question, so a
        // trigger on the thread can continue it.
        let t = with_report(ReportState::Waiting);
        assert_eq!(ticket_stage(&t), Stage::Idle);
    }

    #[test]
    fn an_ended_assignment_is_idle() {
        let mut t = ticket();
        t.assignments.push(Assignment {
            id: "a1".to_string(),
            state: AssignmentState::Ended,
            report: None,
        });
        assert_eq!(ticket_stage(&t), Stage::Idle);
    }

    #[test]
    fn a_running_assignment_with_progress_is_working() {
        let t = with_report(ReportState::Progress);
        assert_eq!(ticket_stage(&t), Stage::Working { queued: false });
    }

    #[test]
    fn retry_is_offered_only_where_it_can_help() {
        assert!(!can_retry(&ticket()), "queued work is not retryable");
        assert!(!can_retry(&with_report(ReportState::Progress)));
        assert!(!can_retry(&with_report(ReportState::Ready)));
        assert!(can_retry(&with_report(ReportState::Blocked)));
        assert!(can_retry(&with_report(ReportState::Failed)));

        let mut ended = with_report(ReportState::Failed);
        ended.ended = true;
        assert!(!can_retry(&ended), "an ended ticket cannot be retried");
    }

    #[test]
    fn labels_say_who_a_row_waits_for() {
        assert_eq!(
            ticket_stage(&with_report(ReportState::Ready)).label(),
            "Awaiting review"
        );
        assert_eq!(ticket_stage(&ticket()).label(), "Queued");
        for action in [
            PersonAction::Answer,
            PersonAction::Review,
            PersonAction::Blocked,
            PersonAction::Failed,
        ] {
            assert!(!action.label().is_empty());
        }
    }

    #[test]
    fn a_dropped_report_leaves_the_ticket_idle() {
        // Nothing to do is not a failure: the ticket waits for a trigger.
        let t = with_report(ReportState::Dropped);
        assert_eq!(ticket_stage(&t), Stage::Working { queued: false });
        assert!(!can_retry(&t));
    }

    #[test]
    fn trigger_kinds_and_actors_are_classified() {
        let ask = crate::domain::Trigger {
            id: "x".to_string(),
            kind: TriggerKind::UserAsk,
            actor: Actor::User,
            text: "do it".to_string(),
            thread: ThreadKey("t".to_string()),
            at: 0,
        };
        assert!(ask.is_human());
        assert!(!Actor::Task.is_human());
    }
}
