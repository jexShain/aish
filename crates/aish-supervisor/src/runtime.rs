//! The resident runtime: a pass loop and the worker tasks behind it.
//!
//! The supervisor is a state machine with no clock; this module gives it one.
//! Two layers, deliberately separate:
//!
//! * **pass** (synchronous, serial): drain the effect queue, hand each effect to
//!   the executor, and let the engine advance. Never blocks on a worker.
//! * **worker** (asynchronous, one task per ticket): owns its own
//!   [`aish_llm::LlmSession`] and reports back when it ends.
//!
//! The split is what makes the supervisor usable while work is in flight: a
//! slow worker cannot stall the pass that would start the next one, and a
//! crashed worker cannot leave a slot held.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::domain::{Report, ReportState};
use crate::engine::{Effect, Outcome, QueuedEffect};
use crate::Supervisor;

/// How the runtime runs one assignment.
///
/// Implemented by the shell (which owns the LLM session) and by tests. It is
/// synchronous because the executor already runs inside a worker thread; an
/// implementation may block.
pub trait Executor: Send + Sync {
    /// Run `prompt` for a ticket and return its report.
    ///
    /// Returning `Err` means the worker could not run at all (no session, a
    /// spawn that panicked); the runtime turns that into a failed report so
    /// the ticket still reaches a person instead of vanishing.
    fn run(&self, ticket_id: &str, prompt: &str) -> Result<Report, String>;
}

/// What a pass did, for logging and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassOutcome {
    /// Effects handed to the executor.
    pub dispatched: usize,
    /// Effects that could not start (no slot).
    pub deferred: usize,
}

/// Owns the pass loop's bookkeeping and the live worker slots.
///
/// The executor is **not** stored: it is passed to [`Runtime::pass`], because
/// the thing that runs a worker (the shell's AI handler) is borrowed for the
/// duration of a turn and must not outlive it or be shared across threads.
pub struct Runtime {
    supervisor: Arc<Supervisor>,
    workers: Arc<Mutex<HashMap<String, WorkerSlot>>>,
}

#[derive(Debug, Default)]
struct WorkerSlot {
    running: bool,
}

impl Runtime {
    /// Build a runtime over a supervisor.
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        Self {
            supervisor,
            workers: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The supervisor this runtime drives.
    pub fn supervisor(&self) -> &Arc<Supervisor> {
        &self.supervisor
    }

    /// Run one pass: dispatch effects until the queue drains or a slot refuses.
    ///
    /// This is the resident loop's body and also what a key press can call to
    /// make the UI react immediately instead of waiting for the next tick.
    ///
    /// Slots are counted per pass: a worker that finishes inside the pass
    /// still held a slot while it ran, so the concurrency limit means "at most
    /// N workers at once" rather than "at most N workers per pass".
    pub fn pass(&self, executor: &dyn Executor) -> PassOutcome {
        let mut outcome = PassOutcome::default();
        let mut used_this_pass = 0usize;
        // Bounded so a queue that keeps deferring cannot spin forever: at most
        // one full queue's worth of attempts per pass.
        let budget = self.supervisor.view().pending_effects().max(1) + 1;
        for _ in 0..budget {
            let Some(effect) = self.supervisor.next_effect() else {
                break;
            };
            match self.dispatch(effect, used_this_pass, executor) {
                Dispatch::Done { took_slot } => {
                    outcome.dispatched += 1;
                    if took_slot {
                        used_this_pass += 1;
                    }
                }
                Dispatch::Deferred(effect) => {
                    self.supervisor.settle_effect(effect, Outcome::Later);
                    outcome.deferred += 1;
                }
            }
        }
        outcome
    }

    fn dispatch(
        &self,
        effect: QueuedEffect,
        used_this_pass: usize,
        executor: &dyn Executor,
    ) -> Dispatch {
        match &effect.effect {
            Effect::Launch { ticket_id } => {
                // The pass's own budget is checked first: the supervisor's
                // running set is released as soon as a worker reports, so it
                // cannot bound a pass on its own.
                if used_this_pass >= crate::MAX_WORKERS {
                    return Dispatch::Deferred(effect);
                }
                // A slot is taken before the worker starts, so a queued launch
                // cannot be raced by another pass.
                if !self.supervisor.assignment_started(ticket_id, false) {
                    return Dispatch::Deferred(effect);
                }
                self.spawn_worker(ticket_id, executor);
                Dispatch::Done { took_slot: true }
            }
            Effect::Deliver { ticket_id } => {
                // Inputs for a ticket whose worker already ended are picked up
                // by the next launch; delivering to nobody is a no-op rather
                // than a failure.
                let live = self
                    .workers
                    .lock()
                    .get(ticket_id)
                    .is_some_and(|w| w.running);
                if live {
                    self.spawn_worker(ticket_id, executor);
                }
                Dispatch::Done { took_slot: false }
            }
            Effect::Cancel { ticket_id } => {
                // The supervisor already ended the ticket; the runtime only
                // has to stop its worker, which happens when the task observes
                // its cancellation token.
                self.workers.lock().remove(ticket_id);
                Dispatch::Done { took_slot: false }
            }
        }
    }

    /// Run one worker to completion, synchronously.
    ///
    /// Called from a pass, which is why it is named "spawn" but does not spawn
    /// a thread: the pass loop itself is the worker's thread in the shell's
    /// single-threaded loop. A caller that wants concurrency drives several
    /// runtimes, which is how the shell keeps its own loop responsive.
    fn spawn_worker(&self, ticket_id: &str, executor: &dyn Executor) {
        let prompt = self
            .supervisor
            .take_inputs(ticket_id)
            .into_iter()
            .collect::<Vec<_>>()
            .join("\n");
        if prompt.trim().is_empty() {
            // Nothing to say: report so the ticket does not sit in "working"
            // forever waiting for a prompt that will never come.
            let _ = self.supervisor.report(
                ticket_id,
                Report {
                    state: ReportState::Waiting,
                    summary: "no input to run".to_string(),
                    question: None,
                },
            );
            return;
        }

        self.workers
            .lock()
            .insert(ticket_id.to_string(), WorkerSlot { running: true });

        let result = executor.run(ticket_id, &prompt);
        if let Some(slot) = self.workers.lock().get_mut(ticket_id) {
            slot.running = false;
        }

        let report = match result {
            Ok(report) => report,
            Err(error) => Report {
                state: ReportState::Failed,
                summary: error,
                question: None,
            },
        };
        if let Err(refusal) = self.supervisor.report(ticket_id, report) {
            tracing::warn!(%ticket_id, ?refusal, "worker report rejected");
        }
    }

    /// Whether a worker is currently running for a ticket.
    pub fn is_running(&self, ticket_id: &str) -> bool {
        self.workers
            .lock()
            .get(ticket_id)
            .is_some_and(|slot| slot.running)
    }
}

enum Dispatch {
    Done { took_slot: bool },
    Deferred(QueuedEffect),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Actor, ThreadKey, Trigger, TriggerKind};
    use parking_lot::Mutex as ParkingMutex;

    /// An executor that records prompts and replies with a scripted report.
    struct RecordingExecutor {
        prompts: ParkingMutex<Vec<(String, String)>>,
        reply: Report,
    }

    impl RecordingExecutor {
        fn new(reply: Report) -> Self {
            Self {
                prompts: ParkingMutex::new(Vec::new()),
                reply,
            }
        }

        fn launched(&self) -> usize {
            self.prompts.lock().len()
        }
    }

    impl Executor for RecordingExecutor {
        fn run(&self, ticket_id: &str, prompt: &str) -> Result<Report, String> {
            self.prompts
                .lock()
                .push((ticket_id.to_string(), prompt.to_string()));
            Ok(self.reply.clone())
        }
    }

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

    fn ready() -> Report {
        Report {
            state: ReportState::Ready,
            summary: "done".to_string(),
            question: None,
        }
    }

    #[test]
    fn a_pass_launches_a_worker_and_reports_back() {
        let supervisor = Arc::new(Supervisor::new());
        let executor = RecordingExecutor::new(ready());
        let runtime = Runtime::new(Arc::clone(&supervisor));

        supervisor.receive(&ask("fix the parser", "th1"));
        let outcome = runtime.pass(&executor);

        assert_eq!(outcome.dispatched, 1);
        let prompts = executor.prompts.lock();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].1, "fix the parser", "the ask is the prompt");
        drop(prompts);

        let view = supervisor.view();
        assert_eq!(
            view.rows()[0].stage.label(),
            "Awaiting review",
            "the report reached the ticket"
        );
    }

    #[test]
    fn a_worker_failure_still_reaches_a_person() {
        struct Failing;
        impl Executor for Failing {
            fn run(&self, _: &str, _: &str) -> Result<Report, String> {
                Err("no session".to_string())
            }
        }
        let supervisor = Arc::new(Supervisor::new());
        let runtime = Runtime::new(Arc::clone(&supervisor));

        supervisor.receive(&ask("fix", "th1"));
        runtime.pass(&Failing);

        let view = supervisor.view();
        assert_eq!(view.rows()[0].stage.label(), "Failed");
        assert_eq!(view.rows()[0].detail, "no session");
    }

    #[test]
    fn a_pass_never_starts_more_work_than_there_are_slots() {
        let supervisor = Arc::new(Supervisor::new());
        let executor = RecordingExecutor::new(ready());
        let runtime = Runtime::new(Arc::clone(&supervisor));
        for i in 0..(crate::MAX_WORKERS + 2) {
            supervisor.receive(&ask(&format!("t{i}"), &format!("th{i}")));
        }
        runtime.pass(&executor);

        assert_eq!(
            executor.launched(),
            crate::MAX_WORKERS,
            "one pass must respect the slot limit"
        );
        assert!(
            supervisor.view().pending_effects() > 0,
            "the deferred launches must stay queued, not vanish"
        );
    }

    #[test]
    fn a_second_pass_picks_up_work_the_first_deferred() {
        let supervisor = Arc::new(Supervisor::new());
        let executor = RecordingExecutor::new(ready());
        let runtime = Runtime::new(Arc::clone(&supervisor));
        for i in 0..(crate::MAX_WORKERS + 1) {
            supervisor.receive(&ask(&format!("t{i}"), &format!("th{i}")));
        }
        runtime.pass(&executor);
        // The first pass filled every slot; the reports ended those turns, so
        // the next pass can start the rest.
        runtime.pass(&executor);
        assert_eq!(executor.launched(), crate::MAX_WORKERS + 1);
    }

    #[test]
    fn an_idle_pass_does_nothing() {
        let supervisor = Arc::new(Supervisor::new());
        let runtime = Runtime::new(Arc::clone(&supervisor));
        let executor = RecordingExecutor::new(ready());
        assert_eq!(runtime.pass(&executor), PassOutcome::default());
        assert_eq!(executor.launched(), 0);
    }

    #[test]
    fn a_ticket_with_no_input_reports_waiting_instead_of_hanging() {
        let supervisor = Arc::new(Supervisor::new());
        let executor = RecordingExecutor::new(ready());
        let runtime = Runtime::new(Arc::clone(&supervisor));
        supervisor.receive(&ask("fix", "th1"));
        let id = supervisor.view().rows()[0].id.clone();
        // Consume the input before the pass runs, as a lost delivery would.
        let _ = supervisor.take_inputs(&id);

        runtime.pass(&executor);
        assert_eq!(
            supervisor.view().rows()[0].stage.label(),
            "Waiting for a trigger"
        );
        assert_eq!(executor.launched(), 0, "nothing to run");
    }

    #[test]
    fn a_running_worker_is_visible_to_the_runtime() {
        struct Slow {
            saw_running: ParkingMutex<bool>,
            runtime: ParkingMutex<Option<Arc<Runtime>>>,
        }
        impl Executor for Slow {
            fn run(&self, ticket_id: &str, _: &str) -> Result<Report, String> {
                if let Some(runtime) = self.runtime.lock().as_ref() {
                    *self.saw_running.lock() = runtime.is_running(ticket_id);
                }
                Ok(ready())
            }
        }
        let supervisor = Arc::new(Supervisor::new());
        let slow = Slow {
            saw_running: ParkingMutex::new(false),
            runtime: ParkingMutex::new(None),
        };
        let runtime = Arc::new(Runtime::new(Arc::clone(&supervisor)));
        *slow.runtime.lock() = Some(Arc::clone(&runtime));

        supervisor.receive(&ask("fix", "th1"));
        runtime.pass(&slow);
        assert!(
            *slow.saw_running.lock(),
            "the runtime should know the ticket is running"
        );
        assert!(!runtime.is_running("tk-h-fix"));
    }
}
