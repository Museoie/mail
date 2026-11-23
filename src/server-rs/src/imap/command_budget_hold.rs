//! Whether the command being run holds a command-budget slot
//! (imap/command-budget-hold.ts).

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::command_budget::{acquire_command_budget, release_command_budget};

/// One command's claim on the command budget.
#[derive(Debug, Default)]
pub struct CommandBudgetHold {
    held: AtomicBool,
    /// Total time this command has spent queued for a slot: the first
    /// acquire plus every reacquire after a yield. A fetch that takes its
    /// slot at once and then waits seconds to get back into a saturated
    /// queue has to read as a full budget, not as a slow database.
    waited_micros: AtomicU64,
}

impl CommandBudgetHold {
    pub fn held(&self) -> bool {
        self.held.load(Ordering::SeqCst)
    }

    pub fn waited(&self) -> Duration {
        Duration::from_micros(self.waited_micros.load(Ordering::SeqCst))
    }

    pub fn waited_ms(&self) -> f64 {
        self.waited().as_secs_f64() * 1000.0
    }

    fn add_wait(&self, wait: Duration) {
        self.waited_micros.fetch_add(wait.as_micros() as u64, Ordering::SeqCst);
    }
}

tokio::task_local! {
    /// Bound in `handler.rs` around a budgeted command's whole processing, so
    /// `yield_command_budget_during` knows whether THIS command holds a slot,
    /// and so the handler's own release reads the flag the yield changes.
    static HOLD: Arc<CommandBudgetHold>;
}

pub fn create_command_budget_hold(held: bool, waited: Duration) -> Arc<CommandBudgetHold> {
    let hold = CommandBudgetHold { held: AtomicBool::new(held), waited_micros: AtomicU64::new(0) };
    hold.add_wait(waited);
    Arc::new(hold)
}

pub async fn run_in_command_budget_context<F: Future>(hold: Arc<CommandBudgetHold>, work: F) -> F::Output {
    HOLD.scope(hold, work).await
}

/// Runs `wait` — another resource's blocking acquire — with the calling
/// command's command-budget slot given up, and takes the slot back before
/// returning.
///
/// Without this, a slot taken before dispatch stays held for however long
/// the command then queues behind a DIFFERENT resource's limit (the body
/// budget, the stream mutex). That nests two budgets instead of partitioning
/// them: a burst of exactly the commands this budget bounds fills it with
/// commands that are themselves only queued elsewhere, starving every other
/// command behind them.
///
/// The window is the inner wait and nothing more. Callers pass only a wait
/// that really blocks: the reacquire costs a full queue rotation under
/// saturation, so an uncontended acquire takes its own fast path instead.
///
/// The reacquire runs while holding the inner resource, but nothing waits on
/// that resource while holding a command slot, so there is no cycle.
pub async fn yield_command_budget_during<F: Future<Output = ()>>(wait: F) {
    let hold = HOLD.try_with(Arc::clone).ok().filter(|hold| hold.held());
    let Some(hold) = hold else {
        wait.await;
        return;
    };
    release_command_budget();
    // If this future is dropped during the wait the hold stays released, and
    // the handler, which reads the same flag, does not release it again.
    hold.held.store(false, Ordering::SeqCst);
    wait.await;
    hold.add_wait(acquire_command_budget().await);
    hold.held.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use tokio::sync::oneshot;

    use super::super::command_budget::{
        command_budget_capacity, command_budget_in_flight, reset_command_budget, test_lock,
    };
    use super::*;

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn gives_up_a_held_slot_for_the_wait_and_reacquires_it_before_returning() {
        let _lock = test_lock().await;
        reset_command_budget();
        acquire_command_budget().await;
        let hold = create_command_budget_hold(true, Duration::ZERO);
        let during = Arc::new(AtomicUsize::new(usize::MAX));

        run_in_command_budget_context(hold.clone(), {
            let during = during.clone();
            async move {
                yield_command_budget_during(async move {
                    during.store(command_budget_in_flight(), Ordering::SeqCst);
                })
                .await;
            }
        })
        .await;

        assert_eq!(during.load(Ordering::SeqCst), 0);
        assert_eq!(command_budget_in_flight(), 1);
        assert!(hold.held());
        reset_command_budget();
    }

    #[tokio::test]
    async fn is_a_no_op_when_the_command_never_held_a_slot() {
        let _lock = test_lock().await;
        reset_command_budget();
        let hold = create_command_budget_hold(false, Duration::ZERO);
        run_in_command_budget_context(hold.clone(), async {
            yield_command_budget_during(async {
                assert_eq!(command_budget_in_flight(), 0);
            })
            .await;
        })
        .await;
        assert_eq!(command_budget_in_flight(), 0);
        assert!(!hold.held());

        // And outside any command at all.
        yield_command_budget_during(async {}).await;
        assert_eq!(command_budget_in_flight(), 0);
    }

    #[tokio::test]
    async fn frees_the_slot_for_another_command_while_waiting_and_queues_to_get_back_in() {
        let _lock = test_lock().await;
        reset_command_budget();
        let cap = command_budget_capacity();
        for _ in 0..cap {
            acquire_command_budget().await;
        }
        let hold = create_command_budget_hold(true, Duration::ZERO);
        let (finish_wait, wait_finished) = oneshot::channel::<()>();

        let yielding = tokio::spawn(run_in_command_budget_context(hold.clone(), async move {
            yield_command_budget_during(async move {
                let _ = wait_finished.await;
            })
            .await;
        }));
        settle().await;
        assert_eq!(command_budget_in_flight(), cap - 1);
        assert!(!hold.held());

        // Another command takes the slot that was given up.
        assert_eq!(acquire_command_budget().await, Duration::ZERO);
        assert_eq!(command_budget_in_flight(), cap);

        // The wait is over, but the budget is full again: the yielder queues.
        finish_wait.send(()).unwrap();
        settle().await;
        assert!(!yielding.is_finished());
        assert!(!hold.held());

        release_command_budget();
        yielding.await.unwrap();
        assert!(hold.held());
        assert_eq!(command_budget_in_flight(), cap);
        reset_command_budget();
    }

    #[tokio::test]
    async fn credits_the_reacquire_queue_time_to_the_hold() {
        let _lock = test_lock().await;
        reset_command_budget();
        let cap = command_budget_capacity();
        for _ in 0..cap {
            acquire_command_budget().await;
        }
        let hold = create_command_budget_hold(true, Duration::from_millis(5));
        let (taken, slot_taken) = oneshot::channel::<()>();

        let yielding = tokio::spawn(run_in_command_budget_context(hold.clone(), async move {
            yield_command_budget_during(async move {
                // Someone else takes the slot before this wait is over.
                acquire_command_budget().await;
                let _ = taken.send(());
            })
            .await;
        }));
        slot_taken.await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        release_command_budget();
        yielding.await.unwrap();

        assert!(hold.waited() >= Duration::from_millis(5 + 30), "waited {:?}", hold.waited());
        reset_command_budget();
    }

    #[tokio::test]
    async fn an_abandoned_wait_leaves_the_hold_released() {
        let _lock = test_lock().await;
        reset_command_budget();
        acquire_command_budget().await;
        let hold = create_command_budget_hold(true, Duration::ZERO);
        let yielding = tokio::spawn(run_in_command_budget_context(hold.clone(), async {
            yield_command_budget_during(std::future::pending()).await;
        }));
        settle().await;
        yielding.abort();
        let _ = yielding.await;
        // The slot went back when the wait began, and the hold says so: the
        // handler's release must not hand it back a second time.
        assert!(!hold.held());
        assert_eq!(command_budget_in_flight(), 0);
    }
}
