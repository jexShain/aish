//! Domain types for authorized work.
//!
//! The vocabulary is taken from `docs/tui-superagent.md`, which adapts it from
//! dsh-superagent. Only concepts the work needs are here: a business need
//! becomes an attribute of one of these types rather than a new type.
//!
//! The one rule that shapes everything else: **a ticket's stage is derived,
//! never stored.** Storing it would let the store and the code disagree.

use std::fmt;

/// Who produced a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Actor {
    /// The person at the keyboard.
    #[default]
    User,
    /// A background task reporting completion.
    Task,
    /// A sub-agent reporting back.
    Agent,
    /// The supervisor itself (a retry it scheduled).
    System,
}

impl Actor {
    /// Whether a person produced this.
    pub fn is_human(self) -> bool {
        matches!(self, Actor::User)
    }
}

/// What kind of event a trigger is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// The user asked for something.
    UserAsk,
    /// A background task finished.
    TaskDone,
    /// A sub-agent returned a result.
    AgentReport,
    /// A person answered a question the worker asked.
    UserAnswer,
}

/// One immutable event.
///
/// `id` is content-addressed by the caller so the same event can never create
/// two tickets; `intake` is the only path into the domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    /// Content-addressed id, stable across restarts.
    pub id: String,
    /// What happened.
    pub kind: TriggerKind,
    /// Who caused it.
    pub actor: Actor,
    /// The event's text, as the actor wrote it.
    pub text: String,
    /// The thread it belongs to (a conversation, a ticket's follow-ups).
    pub thread: ThreadKey,
    /// When it happened, in the caller's clock.
    pub at: u64,
}

impl Trigger {
    /// Whether a person produced this trigger.
    pub fn is_human(&self) -> bool {
        self.actor.is_human()
    }
}

/// Identity of a thread: triggers in time order share one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct ThreadKey(pub String);

impl fmt::Display for ThreadKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// How strong the basis for an authorization is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    /// Derived from context; the work must stay small and in scope.
    Inferred,
    /// The person asked for it.
    Explicit,
}

/// Why a mandate exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The person asked directly.
    Direct,
    /// A follow-up on a thread that already has an open ticket.
    Followup,
}

/// What an accept grants. Immutable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mandate {
    /// How strong the basis is.
    pub strength: Strength,
    /// Why it was granted.
    pub basis: Basis,
    /// The scope the work may cover, as one line.
    pub scope: String,
}

impl Mandate {
    /// Whether this mandate allows acting beyond the literal request.
    pub fn allows_breadth(&self) -> bool {
        self.strength == Strength::Explicit
    }
}

/// The outcome of an evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Start work, with this mandate.
    Accept(Mandate),
    /// Do nothing, for this reason.
    Ignore(String),
    /// Stop the ticket this trigger belongs to.
    Stop(String),
}

/// A structured status report from a worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// What the worker is saying.
    pub state: ReportState,
    /// One line of detail.
    pub summary: String,
    /// A question that blocks progress, if any.
    ///
    /// Only a question here can move a ticket to `person/answer`; free text is
    /// never parsed for "is this a question".
    pub question: Option<String>,
}

/// The state a report carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportState {
    /// Still working.
    Progress,
    /// Turn ended; ready for the next input.
    Waiting,
    /// Finished; waiting for review.
    Ready,
    /// Cannot continue.
    Blocked,
    /// Failed.
    Failed,
    /// Found nothing to do.
    Dropped,
}

/// Assignment lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AssignmentState {
    /// Created, not yet launched.
    #[default]
    Pending,
    /// A worker is running it.
    Running,
    /// Over.
    Ended,
}

/// The binding of a ticket to one worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// Stable id.
    pub id: String,
    /// Lifecycle.
    pub state: AssignmentState,
    /// The most recent report.
    pub report: Option<Report>,
}

/// Context delivered to a ticket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketInput {
    /// The trigger that carries it.
    pub trigger_id: String,
    /// Whether a worker has taken it up.
    pub read: bool,
}

/// A unit of authorized work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    /// Stable id, derived from the accepting trigger.
    pub id: String,
    /// One-line title, taken from the trigger.
    pub title: String,
    /// Why it may be done.
    pub mandate: Mandate,
    /// The thread it lives on.
    pub thread: ThreadKey,
    /// Whether it is over.
    pub ended: bool,
    /// A question waiting for a person.
    pub attention: Option<String>,
    /// Its assignments, oldest first.
    pub assignments: Vec<Assignment>,
    /// Context handed to it.
    pub inputs: Vec<TicketInput>,
}

impl Ticket {
    /// The most recent assignment, if any.
    pub fn latest_assignment(&self) -> Option<&Assignment> {
        self.assignments.last()
    }

    /// The most recent report, if any.
    pub fn latest_report(&self) -> Option<&Report> {
        self.latest_assignment().and_then(|a| a.report.as_ref())
    }

    /// Whether a report is waiting on a person and nobody has spoken since.
    ///
    /// A person's message returns the ticket to the worker, which is what makes
    /// `blocked`/`failed` recoverable without a "restart" command.
    pub fn waits_for_person(&self) -> bool {
        let Some(report) = self.latest_report() else {
            return false;
        };
        if !matches!(report.state, ReportState::Blocked | ReportState::Failed) {
            return false;
        }
        // Any unread input from a person counts as a newer follow-up.
        !self.inputs.iter().any(|input| !input.read)
    }
}
