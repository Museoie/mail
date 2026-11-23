//! How many DATA transactions may materialize a message at once
//! (`smtp-data-budget.ts`).

use std::fmt;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::json;

use super::types::SmtpHookError;
use crate::concurrency_env::parse_concurrency_value;
use crate::fifo_semaphore::FifoSemaphore;
use crate::{env, logger};

/// Two, not one: the slot spans the whole transaction, and the save behind it
/// makes network calls — DNSBL lookups with a 2s timeout each, then push
/// notifications. At capacity 1 a single stalled lookup would hold the only
/// slot and serialize every other inbound message behind it.
const DEFAULT_CONCURRENCY: usize = 2;

/// How long a transaction will wait for a slot before being refused.
///
/// Under the SMTP server's 60s socket timeout, which is what the peer would hit
/// instead: a timeout drops the connection with nothing said, where the refusal
/// below names a transient condition the sending MTA knows to retry.
const WAIT_CEILING: Duration = Duration::from_secs(20);

/// The budget, sized from `SMTP_DATA_CONCURRENCY` the first time it is used.
fn semaphore() -> &'static FifoSemaphore {
    static SEMAPHORE: OnceLock<FifoSemaphore> = OnceLock::new();
    SEMAPHORE.get_or_init(|| {
        FifoSemaphore::new(parse_concurrency_value(
            env::var("SMTP_DATA_CONCURRENCY").as_deref(),
            "SMTP_DATA_CONCURRENCY",
            DEFAULT_CONCURRENCY,
            "smtp-data-budget",
        ))
    })
}

/// RFC 5321 §3.8 gives 421 to a service that is not available and is closing
/// the channel — the code a sending MTA queues and retries on, rather than
/// bouncing. Permanent codes are wrong here: the message is fine and the host
/// is only momentarily full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DataBudgetUnavailableError;

impl DataBudgetUnavailableError {
    pub const RESPONSE_CODE: u16 = 421;
    pub const MESSAGE: &'static str = "4.3.2 Error: too many concurrent deliveries, try again later";

    pub fn response_code(&self) -> u16 {
        Self::RESPONSE_CODE
    }
}

impl fmt::Display for DataBudgetUnavailableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::MESSAGE)
    }
}

impl std::error::Error for DataBudgetUnavailableError {}

impl From<DataBudgetUnavailableError> for SmtpHookError {
    fn from(_: DataBudgetUnavailableError) -> SmtpHookError {
        SmtpHookError::with_code(DataBudgetUnavailableError::RESPONSE_CODE, DataBudgetUnavailableError::MESSAGE)
    }
}

/// Returns the slot when the transaction settles, however it settles.
struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        semaphore().release();
    }
}

/// Runs `transaction` holding one of [`smtp_data_budget_capacity`] slots, so
/// the number of messages materializing at once is bounded independently of
/// `max_clients`.
///
/// Acquire before the DATA stream is read, not after. The SMTP server stops
/// reading its socket while the stream is not drained, so a transaction
/// waiting here holds one stream buffer rather than a whole decoded message —
/// which is the entire bound. Reading first and gating the save afterwards
/// would leave every byte already resident.
///
/// The slot is released when the transaction settles rather than when the
/// parse returns, because the decoded text and the attachment files stay in
/// use through the save.
///
/// Fails with [`DataBudgetUnavailableError`] when no slot frees up within the
/// wait ceiling; `transaction` is then never called.
pub async fn with_smtp_data_budget<T, F, Fut>(transaction: F) -> Result<T, DataBudgetUnavailableError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = T>,
{
    if semaphore().acquire_within(WAIT_CEILING).await.is_none() {
        logger::warn(
            "SMTP: DATA budget exhausted, refusing transaction",
            json!({ "capacity": smtp_data_budget_capacity(), "waitCeilingMs": smtp_data_budget_wait_ceiling_ms() }),
        );
        return Err(DataBudgetUnavailableError);
    }
    let _slot = Slot;
    Ok(transaction().await)
}

pub fn smtp_data_budget_capacity() -> usize {
    semaphore().capacity()
}

pub fn smtp_data_budget_in_flight() -> usize {
    semaphore().in_flight()
}

pub fn smtp_data_budget_wait_ceiling_ms() -> u64 {
    WAIT_CEILING.as_millis() as u64
}

/// Forgets every slot and waiter. For tests.
pub fn reset_smtp_data_budget() {
    semaphore().reset();
}
