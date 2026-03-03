//! Transaction orchestrator. See [`Sender`].

use crate::adapter::{
    ChainAdapter, ChainClient, FeeManager, PendingTransaction, ReplayProtection, RetryDecision,
    RetryStrategy, SendOutcome,
};
use crate::clock::{Clock, SystemClock};
use crate::{Error, Message, PriorityQueue};
use alloy::transports::RpcError::ErrorResp;
use log::{debug, error};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, Notify, Semaphore};

/// Maximum number of transactions that may be in-flight simultaneously.
const MAX_IN_FLIGHT_TRANSACTIONS: usize = 16;

/// JSON-RPC error code returned when the sender has insufficient funds.
const TX_FAILURE_INSUFFICIENT_FUNDS: i64 = -32003;

type SharedQueue = Arc<Mutex<PriorityQueue>>;

/// An orchestrator that manages concurrent transaction submission.
///
/// [`Sender`] is generic over a [`ChainAdapter`], which binds all chain-specific
/// components (client, fee manager, replay protection, retry strategy) together.
///
/// Callers add [`Message`] values via [`add_message`] and then drive
/// processing by calling [`run`], which blocks until an unrecoverable error
/// occurs.
///
/// [`add_message`]: Self::add_message
/// [`run`]: Self::run
pub struct Sender<A: ChainAdapter> {
    client: Arc<A::Client>,
    fees: Arc<A::FeeManager>,
    replay: Arc<A::ReplayProtection>,
    retry: Arc<A::RetryStrategy>,

    queue: SharedQueue,
    max_in_flight: Arc<Semaphore>,
    /// Notified by [`add_message`] so that [`process_next_message`] can sleep
    /// instead of spinning when the queue is empty.
    ///
    /// [`add_message`]: Self::add_message
    /// [`process_next_message`]: Self::process_next_message
    message_ready: Arc<Notify>,
}

impl<A: ChainAdapter> Clone for Sender<A> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            fees: self.fees.clone(),
            replay: self.replay.clone(),
            retry: self.retry.clone(),
            queue: self.queue.clone(),
            max_in_flight: self.max_in_flight.clone(),
            message_ready: self.message_ready.clone(),
        }
    }
}

impl<A: ChainAdapter> Sender<A> {
    /// Creates a new [`Sender`] with the given adapter components.
    pub fn new(
        client: Arc<A::Client>,
        fees: Arc<A::FeeManager>,
        replay: Arc<A::ReplayProtection>,
        retry: Arc<A::RetryStrategy>,
    ) -> Self {
        Self {
            client,
            fees,
            replay,
            retry,

            queue: Arc::new(Mutex::new(PriorityQueue::new())),
            max_in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TRANSACTIONS)),
            message_ready: Arc::new(Notify::new()),
        }
    }

    /// Enqueues `msg` for processing on the next available concurrency slot.
    pub async fn add_message(&self, msg: Message) {
        debug!("adding message {:?} {}", msg.to, msg.value);
        let now_ms = SystemClock.now_ms();
        self.queue.lock().await.push(msg, now_ms);
        self.message_ready.notify_one();
    }

    /// Drives the transaction send loop until an unrecoverable error occurs.
    ///
    /// [`run`] subscribes to new block/slot notifications and processes messages
    /// from the priority queue. On each new block the replay protection state
    /// is synced with the chain. When a concurrency slot is free, the highest-
    /// priority queued message is popped and processed in a spawned task.
    ///
    /// [`run`]: Self::run
    pub async fn run(&self) -> Result<(), Error> {
        let mut block_stream = self.client.subscribe_new_blocks().await?;

        loop {
            tokio::select! {
                biased;

                block = block_stream.recv() => {
                    match block {
                        Some(number) => {
                            debug!("Received new block notification {}", number);
                            self.replay.sync().await?;
                        }
                        None => {
                            return Err(Error::SubscriptionClosed);
                        }
                    }
                }

                _ = self.process_next_message() => {}
            }
        }
    }

    /// Acquires a concurrency slot, then pops and dispatches the next message.
    ///
    /// If the queue is empty, this method releases the slot and yields to the
    /// executor before returning, avoiding a tight busy-loop.
    async fn process_next_message(&self) {
        // Block until we have a slot available
        let permit = self.max_in_flight.clone().acquire_owned().await.unwrap();

        // Now see if there are any messages
        if let Some(msg) = self.queue.lock().await.pop() {
            // We have a message, process it in a separate task.
            let sender = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = sender.process_message(msg).await {
                    match &e {
                        Error::ChainError(crate::chain::Error::Rpc(ErrorResp(resp)))
                            if resp.code == TX_FAILURE_INSUFFICIENT_FUNDS =>
                        {
                            error!("Insufficient funds to send transaction; dropping message");
                        }
                        Error::ChainError(_) => {
                            error!("Error processing message: {:?}", e);
                        }
                        _ => {
                            error!("Error processing message: {:?}", e);
                        }
                    }
                }
            });
        } else {
            // No messages; release the permit and sleep until add_message wakes us.
            drop(permit);
            self.message_ready.notified().await;
        }
    }

    /// Validates and sends a single message, binding its replay token and fees
    /// immediately before submission.
    async fn process_message(&self, msg: Message) -> Result<(), Error> {
        debug!("processing message {:?} {}", msg.to, msg.value);

        let now_ms = SystemClock.now_ms();

        if msg.is_expired(now_ms) {
            return Err(Error::MessageExpired);
        }

        // Late-bind replay token (nonce / blockhash) and fees
        let replay_token = self.replay.next().await;
        let fee = match self
            .fees
            .get_fee_params(msg.effective_priority(now_ms))
            .await
        {
            Ok(fee) => fee,
            Err(err) => {
                self.replay.release(&replay_token).await;
                return Err(err);
            }
        };

        // Send the transaction
        self.send_and_watch(msg, fee, replay_token, 0).await
    }

    /// Sends a transaction and watches for confirmation, handling retries on drop.
    async fn send_and_watch(
        &self,
        mut msg: Message,
        mut fee: A::FeeParams,
        mut replay_token: A::ReplayToken,
        mut replacement_count: u32,
    ) -> Result<(), Error> {
        loop {
            let now_ms = SystemClock.now_ms();
            // Ensure the message is still valid
            if msg.is_expired(now_ms) {
                self.replay.release(&replay_token).await;
                return Err(Error::MessageExpired);
            }

            let created_at = Instant::now();

            // Send the transaction via the chain client.
            // Returns a SendOutcome indicating confirmation, drop, or revert.
            let outcome = match self
                .client
                .send_transaction(&msg, &fee, &replay_token)
                .await
            {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.replay.release(&replay_token).await;
                    return Err(err);
                }
            };

            match outcome {
                SendOutcome::Confirmed { tx_id } => {
                    // Transaction confirmed — update trackers
                    let pending = PendingTransaction {
                        msg,
                        tx_id,
                        fee,
                        replay_token,
                        created_at,
                        replacement_count,
                    };
                    let confirmation_time = pending.created_at.elapsed();
                    self.retry
                        .handle_confirmed(&pending, &self.fees, &self.replay, confirmation_time)
                        .await;
                    return Ok(());
                }
                SendOutcome::Dropped { tx_id } | SendOutcome::Reverted { tx_id } => {
                    // Transaction was dropped/timed out or reverted — delegate to retry
                    let pending = PendingTransaction {
                        msg: msg.clone(),
                        tx_id: tx_id.clone(),
                        fee: fee.clone(),
                        replay_token: replay_token.clone(),
                        created_at,
                        replacement_count,
                    };

                    if !msg.increment_retry() {
                        self.replay.release(&pending.replay_token).await;
                        error!(
                            "Message for transaction {:?} failed after max retries",
                            tx_id
                        );
                        return Ok(());
                    }

                    match self
                        .retry
                        .handle_dropped(&pending, &self.client, &self.fees, &self.replay)
                        .await
                    {
                        RetryDecision::Resubmit {
                            fee: new_fee,
                            replay_token: new_replay,
                        } => {
                            fee = new_fee;
                            replay_token = new_replay;
                            replacement_count = pending.replacement_count + 1;
                            continue;
                        }
                        RetryDecision::Requeue => {
                            self.replay.release(&pending.replay_token).await;
                            // Re-queue the message; it will get fresh fee params and replay
                            // token when it is next popped from the priority queue.
                            self.add_message(msg).await;
                            return Ok(());
                        }
                        RetryDecision::Abandon => {
                            self.replay.release(&pending.replay_token).await;
                            error!("Abandoning transaction {:?}", tx_id);
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Sender;
    use crate::adapter::ethereum::{
        Eth, EthClient, EthFeeManager, EthReplayProtection, EthRetryStrategy, bump_by_percent,
    };
    use crate::{Message, chain};
    use alloy::primitives::Address;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;

    // A minimal mock that satisfies NonceManager::new inside EthReplayProtection::new.
    fn base_mock() -> chain::MockChainClient {
        let mut mock = chain::MockChainClient::new();
        mock.expect_get_account_nonce().returning(|_| Ok(0u64));
        mock
    }

    async fn make_sender(mock: chain::MockChainClient) -> Sender<Eth> {
        let chain = Arc::new(mock);
        let client = Arc::new(EthClient::new(chain.clone()));
        let fees = Arc::new(EthFeeManager::new());
        let replay = Arc::new(
            EthReplayProtection::new(chain.clone(), Address::default())
                .await
                .unwrap(),
        );
        let retry = Arc::new(EthRetryStrategy::new());
        Sender::new(client, fees, replay, retry)
    }

    // ── bump_by_percent ──────────────────────────────────────────────────────

    #[test]
    fn test_bump_zero_base() {
        assert_eq!(bump_by_percent(0, 20), 0);
    }

    #[test]
    fn test_bump_twenty_percent() {
        assert_eq!(bump_by_percent(100, 20), 120);
    }

    #[test]
    fn test_bump_zero_percent() {
        assert_eq!(bump_by_percent(500, 0), 500);
    }

    #[test]
    fn test_bump_is_an_increase_not_a_fraction() {
        let fee: u128 = 1_000_000_000;
        let bumped = bump_by_percent(fee, 20);
        assert!(bumped > fee, "bumped fee must exceed the original");
        assert_eq!(bumped, 1_200_000_000);
    }

    // ── Sender::new / add_message ────────────────────────────────────────────

    #[tokio::test]
    async fn test_new_has_empty_queue() {
        let sender = make_sender(base_mock()).await;
        assert_eq!(sender.queue.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn test_add_message_enqueues() {
        let sender = make_sender(base_mock()).await;
        sender.add_message(Message::default()).await;
        assert_eq!(sender.queue.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn test_add_multiple_messages() {
        let sender = make_sender(base_mock()).await;
        for _ in 0..4 {
            sender.add_message(Message::default()).await;
        }
        assert_eq!(sender.queue.lock().await.len(), 4);
    }

    // ── run() ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_run_errors_when_block_stream_closes() {
        let mut mock = base_mock();
        // subscribe_new_blocks needs to be mocked on the inner chain client
        // but our EthClient wraps it. We need to set up the mock to handle
        // subscribe_new_blocks returning a stream that closes immediately.
        mock.expect_subscribe_new_blocks().returning(|| {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        });

        let sender = make_sender(mock).await;
        let result = sender.run().await;
        assert!(
            result.is_err(),
            "run() must return Err when the block stream closes"
        );
    }

    // ── process_next_message() ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_process_next_message_blocks_when_queue_empty() {
        let sender = Arc::new(make_sender(base_mock()).await);
        let s = sender.clone();
        let handle = tokio::spawn(async move { s.process_next_message().await });

        tokio::task::yield_now().await;
        assert!(!handle.is_finished(), "should block when queue is empty");

        sender.message_ready.notify_one();
        tokio::time::timeout(Duration::from_millis(100), handle)
            .await
            .expect("process_next_message did not unblock after notify")
            .expect("task panicked");
    }

    #[tokio::test]
    async fn test_process_next_message_pops_and_attempts_send() {
        let mut mock = base_mock();
        mock.expect_id().returning(|| 1337u64);
        mock.expect_send_transaction()
            .returning(|_| Err(chain::Error::Subscription("injected".into())));

        let sender = make_sender(mock).await;
        let mut msg = Message::default();
        msg.to = Some(Address::default());
        msg.gas = 21_000;
        sender.add_message(msg).await;
        assert_eq!(sender.queue.lock().await.len(), 1);

        sender.process_next_message().await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(sender.queue.lock().await.len(), 0);
    }
}
