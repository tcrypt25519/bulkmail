//! Transaction orchestrator. See [`Sender`].

use crate::{
    adapter::{
        ChainAdapter, ChainClient, FeeManager, PendingTransaction, ReplayProtection, RetryDecision,
        RetryStrategy, SendOutcome,
    },
    Error, Message, PriorityQueue,
};
use alloy::transports::RpcError::ErrorResp;
use log::{debug, error, warn};
use std::{collections::HashMap, sync::Arc, time::Instant};
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
    pending: Arc<Mutex<HashMap<A::TxId, PendingTransaction<A>>>>,
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
            pending: self.pending.clone(),
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
            pending: Arc::new(Mutex::new(HashMap::new())),
            max_in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_TRANSACTIONS)),
            message_ready: Arc::new(Notify::new()),
        }
    }

    /// Enqueues `msg` for processing on the next available concurrency slot.
    pub async fn add_message(&self, msg: Message) {
        debug!("adding message {:?} {}", msg.to, msg.value);
        self.queue.lock().await.push(msg);
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
                        Error::ChainError(chain_err) => {
                            if let crate::chain::Error::Rpc(ErrorResp(resp)) = chain_err {
                                if resp.code == TX_FAILURE_INSUFFICIENT_FUNDS {
                                    error!(
                                        "Insufficient funds to send transaction; dropping message"
                                    );
                                    return;
                                }
                            }
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

        if msg.is_expired() {
            return Err(Error::MessageExpired);
        }

        // Late-bind replay token (nonce / blockhash) and fees
        let replay_token = self.replay.next().await;
        let fee = self.fees.get_fee_params(msg.effective_priority()).await?;

        // Send the transaction
        self.send_and_watch(msg, fee, replay_token, 0).await
    }

    /// Sends a transaction and watches for confirmation, handling retries on drop.
    async fn send_and_watch(
        &self,
        msg: Message,
        fee: A::FeeParams,
        replay_token: A::ReplayToken,
        replacement_count: u32,
    ) -> Result<(), Error> {
        // Ensure the message is still valid
        if msg.is_expired() {
            return Err(Error::MessageExpired);
        }

        let created_at = Instant::now();

        // Send the transaction via the chain client.
        // Returns a SendOutcome indicating confirmation, drop, or revert.
        match self
            .client
            .send_transaction(&msg, &fee, &replay_token)
            .await?
        {
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
                Ok(())
            }
            SendOutcome::Dropped { tx_id } | SendOutcome::Reverted { tx_id } => {
                // Transaction was dropped/timed out or reverted — delegate to retry
                let pending = PendingTransaction {
                    msg,
                    tx_id: tx_id.clone(),
                    fee,
                    replay_token,
                    created_at,
                    replacement_count,
                };
                self.pending.lock().await.insert(tx_id.clone(), pending);
                self.handle_dropped(&tx_id).await;
                Ok(())
            }
        }
    }

    /// Handles a dropped/timed-out transaction by delegating to the retry strategy.
    async fn handle_dropped(&self, tx_id: &A::TxId) {
        let pending = {
            let mut map = self.pending.lock().await;
            match map.remove(tx_id) {
                Some(p) => p,
                None => {
                    warn!("Transaction {:?} not found in pending map", tx_id);
                    return;
                }
            }
        };

        let mut msg = pending.msg.clone();
        if !msg.increment_retry() {
            error!(
                "Message for transaction {:?} failed after max retries",
                tx_id
            );
            return;
        }

        match self
            .retry
            .handle_dropped(&pending, &self.client, &self.fees, &self.replay)
            .await
        {
            RetryDecision::Resubmit { .. } | RetryDecision::Requeue => {
                // Re-queue the message; it will get fresh fee params and replay
                // token when it is next popped from the priority queue.
                self.add_message(msg).await;
            }
            RetryDecision::Abandon => {
                error!("Abandoning transaction {:?}", tx_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Sender;
    use crate::{
        adapter::ethereum::{
            bump_by_percent, Eth, EthClient, EthFeeManager, EthReplayProtection, EthRetryStrategy,
        },
        chain, Message,
    };
    use alloy::primitives::Address;
    use std::{sync::Arc, time::Duration};
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
