//! The authorization gate and effect queue — the Engine's two decision points.
//!
//! [`gate`] answers "may this trigger start work?"; [`EffectQueue`] is how a
//! decision becomes an action without doing IO inside a transaction. Both are
//! pure enough to unit-test without a worker, a terminal or a database.

use std::collections::VecDeque;

use crate::domain::{Actor, Basis, Decision, Mandate, Strength, ThreadKey, Trigger, TriggerKind};

/// Why a trigger may not start work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The event says nothing that asks for work.
    NotARequest,
    /// A system event arrived on a thread with no open ticket, so there is
    /// nothing to attach it to.
    NoTicket,
}

impl Rejection {
    /// A one-line reason, for the audit trail.
    pub fn reason(self) -> &'static str {
        match self {
            Rejection::NotARequest => "not_a_request",
            Rejection::NoTicket => "no_open_ticket",
        }
    }
}

/// Decide whether a trigger starts work.
///
/// The policy is deliberately small: aish is a single-user local shell, so
/// "who may ask" needs no configured allow-list. What still matters is the
/// *strength* of the basis, because it decides how far the worker may go.
pub fn gate(trigger: &Trigger, thread_has_open_ticket: bool) -> Result<Mandate, Rejection> {
    // A person asking for something is the only explicit basis there is.
    if trigger.kind == TriggerKind::UserAsk && trigger.actor == Actor::User {
        return Ok(Mandate {
            strength: Strength::Explicit,
            basis: Basis::Direct,
            scope: trigger.text.trim().to_string(),
        });
    }
    // A system event can only continue work that already exists. This is what
    // keeps a background task completing from silently starting new work.
    match trigger.kind {
        TriggerKind::TaskDone | TriggerKind::AgentReport | TriggerKind::UserAnswer => {
            if thread_has_open_ticket {
                Ok(Mandate {
                    strength: Strength::Inferred,
                    basis: Basis::Followup,
                    scope: trigger.text.trim().to_string(),
                })
            } else {
                Err(Rejection::NoTicket)
            }
        }
        TriggerKind::UserAsk => Err(Rejection::NotARequest),
    }
}

/// Resolve a gate result and the thread's state into a decision.
pub fn decide(trigger: &Trigger, thread_has_open_ticket: bool) -> Decision {
    match gate(trigger, thread_has_open_ticket) {
        Ok(mandate) => Decision::Accept(mandate),
        Err(rejection) => Decision::Ignore(rejection.reason().to_string()),
    }
}

/// The thread a trigger belongs to, and whether it already owns work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadHint {
    /// The thread.
    pub key: ThreadKey,
    /// Whether an open ticket already subscribes to it.
    pub has_open_ticket: bool,
}

/// One side effect a command decided on.
///
/// Effects are produced inside the (synchronous, in-memory or transactional)
/// command and executed afterwards. That split is the whole reason a crash
/// cannot lose "we already decided to launch this": the decision is durable
/// before the action is attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Start a worker for a ticket.
    Launch {
        /// The ticket to run.
        ticket_id: String,
    },
    /// Hand inputs to a running worker.
    Deliver {
        /// The ticket.
        ticket_id: String,
    },
    /// Ask the worker to stop.
    Cancel {
        /// The ticket.
        ticket_id: String,
    },
}

impl Effect {
    /// A short kind, for per-kind concurrency and health grouping.
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Launch { .. } => "launch",
            Effect::Deliver { .. } => "deliver",
            Effect::Cancel { .. } => "cancel",
        }
    }
}

/// A queued effect with its retry bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedEffect {
    /// Monotonic sequence number; also the identity for dedup-free replay.
    pub seq: u64,
    /// What to do.
    pub effect: Effect,
    /// How many times it has been attempted.
    pub attempts: u32,
    /// Whether the last attempt failed in a way that may be retried.
    pub retryable: bool,
}

/// Why an attempt did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It worked.
    Done,
    /// It failed and should be retried after a backoff.
    Transient,
    /// It cannot run yet (no slot, paused); retry later without counting it as
    /// a failure.
    Later,
    /// It failed permanently.
    Permanent,
}

/// A FIFO of effects with bounded retries.
///
/// Deliberately in-memory for now: persistence is `S4` in the design doc, and
/// the retry semantics are what S1 is meant to pin down.
#[derive(Debug, Default)]
pub struct EffectQueue {
    pending: VecDeque<QueuedEffect>,
    next_seq: u64,
}

/// Attempts before an effect is dropped as failed.
pub const MAX_ATTEMPTS: u32 = 3;

impl EffectQueue {
    /// An empty queue.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enqueue an effect, returning its sequence number.
    pub fn enqueue(&mut self, effect: Effect) -> u64 {
        self.next_seq += 1;
        let seq = self.next_seq;
        self.pending.push_back(QueuedEffect {
            seq,
            effect,
            attempts: 0,
            retryable: true,
        });
        seq
    }

    /// Take the next runnable effect.
    ///
    /// Effects are returned in order so a `launch` always precedes the
    /// `deliver` that depends on it, which is what keeps a ticket from being
    /// handed inputs before it exists.
    pub fn take_next(&mut self) -> Option<QueuedEffect> {
        self.pending.pop_front()
    }

    /// Record how an attempt went, putting the effect back when it should run
    /// again.
    ///
    /// `Later` never counts against the attempt budget: being out of worker
    /// slots is not a failure.
    pub fn settle(&mut self, mut effect: QueuedEffect, outcome: Outcome) {
        match outcome {
            Outcome::Done | Outcome::Permanent => {}
            Outcome::Later => {
                effect.retryable = true;
                self.pending.push_back(effect);
            }
            Outcome::Transient => {
                effect.attempts += 1;
                if effect.attempts < MAX_ATTEMPTS {
                    effect.retryable = true;
                    self.pending.push_back(effect);
                }
            }
        }
    }

    /// How many effects are waiting.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Drop everything (a paused supervisor still keeps its decisions; this is
    /// only for teardown).
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trigger(kind: TriggerKind, actor: Actor) -> Trigger {
        Trigger {
            id: "tr".to_string(),
            kind,
            actor,
            text: "do the thing".to_string(),
            thread: ThreadKey("th".to_string()),
            at: 0,
        }
    }

    #[test]
    fn a_user_ask_is_explicit_and_direct() {
        let decision = decide(&trigger(TriggerKind::UserAsk, Actor::User), false);
        let Decision::Accept(mandate) = decision else {
            panic!("a user ask must be accepted: {decision:?}");
        };
        assert_eq!(mandate.strength, Strength::Explicit);
        assert_eq!(mandate.basis, Basis::Direct);
        assert!(mandate.allows_breadth());
    }

    #[test]
    fn a_system_event_only_continues_existing_work() {
        let standalone = decide(&trigger(TriggerKind::TaskDone, Actor::Task), false);
        assert_eq!(
            standalone,
            Decision::Ignore("no_open_ticket".to_string()),
            "a background task must not start work on its own"
        );

        let followup = decide(&trigger(TriggerKind::TaskDone, Actor::Task), true);
        let Decision::Accept(mandate) = followup else {
            panic!("an attached event continues the ticket");
        };
        assert_eq!(mandate.strength, Strength::Inferred);
        assert!(!mandate.allows_breadth(), "inferred work stays small");
    }

    #[test]
    fn an_agent_report_on_a_live_thread_is_a_followup() {
        let decision = decide(&trigger(TriggerKind::AgentReport, Actor::Agent), true);
        assert!(matches!(decision, Decision::Accept(_)));
    }

    #[test]
    fn a_user_answer_is_a_followup_not_new_work() {
        let decision = decide(&trigger(TriggerKind::UserAnswer, Actor::User), true);
        let Decision::Accept(mandate) = decision else {
            panic!("an answer continues the ticket");
        };
        assert_eq!(mandate.strength, Strength::Inferred);
    }

    #[test]
    fn the_scope_is_the_request_text() {
        let mut t = trigger(TriggerKind::UserAsk, Actor::User);
        t.text = "  rename the button  ".to_string();
        let Decision::Accept(mandate) = decide(&t, false) else {
            panic!("accepted");
        };
        assert_eq!(mandate.scope, "rename the button");
    }

    #[test]
    fn effects_run_in_the_order_they_were_queued() {
        let mut queue = EffectQueue::new();
        queue.enqueue(Effect::Launch {
            ticket_id: "t1".to_string(),
        });
        queue.enqueue(Effect::Deliver {
            ticket_id: "t1".to_string(),
        });

        let first = queue.take_next().expect("launch");
        assert_eq!(first.effect.kind(), "launch");
        queue.settle(first, Outcome::Done);
        let second = queue.take_next().expect("deliver");
        assert_eq!(second.effect.kind(), "deliver");
    }

    #[test]
    fn a_transient_failure_is_retried_up_to_the_budget() {
        let mut queue = EffectQueue::new();
        queue.enqueue(Effect::Launch {
            ticket_id: "t1".to_string(),
        });
        for expected_attempts in 1..MAX_ATTEMPTS {
            let effect = queue.take_next().expect("still retrying");
            queue.settle(effect, Outcome::Transient);
            assert_eq!(queue.len(), 1);
            let peeked = queue.take_next().expect("requeued");
            assert_eq!(peeked.attempts, expected_attempts);
            queue.settle(peeked, Outcome::Later);
        }
    }

    #[test]
    fn a_transient_failure_stops_at_the_budget() {
        let mut queue = EffectQueue::new();
        queue.enqueue(Effect::Launch {
            ticket_id: "t1".to_string(),
        });
        let mut effect = queue.take_next().expect("queued");
        for _ in 0..MAX_ATTEMPTS {
            queue.settle(effect, Outcome::Transient);
            match queue.take_next() {
                Some(next) => effect = next,
                None => return,
            }
        }
        panic!("the effect outlived its attempt budget");
    }

    #[test]
    fn later_never_consumes_the_attempt_budget() {
        // Being out of worker slots is not a failure: a queued launch must be
        // able to wait indefinitely.
        let mut queue = EffectQueue::new();
        queue.enqueue(Effect::Launch {
            ticket_id: "t1".to_string(),
        });
        for _ in 0..(MAX_ATTEMPTS * 10) {
            let effect = queue.take_next().expect("still queued");
            assert_eq!(effect.attempts, 0, "Later must not count as a failure");
            queue.settle(effect, Outcome::Later);
        }
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn a_permanent_failure_is_dropped() {
        let mut queue = EffectQueue::new();
        queue.enqueue(Effect::Cancel {
            ticket_id: "t1".to_string(),
        });
        let effect = queue.take_next().expect("queued");
        queue.settle(effect, Outcome::Permanent);
        assert!(queue.is_empty());
    }

    #[test]
    fn a_done_effect_is_not_requeued() {
        let mut queue = EffectQueue::new();
        queue.enqueue(Effect::Deliver {
            ticket_id: "t1".to_string(),
        });
        let effect = queue.take_next().expect("queued");
        queue.settle(effect, Outcome::Done);
        assert!(queue.is_empty());
    }

    #[test]
    fn sequence_numbers_are_strictly_increasing() {
        let mut queue = EffectQueue::new();
        let first = queue.enqueue(Effect::Cancel {
            ticket_id: "a".to_string(),
        });
        let second = queue.enqueue(Effect::Cancel {
            ticket_id: "b".to_string(),
        });
        assert!(second > first);
    }
}
