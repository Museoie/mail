//! The global bound on message bodies being fetched at once
//! (imap/body-budget.ts).

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use super::command_budget_hold::yield_command_budget_during;
use crate::concurrency_env::parse_concurrency_value;
use crate::env;
use crate::fifo_semaphore::FifoSemaphore;

const DEFAULT_CONCURRENCY: usize = 3;

fn semaphore() -> &'static FifoSemaphore {
    static SEMAPHORE: OnceLock<FifoSemaphore> = OnceLock::new();
    SEMAPHORE.get_or_init(|| {
        FifoSemaphore::new(parse_concurrency_value(
            env::var("IMAP_BODY_FETCH_CONCURRENCY").as_deref(),
            "IMAP_BODY_FETCH_CONCURRENCY",
            DEFAULT_CONCURRENCY,
            "body-budget",
        ))
    })
}

tokio::task_local! {
    /// Per-command wait accumulator, bound at the top of the command handler:
    /// every acquire made inside it, however deep, adds its measured wait
    /// here, so a command's total is never mixed with another socket's.
    static WAIT_LEDGER: Arc<AtomicU64>;
}

pub async fn run_in_body_budget_context<F: Future>(work: F) -> F::Output {
    WAIT_LEDGER.scope(Arc::new(AtomicU64::new(0)), work).await
}

/// How long the command being run has waited for the body budget so far, in
/// milliseconds. Zero outside a command.
pub fn get_body_budget_wait_ms() -> f64 {
    WAIT_LEDGER.try_with(|ledger| ledger.load(Ordering::SeqCst) as f64 / 1000.0).unwrap_or(0.0)
}

/// A slot in the body budget, given back when dropped.
#[must_use = "the slot is released when this is dropped"]
pub struct BodyBudgetSlot(());

impl Drop for BodyBudgetSlot {
    fn drop(&mut self) {
        semaphore().release();
    }
}

/// Takes a slot for as long as the returned value lives.
///
/// This is what a streamed body holds for the whole of its stream: the
/// streaming path is the largest fetch shape, so it has to sit inside the
/// same bound as the materializing ones. The slot goes back on completion,
/// on an error, and when the consumer abandons the stream on a dead socket.
pub async fn acquire_body_budget() -> BodyBudgetSlot {
    // An uncontended acquire must not go through the yield: giving the
    // command slot up and taking it back costs a full command-queue rotation
    // under saturation, which would dwarf the wait it is meant to hide.
    if semaphore().try_acquire() {
        return BodyBudgetSlot(());
    }
    let waited = Arc::new(AtomicU64::new(0));
    yield_command_budget_during({
        let waited = waited.clone();
        async move {
            let wait: Duration = semaphore().acquire().await;
            waited.store(wait.as_micros() as u64, Ordering::SeqCst);
        }
    })
    .await;
    let _ = WAIT_LEDGER.try_with(|ledger| ledger.fetch_add(waited.load(Ordering::SeqCst), Ordering::SeqCst));
    BodyBudgetSlot(())
}

/// Runs `work` holding a slot.
pub async fn with_body_budget<F: Future>(work: F) -> F::Output {
    let _slot = acquire_body_budget().await;
    work.await
}

/// Forgets every slot and waiter. For tests.
pub fn reset_body_budget() {
    semaphore().reset();
}

pub fn body_budget_capacity() -> usize {
    semaphore().capacity()
}

/// Tests share the process-wide budget; they take this first.
#[cfg(test)]
pub(crate) async fn test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use tokio::sync::oneshot;

    use super::*;

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn runs_up_to_cap_callers_concurrently() {
        let _lock = test_lock().await;
        reset_body_budget();
        let cap = body_budget_capacity();
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..cap * 3 {
            let (running, peak) = (running.clone(), peak.clone());
            tasks.push(tokio::spawn(with_body_budget(async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                running.fetch_sub(1, Ordering::SeqCst);
            })));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), cap);
    }

    #[tokio::test]
    async fn wakes_multiple_queued_callers_in_fifo_order() {
        let _lock = test_lock().await;
        reset_body_budget();
        let cap = body_budget_capacity();
        let mut holders = Vec::new();
        for _ in 0..cap {
            holders.push(acquire_body_budget().await);
        }
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for n in 0..3 {
            let order = order.clone();
            tasks.push(tokio::spawn(with_body_budget(async move {
                order.lock().unwrap().push(n);
            })));
            settle().await;
        }
        drop(holders);
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
    }

    #[tokio::test]
    async fn queues_an_extra_caller_until_a_slot_frees() {
        let _lock = test_lock().await;
        reset_body_budget();
        let cap = body_budget_capacity();
        let mut holders = Vec::new();
        for _ in 0..cap {
            holders.push(acquire_body_budget().await);
        }
        let (entered, was_entered) = oneshot::channel::<()>();
        let queued = tokio::spawn(with_body_budget(async move {
            let _ = entered.send(());
        }));
        settle().await;
        assert!(!queued.is_finished());
        holders.pop();
        was_entered.await.unwrap();
        queued.await.unwrap();
    }

    #[tokio::test]
    async fn attributes_wait_time_to_the_commands_own_ledger() {
        let _lock = test_lock().await;
        reset_body_budget();
        let cap = body_budget_capacity();
        let mut holders = Vec::new();
        for _ in 0..cap {
            holders.push(acquire_body_budget().await);
        }
        // One command queues; another, in its own context, never waits.
        let waiter = tokio::spawn(run_in_body_budget_context(async {
            with_body_budget(async {}).await;
            get_body_budget_wait_ms()
        }));
        tokio::time::sleep(Duration::from_millis(40)).await;
        let bystander = run_in_body_budget_context(async { get_body_budget_wait_ms() }).await;
        holders.clear();
        let waited = waiter.await.unwrap();
        assert!(waited >= 30.0, "waited {waited}ms");
        assert_eq!(bystander, 0.0);
    }

    #[tokio::test]
    async fn releases_when_the_work_is_abandoned_so_later_callers_still_run() {
        let _lock = test_lock().await;
        reset_body_budget();
        let cap = body_budget_capacity();
        for _ in 0..cap {
            let stuck = tokio::spawn(with_body_budget(std::future::pending::<()>()));
            settle().await;
            stuck.abort();
            let _ = stuck.await;
        }
        // Every slot came back.
        let mut holders = Vec::new();
        for _ in 0..cap {
            holders.push(acquire_body_budget().await);
        }
        assert_eq!(holders.len(), cap);
    }

    #[tokio::test]
    async fn the_ledger_stays_at_zero_when_the_caller_acquires_without_queuing() {
        let _lock = test_lock().await;
        reset_body_budget();
        let waited = run_in_body_budget_context(async {
            with_body_budget(async {}).await;
            get_body_budget_wait_ms()
        })
        .await;
        assert_eq!(waited, 0.0);
    }

    #[test]
    fn the_wait_outside_a_context_is_zero() {
        assert_eq!(get_body_budget_wait_ms(), 0.0);
    }
}
