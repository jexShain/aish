//! The supervisor: one place where tickets are created, advanced and ended.
//!
//! Every mutation goes through a command on this type, so there is exactly one
//! writer for each piece of state. Commands are synchronous and side-effect
//! free beyond their own fields: what the outside world should do becomes an
//! [`Effect`] on the queue, executed afterwards.
//!
//! The front end calls [`Supervisor::view`] and gets an immutable projection;
//! it never inspects the store.

pub mod domain;
pub mod engine;
pub mod runtime;
pub mod stage;

use parking_lot::Mutex;

use crate::domain::{
    Assignment, AssignmentState, Basis, Decision, Mandate, Report, ReportState, Strength, Ticket,
    TicketInput, Trigger,
};
use crate::engine::{decide, Effect, EffectQueue, Outcome};
use crate::stage::{can_retry, ticket_stage, PersonAction, Stage};

/// A ticket as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketRow {
    /// Stable id.
    pub id: String,
    /// One line.
    pub title: String,
    /// Derived stage.
    pub stage: Stage,
    /// Whether retry applies.
    pub retryable: bool,
    /// The latest report's summary, if any.
    pub detail: String,
    /// The question blocking it, if any.
    pub question: Option<String>,
    /// The thread its messages belong to, so the panel can address it.
    pub thread: String,
}

/// The whole panel state, as data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct View {
    /// Tickets, needing-a-person first, then by insertion order.
    rows: Vec<TicketRow>,
    /// Effects waiting to run; non-zero means work is in flight.
    pending_effects: usize,
}

impl View {
    /// The rows, in display order.
    pub fn rows(&self) -> &[TicketRow] {
        &self.rows
    }

    /// Rows that need a person.
    pub fn needing_person(&self) -> impl Iterator<Item = &TicketRow> {
        self.rows.iter().filter(|row| row.stage.needs_person())
    }

    /// How many tickets need a person.
    pub fn waiting_count(&self) -> usize {
        self.needing_person().count()
    }

    /// How many tickets are still open.
    pub fn open_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.stage != Stage::Ended)
            .count()
    }

    /// How many effects are waiting to run.
    pub fn pending_effects(&self) -> usize {
        self.pending_effects
    }

    /// Whether anything is in flight, i.e. whether the UI should animate.
    pub fn is_busy(&self) -> bool {
        self.pending_effects > 0
            || self
                .rows
                .iter()
                .any(|row| matches!(row.stage, Stage::Working { queued: false }))
    }

    /// One-line status for a status bar.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.waiting_count() > 0 {
            parts.push(format!("{} needs you", self.waiting_count()));
        }
        if self.pending_effects > 0 {
            parts.push(format!("{} queued", self.pending_effects));
        }
        parts.join(" · ")
    }
}

/// Why an operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// No ticket with that id.
    UnknownTicket(String),
    /// The ticket is already over.
    AlreadyEnded(String),
    /// The action does not apply in this state.
    NotApplicable(&'static str),
}

/// Maximum concurrent workers; one extra slot is reserved for the person.
pub use runtime::{Executor, PassOutcome, Runtime};

pub const MAX_WORKERS: usize = 3;

#[derive(Debug)]
struct State {
    tickets: Vec<Ticket>,
    queue: EffectQueue,
    /// Tickets currently holding a worker slot.
    running: Vec<String>,
}

/// The supervisor.
#[derive(Debug)]
pub struct Supervisor {
    state: Mutex<State>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// An empty supervisor.
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                tickets: Vec::new(),
                queue: EffectQueue::new(),
                running: Vec::new(),
            }),
        }
    }

    /// Take in a trigger: decide, and create a ticket when accepted.
    ///
    /// Returns the accepted decision, or `None` when the trigger was ignored.
    /// Accepting the same trigger twice is prevented by the caller's
    /// content-addressed id; this method is otherwise idempotent per id.
    pub fn receive(&self, trigger: &Trigger) -> Option<Decision> {
        let mut state = self.state.lock();
        let has_open = state
            .tickets
            .iter()
            .any(|ticket| ticket.thread == trigger.thread && !ticket.ended);

        let decision = decide(trigger, has_open);
        match &decision {
            Decision::Accept(mandate) => {
                // A follow-up on an open ticket is delivered to it instead of
                // creating a second one: the ticket already owns the thread.
                if let Some(index) = state
                    .tickets
                    .iter()
                    .position(|t| t.thread == trigger.thread && !t.ended)
                {
                    let already_seen = state.tickets[index]
                        .inputs
                        .iter()
                        .any(|i| i.trigger_id == trigger.id);
                    if !already_seen {
                        let id = state.tickets[index].id.clone();
                        state.tickets[index].inputs.push(TicketInput {
                            trigger_id: trigger.id.clone(),
                            text: trigger.text.clone(),
                            read: false,
                        });
                        state.queue.enqueue(Effect::Deliver { ticket_id: id });
                    }
                } else {
                    let mandate = mandate.clone();
                    let ticket = Ticket {
                        id: ticket_id(trigger),
                        title: title_of(trigger),
                        mandate,
                        thread: trigger.thread.clone(),
                        ended: false,
                        attention: None,
                        assignments: Vec::new(),
                        // The ask that created the ticket is its first input:
                        // the worker needs the request itself, not just a
                        // title, and `take_inputs` is the only way it gets it.
                        inputs: vec![TicketInput {
                            trigger_id: trigger.id.clone(),
                            text: trigger.text.clone(),
                            read: false,
                        }],
                    };
                    let id = ticket.id.clone();
                    state.tickets.push(ticket);
                    state.queue.enqueue(Effect::Launch { ticket_id: id });
                }
            }
            Decision::Stop(_) => {
                if let Some(index) = state
                    .tickets
                    .iter()
                    .position(|t| t.thread == trigger.thread && !t.ended)
                {
                    let ticket = &mut state.tickets[index];
                    ticket.ended = true;
                    let id = ticket.id.clone();
                    state.queue.enqueue(Effect::Cancel { ticket_id: id });
                }
            }
            Decision::Ignore(_) => {}
        }
        Some(decision)
    }

    /// Take in a person's message addressed at an existing ticket.
    ///
    /// This is the panel's "talk to this worker" path: the message reaches the
    /// ticket that already owns the thread instead of starting new work, which
    /// is what keeps a conversation in one place.
    pub fn say(&self, ticket_id: &str, text: &str) -> Result<(), Refusal> {
        let mut state = self.state.lock();
        let index = state
            .tickets
            .iter()
            .position(|t| t.id == ticket_id)
            .ok_or_else(|| Refusal::UnknownTicket(ticket_id.to_string()))?;
        if state.tickets[index].ended {
            return Err(Refusal::AlreadyEnded(ticket_id.to_string()));
        }
        let trigger_id = format!("say-{ticket_id}-{}", state.tickets[index].inputs.len() + 1);
        state.tickets[index].inputs.push(TicketInput {
            trigger_id,
            text: text.to_string(),
            read: false,
        });
        // A person's message releases a blocked or failed ticket, as it does
        // in the execution document: they are the one who can unblock it.
        state.tickets[index].attention = None;
        state.queue.enqueue(Effect::Deliver {
            ticket_id: ticket_id.to_string(),
        });
        Ok(())
    }

    /// Record that a worker took the slot and started.
    ///
    /// Returns `false` when no slot was free; the caller then leaves the
    /// launch queued (`Outcome::Later`) rather than treating it as a failure.
    pub fn assignment_started(&self, ticket_id: &str, by_person: bool) -> bool {
        let mut state = self.state.lock();
        let limit = MAX_WORKERS + usize::from(by_person);
        if state.running.len() >= limit {
            return false;
        }
        let Some(ticket) = state.tickets.iter_mut().find(|t| t.id == ticket_id) else {
            return false;
        };
        ticket.assignments.push(Assignment {
            id: format!("{ticket_id}-{}", ticket.assignments.len() + 1),
            state: AssignmentState::Running,
            report: None,
        });
        state.running.push(ticket_id.to_string());
        true
    }

    /// Record a worker's structured report.
    ///
    /// `question` is the only way a ticket reaches "awaiting answer"; the
    /// summary is never parsed for intent.
    pub fn report(&self, ticket_id: &str, report: Report) -> Result<(), Refusal> {
        let mut state = self.state.lock();
        let ticket = state
            .tickets
            .iter_mut()
            .find(|t| t.id == ticket_id)
            .ok_or_else(|| Refusal::UnknownTicket(ticket_id.to_string()))?;
        if ticket.ended {
            return Err(Refusal::AlreadyEnded(ticket_id.to_string()));
        }
        let question = report.question.clone();
        let ends_turn = matches!(
            report.state,
            ReportState::Waiting | ReportState::Ready | ReportState::Dropped
        );
        if let Some(assignment) = ticket.assignments.last_mut() {
            assignment.report = Some(report);
            if ends_turn {
                assignment.state = AssignmentState::Ended;
            }
        } else {
            ticket.assignments.push(Assignment {
                id: format!("{ticket_id}-1"),
                state: if ends_turn {
                    AssignmentState::Ended
                } else {
                    AssignmentState::Running
                },
                report: Some(report),
            });
        }
        // An answered question clears the attention; a new one sets it.
        ticket.attention = question;
        // A turn that ended releases its worker slot for the next ticket.
        if ends_turn {
            state.running.retain(|id| id != ticket_id);
        }
        Ok(())
    }

    /// Answer an open question and hand the ticket back to a worker.
    pub fn answer(&self, ticket_id: &str, answer: &str) -> Result<(), Refusal> {
        let mut state = self.state.lock();
        let ticket = state
            .tickets
            .iter_mut()
            .find(|t| t.id == ticket_id)
            .ok_or_else(|| Refusal::UnknownTicket(ticket_id.to_string()))?;
        if ticket.ended {
            return Err(Refusal::AlreadyEnded(ticket_id.to_string()));
        }
        if ticket.attention.take().is_none() {
            return Err(Refusal::NotApplicable("no open question"));
        }
        ticket.inputs.push(TicketInput {
            trigger_id: format!("answer-{}", ticket.inputs.len() + 1),
            text: answer.to_string(),
            read: false,
        });
        ticket.assignments.push(Assignment {
            id: format!("{ticket_id}-{}", ticket.assignments.len() + 1),
            state: AssignmentState::Pending,
            report: None,
        });
        let id = ticket.id.clone();
        state.queue.enqueue(Effect::Deliver { ticket_id: id });
        Ok(())
    }

    /// Retry a ticket that is stuck on a person.
    pub fn retry(&self, ticket_id: &str) -> Result<(), Refusal> {
        let mut state = self.state.lock();
        let ticket = state
            .tickets
            .iter_mut()
            .find(|t| t.id == ticket_id)
            .ok_or_else(|| Refusal::UnknownTicket(ticket_id.to_string()))?;
        if !can_retry(ticket) {
            return Err(Refusal::NotApplicable("retry does not apply here"));
        }
        ticket.inputs.push(TicketInput {
            trigger_id: format!("retry-{}", ticket.inputs.len() + 1),
            text: "retry".to_string(),
            read: false,
        });
        ticket.attention = None;
        ticket.assignments.push(Assignment {
            id: format!("{ticket_id}-{}", ticket.assignments.len() + 1),
            state: AssignmentState::Pending,
            report: None,
        });
        let id = ticket.id.clone();
        state.queue.enqueue(Effect::Deliver { ticket_id: id });
        Ok(())
    }

    /// Stop a ticket for good. Only a person may do this.
    pub fn stop(&self, ticket_id: &str) -> Result<(), Refusal> {
        let mut state = self.state.lock();
        let ticket = state
            .tickets
            .iter_mut()
            .find(|t| t.id == ticket_id)
            .ok_or_else(|| Refusal::UnknownTicket(ticket_id.to_string()))?;
        if ticket.ended {
            return Err(Refusal::AlreadyEnded(ticket_id.to_string()));
        }
        ticket.ended = true;
        ticket.attention = None;
        let id = ticket.id.clone();
        state.running.retain(|running| running != &id);
        state.queue.enqueue(Effect::Cancel { ticket_id: id });
        Ok(())
    }

    /// Take the next effect to run, if any.
    pub fn next_effect(&self) -> Option<crate::engine::QueuedEffect> {
        self.state.lock().queue.take_next()
    }

    /// Settle an effect after an attempt.
    pub fn settle_effect(&self, effect: crate::engine::QueuedEffect, outcome: Outcome) {
        self.state.lock().queue.settle(effect, outcome);
    }

    /// The panel projection.
    pub fn view(&self) -> View {
        let state = self.state.lock();
        let mut rows: Vec<TicketRow> = state
            .tickets
            .iter()
            .map(|ticket| TicketRow {
                id: ticket.id.clone(),
                title: ticket.title.clone(),
                stage: ticket_stage(ticket),
                retryable: can_retry(ticket),
                detail: ticket
                    .latest_report()
                    .map(|r| r.summary.clone())
                    .unwrap_or_default(),
                question: ticket.attention.clone(),
                thread: ticket.thread.0.clone(),
            })
            .collect();
        // Needs-a-person first, then open work, then ended: the panel answers
        // "does anything need me" before anything else.
        rows.sort_by_key(|row| match row.stage {
            Stage::Person(_) => 0,
            Stage::Working { .. } => 1,
            Stage::Idle => 2,
            Stage::Ended => 3,
        });
        View {
            rows,
            pending_effects: state.queue.len(),
        }
    }

    /// Unread inputs for a ticket, in order.
    ///
    /// The runtime turns these into the worker's prompt; the supervisor itself
    /// never renders text.
    pub fn take_inputs(&self, ticket_id: &str) -> Vec<String> {
        let mut state = self.state.lock();
        let Some(ticket) = state.tickets.iter_mut().find(|t| t.id == ticket_id) else {
            return Vec::new();
        };
        let texts: Vec<String> = ticket
            .inputs
            .iter()
            .filter(|input| !input.read)
            .map(|input| input.text.clone())
            .collect();
        for input in &mut ticket.inputs {
            if !input.read {
                input.read = true;
            }
        }
        texts
    }

    /// Number of live worker slots.
    pub fn running_count(&self) -> usize {
        self.state.lock().running.len()
    }

    /// The person-action a row offers, if it needs one.
    pub fn person_action(&self, ticket_id: &str) -> Option<PersonAction> {
        let state = self.state.lock();
        let ticket = state.tickets.iter().find(|t| t.id == ticket_id)?;
        match ticket_stage(ticket) {
            Stage::Person(action) => Some(action),
            _ => None,
        }
    }
}

/// A stable ticket id derived from the accepting trigger.
fn ticket_id(trigger: &Trigger) -> String {
    format!("tk-{}", trigger.id)
}

/// One line naming the work.
fn title_of(trigger: &Trigger) -> String {
    let text = trigger.text.trim();
    let first = text.lines().next().unwrap_or("").trim();
    let mut title: String = first.chars().take(72).collect();
    if first.chars().count() > 72 {
        title.push('…');
    }
    if title.is_empty() {
        "untitled work".to_string()
    } else {
        title
    }
}

/// Build a mandate for tests and callers that need one directly.
pub fn explicit_mandate(scope: &str) -> Mandate {
    Mandate {
        strength: Strength::Explicit,
        basis: Basis::Direct,
        scope: scope.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Actor, ThreadKey, TriggerKind};
    use crate::engine::Outcome;

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

    fn report(state: ReportState, question: Option<&str>) -> Report {
        Report {
            state,
            summary: "did things".to_string(),
            question: question.map(str::to_string),
        }
    }

    #[test]
    fn a_user_ask_creates_a_ticket_and_queues_a_launch() {
        let supervisor = Supervisor::new();
        let decision = supervisor.receive(&ask("fix the parser", "th1"));
        assert!(matches!(decision, Some(Decision::Accept(_))));

        let view = supervisor.view();
        assert_eq!(view.rows().len(), 1);
        assert_eq!(view.rows()[0].title, "fix the parser");
        assert_eq!(view.rows()[0].stage, Stage::Working { queued: true });

        let effect = supervisor.next_effect().expect("launch queued");
        assert_eq!(effect.effect.kind(), "launch");
    }

    #[test]
    fn a_second_ask_on_the_same_thread_joins_the_open_ticket() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("first", "th1"));
        supervisor.receive(&ask("second", "th1"));
        assert_eq!(
            supervisor.view().rows().len(),
            1,
            "work on one thread belongs to one ticket"
        );
    }

    #[test]
    fn a_background_task_without_a_ticket_starts_nothing() {
        let supervisor = Supervisor::new();
        let mut trigger = ask("done", "th9");
        trigger.kind = TriggerKind::TaskDone;
        trigger.actor = Actor::Task;
        let decision = supervisor.receive(&trigger);
        assert!(matches!(decision, Some(Decision::Ignore(_))));
        assert!(supervisor.view().rows().is_empty());
    }

    #[test]
    fn slots_are_bounded_and_a_person_gets_an_extra_one() {
        let supervisor = Supervisor::new();
        for i in 0..MAX_WORKERS {
            supervisor.receive(&ask(&format!("t{i}"), &format!("th{i}")));
            assert!(
                supervisor.assignment_started(&format!("tk-h-t{i}"), false),
                "slot {i} should be free"
            );
        }
        supervisor.receive(&ask("overflow", "thx"));
        assert!(
            !supervisor.assignment_started("tk-h-overflow", false),
            "the normal slots are full"
        );
        assert!(
            supervisor.assignment_started("tk-h-overflow", true),
            "a person must never be blocked by queued work"
        );
    }

    #[test]
    fn a_question_moves_the_ticket_to_awaiting_answer() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.assignment_started(&id, false);
        supervisor
            .report(&id, report(ReportState::Progress, Some("which file?")))
            .expect("report accepted");

        let view = supervisor.view();
        assert_eq!(view.rows()[0].stage, Stage::Person(PersonAction::Answer));
        assert_eq!(view.rows()[0].question.as_deref(), Some("which file?"));
        assert_eq!(view.waiting_count(), 1);
    }

    #[test]
    fn answering_clears_the_question_and_requeues_the_worker() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.assignment_started(&id, false);
        supervisor
            .report(&id, report(ReportState::Progress, Some("which file?")))
            .unwrap();
        let _ = supervisor.next_effect();

        supervisor.answer(&id, "parser.rs").expect("answered");
        assert!(supervisor.view().rows()[0].question.is_none());
        let effect = supervisor.next_effect().expect("deliver queued");
        assert_eq!(effect.effect.kind(), "deliver");
    }

    #[test]
    fn answering_without_a_question_is_refused() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        assert_eq!(
            supervisor.answer(&id, "x"),
            Err(Refusal::NotApplicable("no open question"))
        );
    }

    #[test]
    fn a_ready_report_waits_for_review_and_releases_the_slot() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.assignment_started(&id, false);
        assert_eq!(supervisor.running_count(), 1);
        supervisor
            .report(&id, report(ReportState::Ready, None))
            .unwrap();
        assert_eq!(supervisor.running_count(), 0, "the turn ended");
        assert_eq!(
            supervisor.view().rows()[0].stage,
            Stage::Person(PersonAction::Review)
        );
    }

    #[test]
    fn a_blocked_ticket_can_be_retried_but_a_ready_one_cannot() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.assignment_started(&id, false);
        supervisor
            .report(&id, report(ReportState::Blocked, None))
            .unwrap();

        assert!(supervisor.view().rows()[0].retryable);
        supervisor.retry(&id).expect("retry accepted");
        let _ = supervisor.next_effect();

        supervisor
            .report(&id, report(ReportState::Ready, None))
            .unwrap();
        assert!(!supervisor.view().rows()[0].retryable);
        assert!(matches!(
            supervisor.retry(&id),
            Err(Refusal::NotApplicable(_))
        ));
    }

    #[test]
    fn stopping_ends_the_ticket_and_cancels_the_worker() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.assignment_started(&id, false);
        let _ = supervisor.next_effect();

        supervisor.stop(&id).expect("stopped");
        assert_eq!(supervisor.view().rows()[0].stage, Stage::Ended);
        assert_eq!(supervisor.running_count(), 0);
        let effect = supervisor.next_effect().expect("cancel queued");
        assert_eq!(effect.effect.kind(), "cancel");
    }

    #[test]
    fn an_ended_ticket_rejects_further_reports() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.stop(&id).unwrap();
        assert_eq!(
            supervisor.report(&id, report(ReportState::Ready, None)),
            Err(Refusal::AlreadyEnded(id.clone()))
        );
    }

    #[test]
    fn rows_put_work_that_needs_a_person_first() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("a", "th1"));
        supervisor.receive(&ask("b", "th2"));
        let ids: Vec<String> = supervisor
            .view()
            .rows()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        // "b" is blocked on a person, "a" is merely queued.
        supervisor.assignment_started(&ids[1], false);
        supervisor
            .report(&ids[1], report(ReportState::Ready, None))
            .unwrap();

        let view = supervisor.view();
        assert!(view.rows()[0].stage.needs_person());
        assert_eq!(view.rows()[0].title, "b");
    }

    #[test]
    fn the_view_counts_only_open_work() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("a", "th1"));
        supervisor.receive(&ask("b", "th2"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.stop(&id).unwrap();

        let view = supervisor.view();
        assert_eq!(view.open_count(), 1);
        assert_eq!(view.rows().len(), 2);
    }

    #[test]
    fn an_idle_supervisor_is_not_busy() {
        let supervisor = Supervisor::new();
        assert!(!supervisor.view().is_busy());
        let _ = supervisor.next_effect();
    }

    #[test]
    fn titles_are_one_line_and_bounded() {
        let long = "x".repeat(200);
        let supervisor = Supervisor::new();
        supervisor.receive(&ask(&format!("{long}\nsecond line"), "th1"));
        let view = supervisor.view();
        let title = view.rows()[0].title.clone();
        assert!(!title.contains('\n'));
        assert!(title.chars().count() <= 73, "{title}");
    }

    #[test]
    fn saying_something_reaches_the_existing_ticket_not_a_new_one() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix it", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        let _ = supervisor.next_effect();

        supervisor.say(&id, "also rename it").expect("said");
        assert_eq!(
            supervisor.view().rows().len(),
            1,
            "a message must not create a second ticket"
        );
        let effect = supervisor.next_effect().expect("deliver queued");
        assert_eq!(effect.effect.kind(), "deliver");
        assert!(matches!(
            effect.effect,
            crate::engine::Effect::Deliver { ref ticket_id } if *ticket_id == id
        ));
    }

    #[test]
    fn saying_something_releases_a_blocked_ticket() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix it", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.assignment_started(&id, false);
        supervisor
            .report(&id, report(ReportState::Blocked, None))
            .unwrap();
        assert_eq!(
            supervisor.view().rows()[0].stage,
            Stage::Person(PersonAction::Blocked)
        );

        supervisor.say(&id, "here is the missing piece").unwrap();
        assert_ne!(
            supervisor.view().rows()[0].stage,
            Stage::Person(PersonAction::Blocked),
            "a person's message is what unblocks it"
        );
    }

    #[test]
    fn saying_something_to_an_ended_ticket_is_refused() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix it", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        supervisor.stop(&id).unwrap();
        assert_eq!(
            supervisor.say(&id, "hello?"),
            Err(Refusal::AlreadyEnded(id))
        );
    }

    #[test]
    fn inputs_are_taken_once() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix it", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        let first = supervisor.take_inputs(&id);
        assert!(!first.is_empty(), "the creating ask is an input");
        let second = supervisor.take_inputs(&id);
        assert!(second.is_empty(), "inputs are consumed, not replayed");
    }

    #[test]
    fn rows_expose_the_thread_for_panel_conversations() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("fix it", "th-abc"));
        assert_eq!(supervisor.view().rows()[0].thread, "th-abc");
    }

    #[test]
    fn an_effect_failing_transiently_stays_queued() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("a", "th1"));
        let effect = supervisor.next_effect().expect("launch");
        supervisor.settle_effect(effect, Outcome::Transient);
        assert_eq!(supervisor.view().pending_effects(), 1);
    }

    #[test]
    fn a_person_action_is_reported_per_ticket() {
        let supervisor = Supervisor::new();
        supervisor.receive(&ask("a", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        assert_eq!(
            supervisor.person_action(&id),
            None,
            "queued work needs nobody"
        );
        supervisor.assignment_started(&id, false);
        supervisor
            .report(&id, report(ReportState::Ready, None))
            .unwrap();
        assert_eq!(supervisor.person_action(&id), Some(PersonAction::Review));
        assert_eq!(supervisor.person_action("nope"), None);
    }
}
