//! The SMTP DATA budget: the cases of `src/server/lib/smtp-data-budget.test.ts`.
//!
//! The budget is one per process, so the tests take turns.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, Mutex, MutexGuard};

use mail_server::smtp::data_budget::{
    reset_smtp_data_budget, smtp_data_budget_capacity, smtp_data_budget_in_flight, smtp_data_budget_wait_ceiling_ms,
    with_smtp_data_budget, DataBudgetUnavailableError,
};
use mail_server::smtp::SmtpHookError;

static TURN: Mutex<()> = Mutex::const_new(());

async fn fresh_budget() -> MutexGuard<'static, ()> {
    let turn = TURN.lock().await;
    reset_smtp_data_budget();
    turn
}

/// Lets the tasks that are ready run.
async fn next_tick() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

type Gate = oneshot::Sender<()>;
type Held = tokio::task::JoinHandle<Result<(), DataBudgetUnavailableError>>;

/// Holds every slot until the returned gates are opened.
async fn saturate() -> (Vec<Gate>, Vec<Held>) {
    let mut gates = Vec::new();
    let mut held = Vec::new();
    for _ in 0..smtp_data_budget_capacity() {
        let (gate, opened) = oneshot::channel::<()>();
        gates.push(gate);
        held.push(tokio::spawn(with_smtp_data_budget(|| async move {
            let _ = opened.await;
        })));
    }
    next_tick().await;
    (gates, held)
}

async fn release(gates: Vec<Gate>, held: Vec<Held>) {
    for gate in gates {
        let _ = gate.send(());
    }
    for transaction in held {
        transaction.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn admits_capacity_transactions_at_once_and_no_more() {
    let _turn = fresh_budget().await;
    let capacity = smtp_data_budget_capacity();
    let started = Arc::new(AtomicUsize::new(0));
    let mut gates = Vec::new();
    let mut all = Vec::new();
    for _ in 0..capacity + 2 {
        let (gate, opened) = oneshot::channel::<()>();
        gates.push(gate);
        let started = started.clone();
        all.push(tokio::spawn(with_smtp_data_budget(|| async move {
            started.fetch_add(1, Ordering::SeqCst);
            let _ = opened.await;
        })));
    }
    next_tick().await;

    assert_eq!(started.load(Ordering::SeqCst), capacity);
    assert_eq!(smtp_data_budget_in_flight(), capacity);

    release(gates, all).await;
    assert_eq!(started.load(Ordering::SeqCst), capacity + 2);
}

#[tokio::test]
async fn delays_a_transaction_past_capacity_rather_than_refusing_it() {
    let _turn = fresh_budget().await;
    let (gates, held) = saturate().await;

    let delivered = Arc::new(AtomicUsize::new(0));
    let queued = tokio::spawn(with_smtp_data_budget({
        let delivered = delivered.clone();
        || async move {
            delivered.fetch_add(1, Ordering::SeqCst);
        }
    }));
    next_tick().await;
    assert_eq!(delivered.load(Ordering::SeqCst), 0);

    release(gates, held).await;
    queued.await.unwrap().unwrap();

    assert_eq!(delivered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn releases_the_slot_when_the_transaction_resolves() {
    let _turn = fresh_budget().await;
    with_smtp_data_budget(|| async {}).await.unwrap();
    assert_eq!(smtp_data_budget_in_flight(), 0);
}

#[tokio::test]
async fn releases_the_slot_when_the_transaction_fails() {
    let _turn = fresh_budget().await;
    let outcome = with_smtp_data_budget(|| async { Err::<(), _>(anyhow::anyhow!("parse failed")) }).await;
    assert_eq!(outcome.unwrap().unwrap_err().to_string(), "parse failed");
    assert_eq!(smtp_data_budget_in_flight(), 0);
}

#[tokio::test]
async fn releases_the_slot_when_the_transaction_is_dropped_or_panics() {
    let _turn = fresh_budget().await;
    let abandoned = tokio::spawn(with_smtp_data_budget(std::future::pending::<()>));
    next_tick().await;
    assert_eq!(smtp_data_budget_in_flight(), 1);
    abandoned.abort();
    let _ = abandoned.await;
    assert_eq!(smtp_data_budget_in_flight(), 0);

    let panicked = tokio::spawn(with_smtp_data_budget(|| async { panic!("the save has a bug") }));
    assert!(panicked.await.is_err());
    assert_eq!(smtp_data_budget_in_flight(), 0);
}

#[tokio::test(start_paused = true)]
async fn refuses_with_a_transient_421_once_the_wait_ceiling_passes() {
    let _turn = fresh_budget().await;
    let (gates, held) = saturate().await;

    let refused = tokio::spawn(with_smtp_data_budget(|| async { "delivered" }));
    next_tick().await;
    tokio::time::advance(Duration::from_millis(smtp_data_budget_wait_ceiling_ms() + 1)).await;
    assert_eq!(refused.await.unwrap(), Err(DataBudgetUnavailableError));

    release(gates, held).await;
}

#[tokio::test(start_paused = true)]
async fn keeps_full_capacity_after_a_refusal() {
    let _turn = fresh_budget().await;
    let capacity = smtp_data_budget_capacity();
    let (gates, held) = saturate().await;

    let refused = tokio::spawn(with_smtp_data_budget(|| async {}));
    next_tick().await;
    tokio::time::advance(Duration::from_millis(smtp_data_budget_wait_ceiling_ms() + 1)).await;
    assert!(refused.await.unwrap().is_err());

    release(gates, held).await;

    // A refused waiter left on the queue would have taken one of these.
    assert_eq!(smtp_data_budget_in_flight(), 0);
    let started = Arc::new(AtomicUsize::new(0));
    for _ in 0..capacity {
        let started = started.clone();
        with_smtp_data_budget(|| async move {
            started.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();
    }
    assert_eq!(started.load(Ordering::SeqCst), capacity);
}

#[test]
fn the_refusal_answers_421_so_a_sending_mta_retries_instead_of_bouncing() {
    assert_eq!(DataBudgetUnavailableError.response_code(), 421);
    let refusal = SmtpHookError::from(DataBudgetUnavailableError);
    assert_eq!(refusal.response_code, Some(421));
    assert_eq!(refusal.message, "4.3.2 Error: too many concurrent deliveries, try again later");
}

#[tokio::test]
async fn the_default_capacity_is_two_and_the_wait_ceiling_twenty_seconds() {
    let _turn = fresh_budget().await;
    if std::env::var_os("SMTP_DATA_CONCURRENCY").is_none() {
        assert_eq!(smtp_data_budget_capacity(), 2);
    }
    assert_eq!(smtp_data_budget_wait_ceiling_ms(), 20_000);
}
