//! The global bound on memory-heavy IMAP commands running at once
//! (imap/command-budget.ts).

use std::sync::OnceLock;
use std::time::Duration;

use crate::concurrency_env::parse_concurrency_value;
use crate::env;
use crate::fifo_semaphore::FifoSemaphore;

const DEFAULT_CONCURRENCY: usize = 8;

fn semaphore() -> &'static FifoSemaphore {
    static SEMAPHORE: OnceLock<FifoSemaphore> = OnceLock::new();
    SEMAPHORE.get_or_init(|| {
        FifoSemaphore::new(parse_concurrency_value(
            env::var("IMAP_COMMAND_CONCURRENCY").as_deref(),
            "IMAP_COMMAND_CONCURRENCY",
            DEFAULT_CONCURRENCY,
            "command-budget",
        ))
    })
}

/// Takes a slot in the global command budget: at once when one is free,
/// otherwise in arrival order once one frees up. Returns how long the wait
/// was, for diagnostics.
pub async fn acquire_command_budget() -> Duration {
    semaphore().acquire().await
}

pub fn release_command_budget() {
    semaphore().release();
}

/// Forgets every slot and waiter. For tests.
pub fn reset_command_budget() {
    semaphore().reset();
}

pub fn command_budget_capacity() -> usize {
    semaphore().capacity()
}

pub fn command_budget_in_flight() -> usize {
    semaphore().in_flight()
}

/// Tests that saturate the process-wide budget, or dispatch a budgeted
/// command for real, take this first so they cannot starve each other.
#[cfg(test)]
pub(crate) async fn test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn runs_up_to_cap_callers_concurrently() {
        let _lock = test_lock().await;
        reset_command_budget();
        let cap = command_budget_capacity();
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..cap * 2 {
            let (running, peak) = (running.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                acquire_command_budget().await;
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                release_command_budget();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), cap);
        assert_eq!(command_budget_in_flight(), 0);
    }

    #[tokio::test]
    async fn queues_an_extra_caller_until_a_slot_frees() {
        let _lock = test_lock().await;
        reset_command_budget();
        let cap = command_budget_capacity();
        for _ in 0..cap {
            acquire_command_budget().await;
        }
        let entered = Arc::new(AtomicUsize::new(0));
        let queued = tokio::spawn({
            let entered = entered.clone();
            async move {
                acquire_command_budget().await;
                entered.fetch_add(1, Ordering::SeqCst);
            }
        });
        settle().await;
        assert_eq!(entered.load(Ordering::SeqCst), 0);
        release_command_budget();
        queued.await.unwrap();
        assert_eq!(entered.load(Ordering::SeqCst), 1);
        assert_eq!(command_budget_in_flight(), cap);
        reset_command_budget();
    }

    #[tokio::test]
    async fn wakes_multiple_queued_callers_in_fifo_order() {
        let _lock = test_lock().await;
        reset_command_budget();
        let cap = command_budget_capacity();
        for _ in 0..cap {
            acquire_command_budget().await;
        }
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for n in 0..3 {
            let order = order.clone();
            tasks.push(tokio::spawn(async move {
                acquire_command_budget().await;
                order.lock().unwrap().push(n);
            }));
            settle().await;
        }
        for _ in 0..3 {
            release_command_budget();
            settle().await;
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec![0, 1, 2]);
        reset_command_budget();
    }

    #[tokio::test]
    async fn acquiring_below_capacity_resolves_with_zero_wait() {
        let _lock = test_lock().await;
        reset_command_budget();
        assert_eq!(acquire_command_budget().await, Duration::ZERO);
        release_command_budget();
    }

    #[tokio::test]
    async fn a_queued_acquire_reports_non_zero_wait_once_woken() {
        let _lock = test_lock().await;
        reset_command_budget();
        let cap = command_budget_capacity();
        for _ in 0..cap {
            acquire_command_budget().await;
        }
        let queued = tokio::spawn(acquire_command_budget());
        tokio::time::sleep(Duration::from_millis(30)).await;
        release_command_budget();
        assert!(queued.await.unwrap() >= Duration::from_millis(20));
        reset_command_budget();
    }

    #[tokio::test]
    async fn an_unmatched_release_cannot_lift_the_ceiling_above_capacity() {
        let _lock = test_lock().await;
        reset_command_budget();
        let cap = command_budget_capacity();
        release_command_budget();
        release_command_budget();
        for _ in 0..cap {
            assert_eq!(acquire_command_budget().await, Duration::ZERO);
        }
        assert_eq!(command_budget_in_flight(), cap);
        let extra = tokio::spawn(acquire_command_budget());
        settle().await;
        assert!(!extra.is_finished(), "a slot past the capacity must queue");
        release_command_budget();
        extra.await.unwrap();
        reset_command_budget();
    }
}
